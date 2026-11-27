//! Built-in Linux sandbox: Landlock for filesystem writes, seccomp for a
//! short deny-list (namespace and tracing escapes, and the network when
//! `sandbox_network` is false).
//!
//! Bubblewrap is preferred when it works: it gives the command a private
//! `/tmp` and can keep `.git` read-only inside a writable workspace.
//! Landlock cannot carve an exception out of a writable directory, so under
//! this backend `.git` and `.buildwithnexus` inside the workspace are
//! writable too. Writes outside the workspace and the temp dirs still fail.
//!
//! Reads are allowed everywhere except the user's credential stores
//! (sandbox::hidden_paths), which no rule grants; see `allow_reads_except`.
//!
//! The ruleset is built in the parent. The child, after `fork` and before
//! `exec`, drops new privileges, restricts itself to the ruleset, and
//! installs the seccomp filter. The parent process is never restricted.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::os::unix::process::CommandExt;

const CREATE_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;

// Landlock ABI 1 write-ish rights, plus ABI 2's refer and ABI 3's truncate.
// A right the running ABI does not know is left out of the ruleset: the
// kernel rejects an unknown one (EINVAL), which on Debian 12's 6.1 (ABI 2)
// refused every confined command (roadmap 1.7; analysis M3).
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
// The rights a rule on a file (not a directory) may carry; the kernel
// rejects a directory-only right on a file.
const FILE_RIGHTS: u64 = FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_REMOVE_DIR: u64 = 1 << 4;
const FS_REMOVE_FILE: u64 = 1 << 5;
const FS_MAKE_CHAR: u64 = 1 << 6;
const FS_MAKE_DIR: u64 = 1 << 7;
const FS_MAKE_REG: u64 = 1 << 8;
const FS_MAKE_SOCK: u64 = 1 << 9;
const FS_MAKE_FIFO: u64 = 1 << 10;
const FS_MAKE_BLOCK: u64 = 1 << 11;
const FS_MAKE_SYM: u64 = 1 << 12;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;

const ABI1: u64 = FS_WRITE_FILE
    | FS_REMOVE_DIR
    | FS_REMOVE_FILE
    | FS_MAKE_CHAR
    | FS_MAKE_DIR
    | FS_MAKE_REG
    | FS_MAKE_SOCK
    | FS_MAKE_FIFO
    | FS_MAKE_BLOCK
    | FS_MAKE_SYM;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
}

const RULE_NET_PORT: u32 = 2;
const NET_CONNECT_TCP: u64 = 1 << 1;

#[repr(C)]
struct NetPortAttr {
    allowed_access: u64,
    port: u64,
}

pub use crate::sandbox::Net;

/// Whether this kernel can limit TCP connections by port (Landlock ABI 4).
pub fn limits_connections() -> bool {
    abi().is_ok_and(|v| v >= 4)
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Directories and device nodes a Landlock-sandboxed command may write (and
/// read).
pub fn write_roots(cwd: &Path, extra: &[PathBuf], tmpdir: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut push = |p: &Path| {
        if p.as_os_str().is_empty() {
            return;
        }
        if !roots.iter().any(|r: &PathBuf| r == p) {
            roots.push(p.to_path_buf());
        }
    };
    push(cwd);
    for dir in extra {
        push(dir);
    }
    push(Path::new("/tmp"));
    if let Some(t) = tmpdir {
        push(t);
    }
    // A path-beneath rule on a single device node is not enough: git opens
    // /dev/null read-write, and a rule that failed to attach leaves that
    // denied. /dev is the set of nodes a command actually needs.
    push(Path::new("/dev"));
    roots
}

fn abi() -> io::Result<i32> {
    // SAFETY: the version query takes a null attribute, which the kernel
    // accepts when the flag is LANDLOCK_CREATE_RULESET_VERSION.
    let v = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0,
            CREATE_VERSION,
        )
    };
    if v < 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(v as i32)
}

fn handled_access(version: i32) -> u64 {
    let mut access = ABI1;
    if version >= 2 {
        access |= FS_REFER;
    }
    if version >= 3 {
        access |= FS_TRUNCATE;
    }
    access
}

/// Whether this kernel's Landlock confines truncate(2) by path (ABI 3,
/// kernel 6.2). Before that, a confined command can truncate a file it can
/// name outside the workspace (roadmap 1.7; analysis M2).
pub fn confines_truncate() -> bool {
    abi().is_ok_and(|v| v >= 3)
}

/// A ruleset fd that allows writes only under `write_roots`. The caller
/// closes it. Fails when the kernel has no Landlock or a writable root
/// that must exist (the workspace) cannot be opened.
///
/// With `hidden` paths, reading files is handled too: everything stays
/// readable except those paths (see `allow_reads_except`).
///
/// The network part of `net` is here too: Landlock limits TCP connects to
/// the proxy's port; the rest is seccomp's, in `engage`.
pub fn ruleset_with(
    cwd: &Path,
    extra: &[PathBuf],
    tmpdir: Option<&Path>,
    hidden: &[PathBuf],
    net: Net,
) -> io::Result<RawFd> {
    let version = abi()?;
    let write = handled_access(version);
    // With stores to hide, reading files and listing folders are handled:
    // allowed everywhere except in (and, for listing, above) a hidden store
    // (roadmap 2.2; analysis M5: names in ~/.ssh were listable).
    let read = if hidden.is_empty() {
        0
    } else {
        FS_READ_FILE | FS_READ_DIR
    };
    let access = write | read;
    let fd = create(access, net, version)?;
    let roots = write_roots(cwd, extra, tmpdir);
    for (i, root) in roots.iter().enumerate() {
        if let Err(e) = add_path(fd, root, write | read) {
            // The workspace itself must be writable. Device nodes and /tmp
            // are best-effort: a container without /dev/shm still confines.
            if i == 0 {
                unsafe { libc::close(fd) };
                return Err(e);
            }
        }
    }
    if read != 0 {
        allow_reads_except(fd, hidden);
    }
    Ok(fd)
}

// A ruleset handling `fs` access and, for Net::Proxy, TCP connects (with
// the one port granted).
fn create(fs: u64, net: Net, version: i32) -> io::Result<RawFd> {
    let net_access = match net {
        Net::Proxy(_) if version >= 4 => NET_CONNECT_TCP,
        Net::Proxy(_) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "this kernel's Landlock cannot limit connections (ABI 4 needed)",
            ))
        }
        _ => 0,
    };
    let attr = RulesetAttr {
        handled_access_fs: fs,
        handled_access_net: net_access,
    };
    // Before ABI 4 the attribute has no net field; pass the short size.
    let size = if net_access == 0 {
        std::mem::size_of::<u64>()
    } else {
        std::mem::size_of::<RulesetAttr>()
    };
    // SAFETY: attr is a valid ruleset attribute and outlives the call.
    let fd = unsafe { libc::syscall(libc::SYS_landlock_create_ruleset, &attr, size, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = fd as RawFd;
    if let Net::Proxy(port) = net {
        let rule = NetPortAttr {
            allowed_access: NET_CONNECT_TCP,
            port: u64::from(port),
        };
        // SAFETY: rule is a valid net-port rule; the kernel copies it.
        let rc = unsafe { libc::syscall(libc::SYS_landlock_add_rule, fd, RULE_NET_PORT, &rule, 0) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            unsafe { libc::close(fd) };
            return Err(e);
        }
    }
    Ok(fd)
}

/// A ruleset that limits only the network (for a command inside
/// bubblewrap, which confines the filesystem itself).
pub fn net_ruleset(net: Net) -> io::Result<RawFd> {
    let version = abi()?;
    create(0, net, version)
}

/// Whether Landlock can keep `hidden` unreadable for a command whose
/// writable roots are `roots`: a rule grants everything beneath its path,
/// so a root that holds a hidden path (a session run from the home folder)
/// would make it readable again, and Landlock cannot carve it out.
pub fn can_hide(roots: &[&Path], hidden: &[PathBuf]) -> bool {
    !hidden
        .iter()
        .any(|h| roots.iter().any(|r| h.starts_with(r)))
}

// Landlock only grants: there is no rule that takes a path away. So every
// path is made readable except the hidden ones by walking from `/` down to
// each hidden path and granting the siblings met on the way: the entries of
// `/` but `home`, the entries of `/home` but the user's folder, and so on.
// Symlinks are skipped (a rule on one would grant its target, which may be
// a hidden path); what they point to is readable through its own rule.
// Entries created later in those few folders are not readable, which is
// fine: a command cannot create them, since it cannot write there.
// Listing is granted the same way, so the folders on the way (`/`, `/home`,
// the home folder) cannot be listed: listing them would name the store.
fn allow_reads_except(ruleset: RawFd, hidden: &[PathBuf]) {
    use std::collections::{HashMap, HashSet};
    let mut on_path: HashMap<PathBuf, HashSet<std::ffi::OsString>> = HashMap::new();
    for h in hidden {
        let mut child = h.as_path();
        while let Some(parent) = child.parent() {
            if let Some(name) = child.file_name() {
                on_path
                    .entry(parent.to_path_buf())
                    .or_default()
                    .insert(name.to_os_string());
            }
            child = parent;
        }
    }
    let mut stack = vec![PathBuf::from("/")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let skip = on_path.get(&dir);
        for e in entries.flatten() {
            let name = e.file_name();
            let path = e.path();
            if skip.is_some_and(|s| s.contains(&name)) {
                if !hidden.contains(&path) {
                    stack.push(path);
                }
                continue;
            }
            match e.file_type() {
                Ok(t) if t.is_symlink() => {}
                Ok(_) => {
                    let _ = add_path(ruleset, &path, FS_READ_FILE | FS_READ_DIR);
                }
                Err(_) => {}
            }
        }
    }
}

fn add_path(ruleset: RawFd, path: &Path, access: u64) -> io::Result<()> {
    let access = if std::fs::metadata(path).is_ok_and(|m| m.is_dir()) {
        access
    } else {
        access & FILE_RIGHTS
    };
    let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL"))?;
    // SAFETY: O_PATH opens the node without using it; closed before return.
    let parent = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if parent < 0 {
        return Err(io::Error::last_os_error());
    }
    let attr = PathBeneathAttr {
        allowed_access: access,
        parent_fd: parent,
    };
    // SAFETY: attr points at a packed path-beneath rule; the kernel copies it.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_landlock_add_rule,
            ruleset,
            RULE_PATH_BENEATH,
            &attr,
            0,
        )
    };
    unsafe { libc::close(parent) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// True when a throwaway child can read the Landlock ABI. The parent is
/// not restricted.
pub fn available() -> Result<(), String> {
    let mut child = Command::new("sh");
    child
        .args(["-c", "true"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    unsafe {
        child.pre_exec(|| {
            abi()?;
            Ok(())
        });
    }
    match child.output() {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!("landlock probe exited {o:?}")),
        Err(e) => Err(format!("landlock is not available: {e}")),
    }
}

/// Apply `ruleset` to this process, then the seccomp deny-list. For the
/// child after fork only. `deny_truncate` refuses truncate(2) outright,
/// for a Landlock ABI that cannot confine it by path (`confines_truncate`).
pub fn engage(ruleset: RawFd, net: Net, deny_truncate: bool) -> io::Result<()> {
    // Landlock and seccomp both require no_new_privs.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ruleset is a landlock ruleset fd inherited across fork.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    seccomp(net, deny_truncate)
}

/// `cmd` as a child that confines itself with Landlock and seccomp.
pub fn command(cmd: &str, cwd: &Path, extra: &[PathBuf], net: Net) -> Result<Command, String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from);
    let roots: Vec<&Path> = std::iter::once(cwd.as_path())
        .chain(extra.iter().map(PathBuf::as_path))
        .collect();
    let hidden = crate::sandbox::hidden_paths(&roots);
    // A root that holds a hidden path cannot have it carved out; the
    // session reports reads as not confined then (sandbox::hides_reads).
    let hidden = if can_hide(&roots, &hidden) {
        hidden
    } else {
        Vec::new()
    };
    let fd = ruleset_with(&cwd, extra, tmp.as_deref(), &hidden, net)
        .map_err(|e| format!("landlock ruleset: {e} (the kernel may not have Landlock enabled)"))?;
    // SAFETY: ruleset returned a fresh fd that nothing else owns.
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };
    // Below ABI 3 Landlock cannot refuse truncate(2) on a path outside the
    // workspace, so seccomp refuses the call everywhere (analysis M2).
    // ftruncate stays: it needs a file opened for writing, which Landlock
    // does check.
    let deny_truncate = !confines_truncate();
    let mut child = Command::new("sh");
    child.args(["-c", cmd]).current_dir(&cwd);
    // The hook owns the parent's copy of the ruleset fd, so it is closed
    // when the Command is dropped after spawning. Before, only the child
    // closed its copy and every sandboxed command leaked one fd in bwn.
    // The child only borrows it: it closes its own copy and execs, and
    // never runs the OwnedFd's drop.
    unsafe {
        child.pre_exec(move || {
            let fd = owned.as_raw_fd();
            let r = engage(fd, net, deny_truncate);
            let _ = libc::close(fd);
            r
        });
    }
    Ok(child)
}

/// `bwn __confine-net <port> -- <program> <args…>`: inside bubblewrap,
/// limit this process's network to the egress proxy's port (Landlock and
/// seccomp, as `command` does for the built-in backend), then become the
/// program. Never returns.
pub fn confine_net_and_exec(args: &[String]) -> ! {
    use std::os::unix::process::CommandExt;
    let fail = |e: String| -> ! {
        eprintln!("bwn sandbox: {e}");
        std::process::exit(126)
    };
    let (Some(port), Some(rest)) = (
        args.first().and_then(|p| p.parse::<u16>().ok()),
        args.iter().position(|a| a == "--").map(|i| &args[i + 1..]),
    ) else {
        fail("usage: __confine-net <port> -- <program> <args>".into())
    };
    let Some((prog, prog_args)) = rest.split_first() else {
        fail("no program to run".into())
    };
    let net = Net::Proxy(port);
    let fd = net_ruleset(net).unwrap_or_else(|e| fail(format!("network rules: {e}")));
    // Inside bubblewrap the mount namespace already confines truncate(2).
    if let Err(e) = engage(fd, net, false) {
        fail(format!("network rules: {e}"));
    }
    unsafe { libc::close(fd) };
    let e = Command::new(prog).args(prog_args).exec();
    fail(format!("{prog}: {e}"))
}

// ── seccomp ──────────────────────────────────────────────────────────────────

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
const BPF_JGE: u16 = 0x30;
const BPF_K: u16 = 0x00;
const BPF_RET: u16 = 0x06;

const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;

#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

const BPF_ALU: u16 = 0x04;
const BPF_AND: u16 = 0x50;
// Offsets into struct seccomp_data: nr, arch, then args[i] at 16 + 8i
// (the low word on these little-endian targets).
const ARG0: u32 = 16;
const ARG1: u32 = 24;
const ARG2: u32 = 32;
const ARG3: u32 = 40;
const MSG_FASTOPEN: u32 = 0x2000_0000;
#[cfg(target_arch = "x86_64")]
const X32_SYSCALL_BIT: u32 = 0x4000_0000;
// O_TRUNC in the open flags, and the access-mode mask. On Linux these are
// the same on every architecture bwn builds for.
const O_TRUNC: u32 = 0x200;
const O_ACCMODE: u32 = 0x3;

fn seccomp(net: Net, deny_truncate: bool) -> io::Result<()> {
    let block_network = net == Net::Off;
    let arch = audit_arch();
    let mut deny: Vec<i64> = vec![
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        // io_uring can open connections and files outside these checks'
        // reach (seccomp sees only the io_uring call).
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ];
    if deny_truncate {
        deny.push(libc::SYS_truncate);
    }
    if block_network {
        deny.extend([
            libc::SYS_socket,
            libc::SYS_connect,
            libc::SYS_bind,
            libc::SYS_listen,
            libc::SYS_accept,
            libc::SYS_accept4,
            libc::SYS_sendto,
            libc::SYS_sendmsg,
            libc::SYS_recvfrom,
            libc::SYS_recvmsg,
        ]);
    }
    // Load the architecture, reject anything else, then deny the list.
    let mut prog = vec![
        stmt(BPF_LD | BPF_W | BPF_ABS, 4),
        jump(BPF_JMP | BPF_JEQ | BPF_K, arch, 1, 0),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (libc::EPERM as u32)),
        stmt(BPF_LD | BPF_W | BPF_ABS, 0),
    ];
    // x32 syscalls share the x86-64 architecture value and set bit 30 of
    // the number, so no number below would match them (analysis M8c).
    #[cfg(target_arch = "x86_64")]
    prog.extend([
        jump(BPF_JMP | BPF_JGE | BPF_K, X32_SYSCALL_BIT, 0, 1),
        stmt(BPF_RET | BPF_K, SECCOMP_RET_ERRNO | (libc::EPERM as u32)),
    ]);
    // Below Landlock ABI 3, truncate(2) is refused above; but an
    // `open(O_RDONLY|O_TRUNC)` also truncates and the ruleset grants
    // READ_FILE broadly, so Landlock there does not catch it. Refuse that
    // open (and openat2, whose flags sit behind a pointer seccomp cannot
    // read, so a caller falls back to openat). A write-mode O_TRUNC open
    // needs WRITE_FILE, which Landlock confines, so it is left alone.
    if deny_truncate {
        prog.extend(otrunc_filter());
    }
    if let Net::Proxy(_) = net {
        prog.extend(proxy_filter());
    }
    for nr in deny {
        prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr as u32, 0, 1));
        prog.push(stmt(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        ));
    }
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

    install(&prog)
}

// With the network limited to the proxy: sockets only of the kinds that
// Landlock's port rule covers (TCP over IPv4 or IPv6), so no UDP (DNS
// tunnels) and no Unix sockets (the SSH agent, the Docker socket); and no
// TCP Fast Open, whose implicit connect from sendto/sendmsg is not a
// connect() call. Each block starts with the syscall number loaded and
// either returns or falls through with it still loaded.
fn proxy_filter() -> Vec<SockFilter> {
    let deny = SECCOMP_RET_ERRNO | (libc::EPERM as u32);
    let ld = |off| stmt(BPF_LD | BPF_W | BPF_ABS, off);
    let ret = |k| stmt(BPF_RET | BPF_K, k);
    let jeq = |k, jt, jf| jump(BPF_JMP | BPF_JEQ | BPF_K, k, jt, jf);
    let mut p = vec![
        jeq(libc::SYS_socket as u32, 0, 8),
        ld(ARG0),
        jeq(libc::AF_INET as u32, 1, 0),
        jeq(libc::AF_INET6 as u32, 0, 3),
        ld(ARG1),
        stmt(BPF_ALU | BPF_AND | BPF_K, 0xf),
        jeq(libc::SOCK_STREAM as u32, 1, 0),
        ret(deny),
        ret(SECCOMP_RET_ALLOW),
    ];
    for (nr, flags) in [(libc::SYS_sendto, ARG3), (libc::SYS_sendmsg, ARG2)] {
        p.extend([
            jeq(nr as u32, 0, 5),
            ld(flags),
            stmt(BPF_ALU | BPF_AND | BPF_K, MSG_FASTOPEN),
            jeq(0, 0, 1),
            ret(SECCOMP_RET_ALLOW),
            ret(deny),
        ]);
    }
    p
}

// Deny `open`/`openat` with O_TRUNC in O_RDONLY mode, and openat2 outright
// (ENOSYS, so glibc falls back to openat). Each block starts with the
// syscall number loaded; a non-match skips the block with it still loaded,
// and a match returns. Used only when `deny_truncate` is set (ABI < 3).
fn otrunc_filter() -> Vec<SockFilter> {
    let deny = SECCOMP_RET_ERRNO | (libc::EPERM as u32);
    let ld = |off| stmt(BPF_LD | BPF_W | BPF_ABS, off);
    let and = |k| stmt(BPF_ALU | BPF_AND | BPF_K, k);
    let ret = |k| stmt(BPF_RET | BPF_K, k);
    let jeq = |k, jt, jf| jump(BPF_JMP | BPF_JEQ | BPF_K, k, jt, jf);
    // openat2's flags are unreadable here; make callers fall back to openat.
    let mut p = vec![
        jeq(libc::SYS_openat2 as u32, 0, 1),
        ret(SECCOMP_RET_ERRNO | (libc::ENOSYS as u32)),
    ];
    // (syscall number, byte offset of its flags argument in seccomp_data).
    #[cfg(target_arch = "x86_64")]
    let opens: &[(i64, u32)] = &[(libc::SYS_openat, ARG2), (libc::SYS_open, ARG1)];
    #[cfg(not(target_arch = "x86_64"))]
    let opens: &[(i64, u32)] = &[(libc::SYS_openat, ARG2)];
    for &(nr, flags) in opens {
        p.extend([
            jeq(nr as u32, 0, 8),
            ld(flags),
            and(O_TRUNC),
            jeq(0, 4, 0), // no O_TRUNC bit -> allow
            ld(flags),
            and(O_ACCMODE),
            jeq(0, 0, 1), // O_RDONLY (0) -> deny; any write mode -> allow
            ret(deny),
            ret(SECCOMP_RET_ALLOW),
        ]);
    }
    p
}

fn install(prog: &[SockFilter]) -> io::Result<()> {
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const SockFilter,
    }
    let fprog = SockFprog {
        len: prog.len() as u16,
        filter: prog.as_ptr(),
    };
    // SAFETY: fprog points at prog, which outlives the prctl that copies it.
    let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &fprog) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn audit_arch() -> u32 {
    #[cfg(target_arch = "x86_64")]
    {
        0xC000_003E
    }
    #[cfg(target_arch = "aarch64")]
    {
        0xC000_00B7
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_roots_include_the_workspace_and_temp_and_not_the_home_dir() {
        let cwd = Path::new("/work/app");
        let extra = vec![PathBuf::from("/other")];
        let roots = write_roots(cwd, &extra, Some(Path::new("/var/tmp/build")));
        assert!(roots.iter().any(|p| p == Path::new("/work/app")));
        assert!(roots.iter().any(|p| p == Path::new("/other")));
        assert!(roots.iter().any(|p| p == Path::new("/tmp")));
        assert!(roots.iter().any(|p| p == Path::new("/var/tmp/build")));
        assert!(roots.iter().any(|p| p == Path::new("/dev")));
        assert!(!roots.iter().any(|p| p == Path::new("/home")));
    }

    // Each command used to leave its ruleset fd open in bwn. Other tests
    // open files on other threads meanwhile, so the check allows some
    // noise, far below one fd per command.
    #[test]
    fn a_confined_command_does_not_leak_its_ruleset_fd() {
        if available().is_err() {
            return;
        }
        let open_fds = || std::fs::read_dir("/proc/self/fd").map_or(0, |d| d.count());
        let dir = std::env::temp_dir();
        let runs = 64;
        let before = open_fds();
        for _ in 0..runs {
            let mut cmd = command("true", &dir, &[], Net::Off).expect("ruleset");
            let out = cmd.stdout(Stdio::null()).output().expect("spawn");
            assert!(out.status.success(), "{:?}", out.status);
        }
        let after = open_fds();
        assert!(
            after < before + runs / 2,
            "{} fds open after {runs} confined commands, {before} before",
            after
        );
    }

    #[test]
    fn a_confined_command_can_write_inside_and_not_outside() {
        if available().is_err() {
            return;
        }
        // Under the crate's target dir, not /tmp (a writable root) and not
        // a path that only exists on one machine. A sibling of the workspace
        // must stay unwritable.
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!(
                "bwn-landlock-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        let dir = base.join("ws");
        let outside = base.join("out.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&dir).unwrap();
        let script = format!(
            "echo inside > in.txt; echo outside > {} ; echo done",
            outside.display()
        );
        let mut cmd = command(&script, &dir, &[], Net::Open).expect("ruleset");
        let out = cmd.output().expect("spawn");
        let inside = std::fs::read_to_string(dir.join("in.txt")).unwrap_or_default();
        let leaked = std::fs::read_to_string(&outside).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&base);
        assert!(
            out.status.success(),
            "status {:?} stderr {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(inside.contains("inside"), "{inside}");
        assert!(
            !leaked.contains("outside"),
            "landlock allowed a write outside the workspace: {leaked}"
        );
    }

    // Conformance (roadmap 2.1): the write guarantee is more than "a write
    // to a new path is refused" — renaming, linking, creating, removing and
    // making a directory outside the workspace are all refused too. Only a
    // file's metadata leaks (the test below) and, below ABI 3, truncation
    // (the seccomp tests). Probing each path, not just one, is what caught
    // the O_TRUNC gap.
    #[test]
    fn invariant_landlock_confines_write_syscall_paths_outside() {
        if available().is_err() {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("bwn-landlock-paths-{}", std::process::id()));
        let ws = base.join("ws");
        let outside = base.join("outside");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim.txt"), "v").unwrap();
        let o = outside.display();
        // Each line prints WORKED only if the operation outside succeeded.
        let script = format!(
            "cd {o}; \
             mv victim.txt moved.txt 2>/dev/null && echo rename-WORKED; \
             ln moved.txt hard.txt 2>/dev/null && echo hardlink-WORKED; \
             ln -s moved.txt sym.txt 2>/dev/null && echo symlink-WORKED; \
             touch newfile.txt 2>/dev/null && echo create-WORKED; \
             mkdir newdir 2>/dev/null && echo mkdir-WORKED; \
             rm moved.txt 2>/dev/null && echo unlink-WORKED; \
             echo end"
        );
        let mut cmd = command(&script, &ws, &[], Net::Open).expect("ruleset");
        let out = cmd.output().expect("spawn");
        let said = String::from_utf8_lossy(&out.stdout);
        let _ = std::fs::remove_dir_all(&base);
        assert!(said.contains("end"), "the probe did not run: {said}");
        assert!(
            !said.contains("WORKED"),
            "landlock let a write-path syscall touch outside the workspace: {said}"
        );
    }

    // Characterization (roadmap 2.2): Landlock has no right for a file's
    // metadata, so `chmod`, `chown`, `utime` and xattrs on a file outside
    // the workspace are not confined. The capability table marks the write
    // guarantee Partial for this reason; a confined command can't read a
    // credential, but it can change one's mode. If this ever starts to be
    // confined, this test fails and the table should be upgraded.
    #[test]
    fn landlock_does_not_confine_file_metadata_outside() {
        use std::os::unix::fs::PermissionsExt;
        if available().is_err() {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("bwn-landlock-meta-{}", std::process::id()));
        let ws = base.join("ws");
        let outside = base.join("keep.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(&outside, "k").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        let mut cmd = command(
            &format!("chmod 600 {}", outside.display()),
            &ws,
            &[],
            Net::Open,
        )
        .expect("ruleset");
        let _ = cmd.output().expect("spawn");
        let mode = std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            mode, 0o600,
            "landlock now confines chmod outside the workspace: upgrade the capability table"
        );
    }

    // Characterization (roadmap 2.2): Landlock blocks listing a hidden
    // store's folder (so names can't be enumerated: see M5), but it does not
    // block `stat` of a path, so a guessed standard name (`~/.ssh/id_ed25519`)
    // can still be confirmed to exist. The table marks the credential
    // guarantee Partial for this. Contents stay unreadable either way.
    #[test]
    fn landlock_credential_names_leak_via_stat() {
        use std::os::unix::process::CommandExt;
        if available().is_err() {
            return;
        }
        let hbase = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|h| h.is_dir() && !h.starts_with("/tmp"))
            .unwrap_or_else(|| PathBuf::from("/var/tmp"));
        let home = hbase.join(format!("bwn-stat-leak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::create_dir_all(home.join("proj")).unwrap();
        std::fs::write(home.join(".ssh/id_test"), "KEY").unwrap();
        let home = home.canonicalize().unwrap();
        let ws = home.join("proj");
        let hidden = crate::sandbox::hidden_paths_in(
            &home,
            &home.join(".buildwithnexus"),
            None,
            &[],
            &[&ws],
        );
        let fd = ruleset_with(&ws, &[], None, &hidden, Net::Off).unwrap();
        let mut c = Command::new("sh");
        c.args([
            "-c",
            &format!(
                "stat {} >/dev/null 2>&1 && echo FOUND || echo HIDDEN",
                home.join(".ssh/id_test").display()
            ),
        ])
        .current_dir(&ws);
        unsafe {
            c.pre_exec(move || engage(fd, Net::Off, false));
        }
        let out = c.output().unwrap();
        unsafe { libc::close(fd) };
        // The contents must still be unreadable even though stat succeeds.
        let mut r = Command::new("sh");
        r.args([
            "-c",
            &format!("cat {} 2>/dev/null", home.join(".ssh/id_test").display()),
        ])
        .current_dir(&ws);
        let fd2 = ruleset_with(&ws, &[], None, &hidden, Net::Off).unwrap();
        unsafe {
            r.pre_exec(move || engage(fd2, Net::Off, false));
        }
        let read = r.output().unwrap();
        unsafe { libc::close(fd2) };
        let _ = std::fs::remove_dir_all(&home);
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "FOUND",
            "landlock now hides a guessed store name from stat: upgrade the capability table"
        );
        assert!(
            !String::from_utf8_lossy(&read.stdout).contains("KEY"),
            "landlock leaked the credential contents, not just its name"
        );
    }

    // Roadmap 2.2 (analysis M2): the seccomp half, forced on so it runs on
    // any kernel. truncate(2) is refused inside the workspace too (the
    // disclosed cost); opening with O_TRUNC and ftruncate still work there.
    #[test]
    fn invariant_m2_seccomp_refuses_truncate_below_abi_3() {
        use std::os::unix::process::CommandExt;
        if available().is_err() {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("bwn-landlock-trunc-sc-{}", std::process::id()));
        let ws = base.join("ws");
        let outside = base.join("keep.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(&outside, "keep").unwrap();
        for f in ["path.txt", "otrunc.txt", "ftrunc.txt"] {
            std::fs::write(ws.join(f), "data").unwrap();
        }
        let fd = ruleset_with(&ws, &[], None, &[], Net::Open).unwrap();
        let script = format!(
            "perl -e 'truncate($ARGV[0], 0) or print \"refused \"' {}; \
             perl -e 'truncate($ARGV[0], 0) or print \"refused \"' path.txt; \
             : > otrunc.txt; \
             perl -e 'open(my $f, \"+<\", $ARGV[0]) or die; truncate($f, 0) or print \"ftruncate-refused \"' ftrunc.txt",
            outside.display()
        );
        let mut c = Command::new("sh");
        c.args(["-c", &script]).current_dir(&ws);
        unsafe {
            c.pre_exec(move || engage(fd, Net::Open, true));
        }
        let out = c.output().expect("spawn");
        unsafe { libc::close(fd) };
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let (kept, path, otrunc, ftrunc) = (
            read(&outside),
            read(&ws.join("path.txt")),
            read(&ws.join("otrunc.txt")),
            read(&ws.join("ftrunc.txt")),
        );
        let _ = std::fs::remove_dir_all(&base);
        let said = String::from_utf8_lossy(&out.stdout);
        assert_eq!(kept, "keep", "truncated outside: {said}");
        assert_eq!(path, "data", "truncate(2) inside was not refused: {said}");
        assert_eq!(otrunc, "", "O_TRUNC inside failed: {said}");
        assert_eq!(ftrunc, "", "ftruncate inside failed: {said}");
    }

    // Roadmap 2.2 (analysis M2): the O_RDONLY|O_TRUNC open path. Below ABI 3
    // Landlock only grants READ_FILE on an outside file, so such an open
    // truncates it unless seccomp stops it. Build an ABI1-only ruleset (no
    // FS_TRUNCATE) so the check holds on any kernel, not just old ones: the
    // outside file must survive, and a normal write-mode truncate inside the
    // workspace must still work.
    #[test]
    fn invariant_m2_seccomp_refuses_otrunc_open_below_abi3() {
        use std::os::unix::process::CommandExt;
        if available().is_err() {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("bwn-landlock-otrunc-{}", std::process::id()));
        let ws = base.join("ws");
        let outside = base.join("keep.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(&outside, "keepdata").unwrap();
        std::fs::write(ws.join("inside.txt"), "insidedata").unwrap();
        // ABI1-only ruleset: writes handled, truncate not (like ABI 1 and 2).
        let fd = create(ABI1, Net::Open, abi().unwrap()).unwrap();
        add_path(fd, &ws, ABI1).unwrap();
        // O_RDONLY|O_TRUNC on the outside file (its open is refused when the
        // filter is on), then a normal write-mode truncate inside.
        let script = format!(
            "python3 -c 'import os,sys; os.close(os.open(sys.argv[1], os.O_RDONLY|os.O_TRUNC))' {}; : > inside.txt",
            outside.display()
        );
        let mut c = Command::new("sh");
        c.args(["-c", &script]).current_dir(&ws);
        unsafe {
            c.pre_exec(move || engage(fd, Net::Open, true));
        }
        let out = c.output().expect("spawn");
        unsafe { libc::close(fd) };
        let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
        let (kept, inside) = (read(&outside), read(&ws.join("inside.txt")));
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            kept,
            "keepdata",
            "O_RDONLY|O_TRUNC emptied the outside file: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            inside, "",
            "a normal write-mode truncate inside was blocked"
        );
    }

    // Roadmap 1.7 and 2.2 (analysis M2): truncate(2) on a file outside the
    // workspace is a write. Landlock handles it from ABI 3 (kernel 6.2);
    // below that, seccomp refuses truncate(2) altogether. Run on 5.15 and
    // 6.1 in the field cells.
    #[test]
    fn invariant_m2_a_confined_command_cannot_truncate_outside() {
        if available().is_err() {
            return;
        }
        let base = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join(format!("bwn-landlock-trunc-{}", std::process::id()));
        let dir = base.join("ws");
        let outside = base.join("keep.txt");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&outside, "keep").unwrap();
        // truncate(2) on the path, which coreutils' truncate does not use
        // (it opens the file for writing first, which every ABI refuses).
        let script = format!(
            "perl -e 'truncate($ARGV[0], 0)' {0} || python3 -c 'import os,sys; os.truncate(sys.argv[1], 0)' {0}; echo done",
            outside.display()
        );
        let Ok(mut cmd) = command(&script, &dir, &[], Net::Open) else {
            let _ = std::fs::remove_dir_all(&base);
            panic!("no ruleset on ABI {:?}", abi());
        };
        let out = cmd.output().expect("spawn");
        let after = std::fs::read_to_string(&outside).unwrap_or_default();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!(
            after,
            "keep",
            "ABI {:?}: truncated outside the workspace ({})",
            abi(),
            String::from_utf8_lossy(&out.stdout)
        );
    }
}
