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
        })
    }

    /// In the child, after `fork` and before `exec`: enter the namespaces,
    /// set up the mounts, return to the workspace and drop every capability.
    /// Async-signal-safe: raw syscalls only, nothing allocated.
    pub fn enter(&self) -> io::Result<()> {
        // SAFETY: each call below passes valid, NUL-terminated buffers that
        // live in `self` (or are string literals) for the whole call.
        unsafe {
            check(libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS))?;
            write_file(c"/proc/self/setgroups", c"deny")?;
            write_file(c"/proc/self/uid_map", &self.uid_map)?;
            write_file(c"/proc/self/gid_map", &self.gid_map)?;
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
            "echo in > rel.txt && echo REL-OK; echo t > /tmp/bwn-userns-probe && echo TMP-OK; echo x > /dev/null && echo DEV-OK; touch .git/hooks/pre-commit 2>/dev/null && echo GIT-WROTE; touch vendor/lib/.git/hooks/post-checkout 2>/dev/null && echo NESTED-WROTE; mkdir .buildwithnexus/x 2>/dev/null && echo BWN-WROTE; echo end",
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
}
