//! A user-namespace mount sandbox for Linux (roadmap 2.2; design note
//! `docs/design/landlock-metadata-and-name-hiding.md`).
//!
//! Landlock cannot confine a file's metadata (`chmod`, `chown`, `utime`,
//! xattrs) and cannot hide whether a name exists. Bubblewrap closes both with a
//! mount namespace; this does the part bwn needs, in-process, when the
//! `bwrap` binary is absent: a new user and mount namespace in which the whole
//! tree is read-only, the workspace, added folders, `/tmp` and `/dev` are
//! read-write again, `.git` and `.buildwithnexus` stay read-only, and each
//! credential store directory is covered by an empty read-only tmpfs. The
//! Landlock ruleset and seccomp filter are applied on top, as before.
//!
//! The user is mapped to itself, not to root, and every capability is dropped
//! before `exec`, so the command cannot unmount or remount anything; seccomp
//! refuses the mount syscalls as a second line.
//!
//! Everything that allocates happens in [`Plan::new`], before `fork`; the
//! child runs only raw syscalls in [`Plan::enter`].

use std::ffi::CString;
use std::io;
use std::path::{Path, PathBuf};

const MS_RDONLY: libc::c_ulong = 1;
const MS_NOSUID: libc::c_ulong = 2;
const MS_NODEV: libc::c_ulong = 4;
const MS_BIND: libc::c_ulong = 4096;
const MS_REC: libc::c_ulong = 16384;
const MS_PRIVATE: libc::c_ulong = 1 << 18;

const AT_RECURSIVE: libc::c_uint = 0x8000;
const MOUNT_ATTR_RDONLY: u64 = 1;
const SYS_MOUNT_SETATTR: libc::c_long = 442;

#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

fn cstr(p: &Path) -> Option<CString> {
    use std::os::unix::ffi::OsStrExt;
    CString::new(p.as_os_str().as_bytes()).ok()
}

/// What the child will do, computed before `fork`.
pub struct Plan {
    uid_map: CString,
    gid_map: CString,
    cwd: CString,
    /// Mounted read-write again (bind + clear read-only).
    rw: Vec<CString>,
    /// Existing paths inside a read-write root kept read-only.
    ro: Vec<CString>,
    /// Directories covered by an empty read-only tmpfs.
    cover: Vec<CString>,
    /// The session's user and network namespaces to join (netns.rs),
    /// instead of making a new user namespace.
    join: Option<(libc::c_int, libc::c_int)>,
}

impl Plan {
    /// `write_roots` are the folders a command may write (workspace, added
    /// folders, temp, `/dev`); `protected` are paths inside them that stay
    /// read-only (a missing one is created first, as bubblewrap does, so a
    /// command cannot create it); `hidden` are credential stores. A hidden
    /// file (not a directory) is left to Landlock: covering it would still
    /// show that it exists.
    pub fn new(
        cwd: &Path,
        write_roots: &[PathBuf],
        protected: &[PathBuf],
        hidden: &[PathBuf],
    ) -> Option<Plan> {
        // SAFETY: getters with no arguments.
        let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
        let rw = write_roots
            .iter()
            .filter(|p| p.is_dir())
            .filter_map(|p| cstr(p))
            .collect();
        let mut ro = Vec::new();
        let mut cover = Vec::new();
        for p in protected {
            match p.symlink_metadata() {
                Ok(_) => ro.extend(cstr(p)),
                Err(_) => {
                    if std::fs::create_dir(p).is_ok() {
                        cover.extend(cstr(p));
                    }
                }
            }
        }
        for p in hidden {
            if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                cover.extend(cstr(p));
            }
        }
        Some(Plan {
            uid_map: CString::new(format!("{uid} {uid} 1")).ok()?,
            gid_map: CString::new(format!("{gid} {gid} 1")).ok()?,
            cwd: cstr(cwd)?,
            rw,
            ro,
            cover,
            join: None,
        })
    }

    /// Join these user and network namespace descriptors (which must stay
    /// open until the command has been spawned) instead of making a user
    /// namespace.
    pub fn join(&mut self, fds: (libc::c_int, libc::c_int)) {
        self.join = Some(fds);
    }

    /// In the child, after `fork` and before `exec`: enter the namespaces,
    /// set up the mounts, return to the workspace and drop every capability.
    /// Async-signal-safe: raw syscalls only, nothing allocated.
    pub fn enter(&self) -> io::Result<()> {
        // SAFETY: each call below passes valid, NUL-terminated buffers that
        // live in `self` (or are string literals) for the whole call.
        unsafe {
            if let Some((user, net)) = self.join {
                // The session's namespaces: its user mapping is already
                // written, and its network holds only loopback and the
                // checker. The mount namespace is the command's own.
                check(libc::setns(user, libc::CLONE_NEWUSER))?;
                check(libc::setns(net, libc::CLONE_NEWNET))?;
                check(libc::unshare(libc::CLONE_NEWNS))?;
            } else {
                check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
                write_file(c"/proc/self/setgroups", c"deny")?;
                write_file(c"/proc/self/uid_map", &self.uid_map)?;
                write_file(c"/proc/self/gid_map", &self.gid_map)?;
            }
            // Keep the host's mount events out of our copy and ours out of
            // the host's.
            check(libc::mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                MS_REC | MS_PRIVATE,
                std::ptr::null(),
            ))?;
            setattr(c"/", MOUNT_ATTR_RDONLY, 0)?;
            for p in &self.rw {
                bind(p)?;
                setattr(p, 0, MOUNT_ATTR_RDONLY)?;
            }
            for p in &self.ro {
                bind(p)?;
                setattr(p, MOUNT_ATTR_RDONLY, 0)?;
            }
            for p in &self.cover {
                check(libc::mount(
                    c"tmpfs".as_ptr(),
                    p.as_ptr(),
                    c"tmpfs".as_ptr(),
                    MS_RDONLY | MS_NOSUID | MS_NODEV,
                    c"size=4k,mode=0555".as_ptr().cast(),
                ))?;
            }
            // The current directory was set before this ran, on the mount
            // that is now read-only; move onto the read-write bind.
            check(libc::chdir(self.cwd.as_ptr()))?;
            drop_capabilities()?;
        }
        Ok(())
    }
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

unsafe fn write_file(path: &std::ffi::CStr, data: &std::ffi::CStr) -> io::Result<()> {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = data.to_bytes();
    let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
    let err = io::Error::last_os_error();
    libc::close(fd);
    if n != bytes.len() as isize {
        return Err(err);
    }
    Ok(())
}

unsafe fn bind(p: &std::ffi::CStr) -> io::Result<()> {
    check(libc::mount(
        p.as_ptr(),
        p.as_ptr(),
        std::ptr::null(),
        MS_BIND | MS_REC,
        std::ptr::null(),
    ))
}

unsafe fn setattr(p: &std::ffi::CStr, set: u64, clr: u64) -> io::Result<()> {
    let attr = MountAttr {
        attr_set: set,
        attr_clr: clr,
        propagation: 0,
        userns_fd: 0,
    };
    let rc = libc::syscall(
        SYS_MOUNT_SETATTR,
        libc::AT_FDCWD,
        p.as_ptr(),
        AT_RECURSIVE,
        &attr as *const MountAttr,
        std::mem::size_of::<MountAttr>(),
    );
    check(rc as libc::c_int)
}

// Drop the bounding set and clear every capability set, so `exec` leaves the
// command with none even when it runs as uid 0 (where an identity map keeps
// root's privileges inside the namespace).
unsafe fn drop_capabilities() -> io::Result<()> {
    for cap in 0..64 {
        // EINVAL past the last capability the kernel knows; ignore it.
        libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
    }
    libc::prctl(
        libc::PR_CAP_AMBIENT,
        libc::PR_CAP_AMBIENT_CLEAR_ALL,
        0,
        0,
        0,
    );
    #[repr(C)]
    struct Header {
        version: u32,
        pid: libc::c_int,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x2008_0522, // _LINUX_CAPABILITY_VERSION_3
        pid: 0,
    };
    let data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    let rc = libc::syscall(libc::SYS_capset, &header as *const Header, data.as_ptr());
    check(rc as libc::c_int)
}

/// Whether unprivileged user and mount namespaces work here, probed once in a
/// forked child that does what `enter` would do to the root and exits. Ubuntu
/// 24.04's AppArmor restriction, `kernel.unprivileged_userns_clone=0` and
/// `user.max_user_namespaces=0` all make it fail.
pub fn available() -> Result<(), String> {
    static RESULT: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    RESULT
        .get_or_init(|| {
            let Some(plan) = Plan::new(Path::new("/"), &[], &[], &[]) else {
                return Err("cannot build the namespace plan".into());
            };
            // SAFETY: the child only makes raw syscalls (Plan::enter) and
            // exits with _exit; the parent waits for it.
            unsafe {
                let pid = libc::fork();
                if pid < 0 {
                    return Err(format!("fork: {}", io::Error::last_os_error()));
                }
                if pid == 0 {
                    let code = match plan.enter() {
                        Ok(()) => 0,
                        Err(e) => e.raw_os_error().unwrap_or(1).clamp(1, 255),
                    };
                    libc::_exit(code);
                }
                let mut status = 0;
                libc::waitpid(pid, &mut status, 0);
                if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    Ok(())
                } else {
                    let code = libc::WEXITSTATUS(status);
                    Err(format!(
                        "user namespaces are unavailable here ({})",
                        io::Error::from_raw_os_error(code)
                    ))
                }
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Stdio;

    // A workspace under a fake home with a credential store, an outside file
    // and a .git, run through the namespace backend (Landlock + seccomp on
    // top, as in production). Returns None where the backend can't run.
    struct Cell {
        home: PathBuf,
        ws: PathBuf,
        outside: PathBuf,
    }

    impl Cell {
        fn new(tag: &str) -> Option<Cell> {
            let usable = crate::landlock::available().and_then(|()| super::available());
            if let Err(e) = usable {
                // The CI cell for this backend must not pass by skipping.
                assert!(
                    std::env::var("BWN_TEST_SANDBOX_BACKEND").as_deref() != Ok("userns"),
                    "the userns backend was forced but is unavailable: {e}"
                );
                return None;
            }
            let base = std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|h| h.is_dir() && !h.starts_with("/tmp"))
                .unwrap_or_else(|| PathBuf::from("/var/tmp"));
            let home = base.join(format!("bwn-userns-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            std::fs::create_dir_all(home.join(".ssh")).unwrap();
            std::fs::create_dir_all(home.join("proj/.git/hooks")).unwrap();
            std::fs::create_dir_all(home.join("proj/vendor/lib/.git/hooks")).unwrap();
            std::fs::write(home.join(".ssh/id_test"), "KEY-MATERIAL\n").unwrap();
            std::fs::write(home.join("outside.txt"), "o\n").unwrap();
            let home = home.canonicalize().unwrap();
            let ws = home.join("proj");
            let outside = home.join("outside.txt");
            std::fs::set_permissions(
                &outside,
                std::os::unix::fs::PermissionsExt::from_mode(0o644),
            )
            .unwrap();
            Some(Cell { home, ws, outside })
        }

        fn run(&self, script: &str) -> (bool, String) {
            let hidden = vec![self.home.join(".ssh")];
            let out = crate::landlock::command_with_hidden_for_test(
                script,
                &self.ws,
                &hidden,
                crate::landlock::Net::Off,
            )
            .expect("command")
            .stdin(Stdio::null())
            .output()
            .expect("spawn");
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), text)
        }

        fn run_net(&self, script: &str, net: crate::landlock::Net) -> (bool, String) {
            let hidden = vec![self.home.join(".ssh")];
            let out = crate::landlock::command_with_hidden_for_test(script, &self.ws, &hidden, net)
                .expect("command")
                .stdin(Stdio::null())
                .output()
                .expect("spawn");
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            (out.status.success(), text)
        }

        fn mode(&self, p: &Path) -> u32 {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(p).unwrap().permissions().mode() & 0o777
        }
    }

    impl Drop for Cell {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    // Roadmap 2.2: Landlock leaves a file's metadata outside the workspace
    // writable, the fd form included (open read-only, then fchmod). In the
    // namespace the tree is read-only, so both get EROFS.
    #[test]
    fn invariant_userns_confines_metadata_outside() {
        let Some(c) = Cell::new("meta") else { return };
        let o = c.outside.display();
        let (_, said) = c.run(&format!(
            "chmod 600 {o}; python3 -c 'import os,sys; fd=os.open(sys.argv[1], os.O_RDONLY); os.fchmod(fd, 0o600)' {o}; echo end"
        ));
        assert!(said.contains("end"), "probe did not run: {said}");
        assert_eq!(
            c.mode(&c.outside),
            0o644,
            "metadata changed outside: {said}"
        );
    }

    // Roadmap 2.2: under Landlock a guessed name in a hidden store answers
    // EACCES (exists) or ENOENT (absent). In the namespace the store is an
    // empty tmpfs, so both answer ENOENT and the name can't be confirmed.
    #[test]
    fn invariant_userns_hides_store_names() {
        let Some(c) = Cell::new("names") else { return };
        let ssh = c.home.join(".ssh");
        let (_, said) = c.run(&format!(
            "stat {0}/id_test >/dev/null 2>&1 && echo FOUND-exists; stat {0}/nope >/dev/null 2>&1 && echo FOUND-absent; cat {0}/id_test 2>&1; echo end",
            ssh.display()
        ));
        assert!(said.contains("end"), "probe did not run: {said}");
        assert!(
            !said.contains("FOUND-exists"),
            "a hidden name was confirmed: {said}"
        );
        assert!(!said.contains("KEY-MATERIAL"), "the key was read: {said}");
        assert!(
            said.contains("No such file"),
            "expected ENOENT for the hidden name: {said}"
        );
    }

    // The ordinary work still runs: writes in the workspace (relative to the
    // current directory, which moves onto the read-write bind), /tmp and
    // /dev/null; .git stays read-only.
    #[test]
    fn invariant_userns_keeps_the_workspace_writable_and_git_read_only() {
        let Some(c) = Cell::new("rw") else { return };
        let (_, said) = c.run(
            "echo in > rel.txt && echo REL-OK; echo t > /tmp/bwn-userns-probe-$$ && rm /tmp/bwn-userns-probe-$$ && echo TMP-OK; echo x > /dev/null && echo DEV-OK; touch .git/hooks/pre-commit 2>/dev/null && echo GIT-WROTE; touch vendor/lib/.git/hooks/post-checkout 2>/dev/null && echo NESTED-WROTE; mkdir .buildwithnexus/x 2>/dev/null && echo BWN-WROTE; echo end",
        );
        assert!(said.contains("REL-OK"), "workspace write failed: {said}");
        assert!(said.contains("TMP-OK"), "/tmp write failed: {said}");
        assert!(said.contains("DEV-OK"), "/dev/null write failed: {said}");
        assert!(!said.contains("GIT-WROTE"), ".git was writable: {said}");
        assert!(
            !said.contains("NESTED-WROTE"),
            "a nested .git was writable: {said}"
        );
        assert!(
            !said.contains("BWN-WROTE"),
            ".buildwithnexus could be created: {said}"
        );
        assert_eq!(
            std::fs::read_to_string(c.ws.join("rel.txt")).unwrap(),
            "in\n"
        );
    }

    // The command must not undo the mounts: no capabilities after exec (even
    // when bwn runs as root), and the mount syscalls are refused.
    #[test]
    fn invariant_userns_command_cannot_undo_the_mounts() {
        let Some(c) = Cell::new("caps") else { return };
        let ssh = c.home.join(".ssh");
        let (_, said) = c.run(&format!(
            "grep CapEff /proc/self/status; umount {0} 2>/dev/null && echo UMOUNTED; mount -o remount,rw / 2>/dev/null && echo REMOUNTED; cat {0}/id_test 2>/dev/null; echo end",
            ssh.display()
        ));
        assert!(said.contains("end"), "probe did not run: {said}");
        assert!(
            said.contains("CapEff:\t0000000000000000"),
            "the command kept capabilities: {said}"
        );
        assert!(
            !said.contains("UMOUNTED"),
            "a store cover was unmounted: {said}"
        );
        assert!(
            !said.contains("REMOUNTED"),
            "/ was remounted read-write: {said}"
        );
        assert!(!said.contains("KEY-MATERIAL"), "the key was read: {said}");
    }

    // Under the checker, a command in the namespace reaches only the
    // checker's port over TCP: no other port, no UDP, and no Unix socket
    // (so not the SSH agent either).
    #[test]
    fn invariant_userns_limits_the_network_to_the_proxy() {
        let Some(c) = Cell::new("net") else { return };
        if !crate::landlock::limits_connections() {
            eprintln!("skip: Landlock ABI 4 is needed to limit connections");
            return;
        }
        let proxy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let other = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let (pp, op) = (
            proxy.local_addr().unwrap().port(),
            other.local_addr().unwrap().port(),
        );
        let out = crate::landlock::command_with_hidden_for_test(
            &format!("{} {pp} {op}", crate::sandbox::tests::NET_PROBE),
            &c.ws,
            &[c.home.join(".ssh")],
            crate::landlock::Net::Proxy(pp),
        )
        .expect("command")
        .stdin(Stdio::null())
        .output()
        .expect("spawn");
        crate::sandbox::tests::assert_proxy_only(&out);
    }

    // What a command in the session's network namespace can reach, checked
    // by python3 inside it. argv: a host port, this machine's own non-
    // loopback address (or "-"), the checker's port inside.
    const ESCAPE_PROBE: &str = r#"python3 -c '
import socket, sys, errno
def why(e):
    return errno.errorcode.get(e.errno, type(e).__name__)
def tcp(host, port, fam=socket.AF_INET):
    try:
        s = socket.socket(fam, socket.SOCK_STREAM)
        s.settimeout(2)
        s.connect((host, port)); return "ok"
    except OSError as e:
        return why(e)
def mk(fam, typ, proto=0):
    try:
        socket.socket(fam, typ, proto); return "ok"
    except OSError as e:
        return why(e)
hp, outer, cp = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
print("checker", tcp("127.0.0.1", cp))
print("host_loopback", tcp("127.0.0.1", hp))
print("host_loopback_v6", tcp("::1", hp, socket.AF_INET6))
if outer != "-":
    print("outer_checker_port", tcp(outer, cp))
    print("outer_host_port", tcp(outer, hp))
print("public_v4", tcp("1.1.1.1", 443))
print("public_v6", tcp("2606:4700:4700::1111", 443, socket.AF_INET6))
print("udp", mk(socket.AF_INET, socket.SOCK_DGRAM))
print("udp_v6", mk(socket.AF_INET6, socket.SOCK_DGRAM))
print("raw", mk(socket.AF_INET, socket.SOCK_RAW, socket.IPPROTO_ICMP))
print("netlink", mk(socket.AF_NETLINK, socket.SOCK_RAW, 0))
print("unix", mk(socket.AF_UNIX, socket.SOCK_STREAM))
print("packet", mk(socket.AF_PACKET, socket.SOCK_RAW))
try:
    socket.getaddrinfo("example.com", 443); print("dns ok")
except OSError as e:
    print("dns", type(e).__name__)
' "#;

    // This machine's address on its default route, if it has one.
    fn outer_ip() -> Option<std::net::IpAddr> {
        let u = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
        u.connect(("192.0.2.1", 9)).ok()?;
        let ip = u.local_addr().ok()?.ip();
        (!ip.is_loopback() && !ip.is_unspecified()).then_some(ip)
    }

    fn lines(out: &str) -> std::collections::HashMap<String, String> {
        out.lines()
            .filter_map(|l| l.split_once(' '))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // Roadmap 2.2, SEC-2: in the session's private network namespace a
    // command reaches the checker and nothing else. Not the host's
    // loopback, not this machine's own address on the checker's port (which
    // Landlock's port rule let through on the host network), not the
    // internet directly, no UDP or DNS, no raw, netlink, packet or Unix
    // sockets.
    #[test]
    fn invariant_netns_command_reaches_only_the_checker() {
        let Some(c) = Cell::new("netns-escape") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        // A host service on every address, and one on the checker's port
        // number (the SEC-2 shape: Landlock's port rule let a command reach
        // the checker's port on any address).
        let host = std::net::TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let hp = host.local_addr().unwrap().port();
        let _same_port = std::net::TcpListener::bind(("0.0.0.0", s.port)).ok();
        let outer = outer_ip();
        if let Some(ip) = outer {
            // The host itself reaches it there, so a refusal inside means
            // something.
            assert!(std::net::TcpStream::connect((ip, hp)).is_ok());
        }
        let outer_arg = outer.map_or("-".to_string(), |ip| ip.to_string());
        let (_, out) = c.run_net(
            &format!("{ESCAPE_PROBE} {hp} {outer_arg} {}", s.port),
            crate::landlock::Net::Private(s.port),
        );
        let got = lines(&out);
        assert_eq!(got.get("checker").map(String::as_str), Some("ok"), "{out}");
        for (k, v) in &got {
            if k != "checker" {
                assert_ne!(v, "ok", "{k} was reachable from the namespace: {out}");
            }
        }
        for k in [
            "host_loopback",
            "public_v4",
            "udp",
            "raw",
            "netlink",
            "unix",
            "packet",
            "dns",
        ] {
            assert!(got.contains_key(k), "probe line {k} missing: {out}");
        }
        if outer.is_some() {
            assert!(got.contains_key("outer_checker_port"), "{out}");
        }
    }

    // One namespace per session: a server one command starts answers the
    // next command, and bwn's own tools, but not the host.
    #[test]
    fn invariant_netns_servers_persist_and_only_the_session_reaches_them() {
        use std::io::Read;
        let Some(c) = Cell::new("netns-persist") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        std::fs::write(
            c.ws.join("srv.py"),
            "import socket, os\n\
             s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(8)\n\
             pid = os.fork()\n\
             if pid:\n    print(s.getsockname()[1], pid, flush=True); os._exit(0)\n\
             os.setsid(); fd = os.open(os.devnull, os.O_RDWR)\n\
             for i in (0, 1, 2): os.dup2(fd, i)\n\
             while True:\n    k, _ = s.accept(); k.sendall(b'HELLO'); k.close()\n",
        )
        .unwrap();
        let net = crate::landlock::Net::Private(s.port);
        let (ok, out) = c.run_net("python3 srv.py", net);
        assert!(ok, "{out}");
        let mut it = out.split_whitespace();
        let port: u16 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let pid: i32 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let read = |mut k: std::net::TcpStream| {
            let mut b = String::new();
            let _ = k.read_to_string(&mut b);
            b
        };
        // The next command.
        let (_, next) = c.run_net(
            &format!(
                "python3 -c 'import socket; k = socket.create_connection((\"127.0.0.1\", {port})); print(k.recv(5).decode())'"
            ),
            net,
        );
        // Through the checker from inside: dialled in the namespace.
        let (_, via) = c.run_net(
            &format!(
                "python3 -c 'import socket; k = socket.create_connection((\"127.0.0.1\", {})); k.sendall(b\"CONNECT 127.0.0.1:{port} HTTP/1.1\\r\\n\\r\\n\"); import time; time.sleep(0.5); print(k.recv(200))'",
                s.port
            ),
            net,
        );
        // bwn's dialer, and the host.
        let dialled = s
            .dial(([127, 0, 0, 1], port).into())
            .map(read)
            .unwrap_or_else(|e| e.to_string());
        let from_host = std::net::TcpStream::connect(("127.0.0.1", port))
            .map(read)
            .unwrap_or_default();
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert!(next.contains("HELLO"), "the next command: {next}");
        assert!(
            via.contains("200") && via.contains("HELLO"),
            "through the checker: {via}"
        );
        assert_eq!(dialled, "HELLO", "bwn's dialer");
        assert!(
            !from_host.contains("HELLO"),
            "the host reached the namespace's server"
        );
    }

    // A request through the checker for a loopback address is dialled in
    // the namespace, so it can't reach a service on the host's loopback.
    #[test]
    fn invariant_netns_checker_does_not_reach_host_loopback() {
        let Some(c) = Cell::new("netns-checker") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        let host = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let hp = host.local_addr().unwrap().port();
        let (_, via) = c.run_net(
            &format!(
                "python3 -c 'import socket; k = socket.create_connection((\"127.0.0.1\", {})); k.sendall(b\"CONNECT 127.0.0.1:{hp} HTTP/1.1\\r\\n\\r\\n\"); print(k.recv(200))'",
                s.port
            ),
            crate::landlock::Net::Private(s.port),
        );
        assert!(
            via.contains("502"),
            "the checker reached the host's loopback: {via}"
        );
    }

    // The namespace outlives its holder: kill the holder and the next
    // command still has no way out but the checker (bwn keeps the
    // namespace and the listener), and bwn's dialer comes back with a new
    // holder that joins the same namespace.
    #[test]
    fn invariant_netns_holds_when_its_holder_dies() {
        let Some(c) = Cell::new("netns-holder") else {
            return;
        };
        let s = crate::netns::start_for_test();
        let old = s.holder_pid();
        unsafe { libc::kill(old, libc::SIGKILL) };
        std::thread::sleep(std::time::Duration::from_millis(200));
        let host = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let hp = host.local_addr().unwrap().port();
        crate::netns::use_for_test(Some(&s));
        let (_, out) = c.run_net(
            &format!("{ESCAPE_PROBE} {hp} - {}", s.port),
            crate::landlock::Net::Private(s.port),
        );
        crate::netns::use_for_test(None);
        let got = lines(&out);
        assert_eq!(got.get("checker").map(String::as_str), Some("ok"), "{out}");
        assert_ne!(
            got.get("host_loopback").map(String::as_str),
            Some("ok"),
            "{out}"
        );
        assert_ne!(
            got.get("public_v4").map(String::as_str),
            Some("ok"),
            "{out}"
        );
        // Refused inside (nothing there), and the checker answers inside.
        assert!(s.dial(([127, 0, 0, 1], hp).into()).is_err());
        assert!(s.dial(([127, 0, 0, 1], s.port).into()).is_ok());
        assert_ne!(s.holder_pid(), old, "the holder was not replaced");
    }

    // A command can't join the host's network namespace (bwn's own), and
    // can't read the holder, a copy of bwn's memory in its user namespace.
    // Two layers refuse the read: the holder is not dumpable, and Landlock
    // refuses a sandboxed process ptrace access to one outside its domain.
    #[test]
    fn invariant_netns_command_cannot_join_the_host_or_read_the_holder() {
        let Some(c) = Cell::new("netns-join") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        let script = format!(
            "python3 -c '\n\
import ctypes, os\n\
libc = ctypes.CDLL(None, use_errno=True)\n\
try:\n    fd = os.open(\"/proc/{bwn}/ns/net\", os.O_RDONLY)\n    print(\"setns\", libc.setns(fd, 0), ctypes.get_errno())\n\
except OSError as e:\n    print(\"open-ns\", e.errno)\n\
for f in (\"environ\", \"mem\", \"maps\"):\n    try:\n        open(\"/proc/{holder}/\" + f, \"rb\").read(16); print(f, \"READ\")\n    except OSError as e:\n        print(f, e.errno)\n\
'",
            bwn = std::process::id(),
            holder = s.holder_pid()
        );
        let (_, out) = c.run_net(&script, crate::landlock::Net::Private(s.port));
        assert!(!out.contains("READ"), "read the holder: {out}");
        assert!(
            !out.contains("setns 0 "),
            "joined the host's namespace: {out}"
        );
        assert!(out.contains("environ"), "{out}");
    }

    // bwn's own tools (fetch, wait_for_url, screenshot_url's check) reach a
    // dev server a command started in the namespace; a client for model
    // providers does not, so a command can't stand in for a local model
    // server.
    #[test]
    fn invariant_netns_tools_reach_session_servers_and_providers_do_not() {
        let Some(c) = Cell::new("netns-tools") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        std::fs::write(
            c.ws.join("web.py"),
            "import socket, os\n\
             s = socket.socket(); s.bind(('127.0.0.1', 0)); s.listen(8)\n\
             pid = os.fork()\n\
             if pid:\n    print(s.getsockname()[1], pid, flush=True); os._exit(0)\n\
             os.setsid(); fd = os.open(os.devnull, os.O_RDWR)\n\
             for i in (0, 1, 2): os.dup2(fd, i)\n\
             while True:\n    k, _ = s.accept(); k.recv(4096)\n    \
             k.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 6\\r\\nConnection: close\\r\\n\\r\\nINSIDE'); k.close()\n",
        )
        .unwrap();
        let (ok, out) = c.run_net("python3 web.py", crate::landlock::Net::Private(s.port));
        assert!(ok, "{out}");
        let mut it = out.split_whitespace();
        let port: u16 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let pid: i32 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let url = format!("http://127.0.0.1:{port}/");
        let tools = crate::tools::web_client()
            .get(&url)
            .call()
            .ok()
            .and_then(|r| r.into_string().ok());
        let provider = crate::net::Client::new(|b| b)
            .get(&url)
            .call()
            .ok()
            .and_then(|r| r.into_string().ok());
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(tools.as_deref(), Some("INSIDE"), "the tools' client");
        assert_ne!(
            provider.as_deref(),
            Some("INSIDE"),
            "a provider client reached the namespace"
        );
    }

    // A dev server that binds `localhost` to ::1 alone (Node 17+, Vite) is
    // found inside the namespace by the tools' client asking for
    // `localhost`. Skips where the namespace has no IPv6.
    #[test]
    fn invariant_netns_tools_find_a_localhost_server_on_ipv6() {
        let Some(c) = Cell::new("netns-v6") else {
            return;
        };
        let s = crate::netns::session().expect("the session namespace");
        std::fs::write(
            c.ws.join("web6.py"),
            "import socket, os\n\
             try:\n    s = socket.socket(socket.AF_INET6); s.bind(('::1', 0)); s.listen(8)\n\
             except OSError as e:\n    print('NOV6', e); raise SystemExit(0)\n\
             pid = os.fork()\n\
             if pid:\n    print(s.getsockname()[1], pid, flush=True); os._exit(0)\n\
             os.setsid(); fd = os.open(os.devnull, os.O_RDWR)\n\
             for i in (0, 1, 2): os.dup2(fd, i)\n\
             while True:\n    k, _ = s.accept(); k.recv(4096)\n    \
             k.sendall(b'HTTP/1.1 200 OK\\r\\nContent-Length: 2\\r\\nConnection: close\\r\\n\\r\\nV6'); k.close()\n",
        )
        .unwrap();
        let (ok, out) = c.run_net("python3 web6.py", crate::landlock::Net::Private(s.port));
        assert!(ok, "{out}");
        if out.contains("NOV6") {
            eprintln!("skip: no IPv6 in the namespace here ({out})");
            return;
        }
        let mut it = out.split_whitespace();
        let port: u16 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let pid: i32 = it.next().and_then(|p| p.parse().ok()).expect(&out);
        let got = crate::tools::web_client()
            .get(&format!("http://localhost:{port}/"))
            .call()
            .ok()
            .and_then(|r| r.into_string().ok());
        unsafe { libc::kill(pid, libc::SIGKILL) };
        assert_eq!(got.as_deref(), Some("V6"));
    }
}
