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
//! The ruleset is built in the parent. The child, after `fork` and before
//! `exec`, drops new privileges, restricts itself to the ruleset, and
//! installs the seccomp filter. The parent process is never restricted.

use std::io;
use std::os::fd::RawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use std::os::unix::process::CommandExt;

const CREATE_VERSION: u32 = 1;
const RULE_PATH_BENEATH: u32 = 1;

// Landlock ABI 1 write-ish rights, plus ABI 2's truncate and refer. A right
// the running ABI does not know is dropped before the ruleset is created.
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
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Directories and device nodes a Landlock-sandboxed command may write.
/// Reads stay allowed everywhere, because the ruleset does not handle them.
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
        access |= FS_REFER | FS_TRUNCATE;
    }
    access
}

/// A ruleset fd that allows writes only under `write_roots`. The caller
/// closes it. Fails when the kernel has no Landlock or a writable root
/// that must exist (the workspace) cannot be opened.
pub fn ruleset(cwd: &Path, extra: &[PathBuf], tmpdir: Option<&Path>) -> io::Result<RawFd> {
    let version = abi()?;
    let access = handled_access(version);
    let attr = RulesetAttr {
        handled_access_fs: access,
    };
    // SAFETY: attr is a valid ruleset attribute and outlives the call.
    let fd = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            &attr,
            std::mem::size_of::<RulesetAttr>(),
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = fd as RawFd;
    let roots = write_roots(cwd, extra, tmpdir);
    for (i, root) in roots.iter().enumerate() {
        if let Err(e) = add_path(fd, root, access) {
            // The workspace itself must be writable. Device nodes and /tmp
            // are best-effort: a container without /dev/shm still confines.
            if i == 0 {
                unsafe { libc::close(fd) };
                return Err(e);
            }
        }
    }
    Ok(fd)
}

fn add_path(ruleset: RawFd, path: &Path, access: u64) -> io::Result<()> {
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
/// child after fork only.
pub fn engage(ruleset: RawFd, block_network: bool) -> io::Result<()> {
    // Landlock and seccomp both require no_new_privs.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: ruleset is a landlock ruleset fd inherited across fork.
    if unsafe { libc::syscall(libc::SYS_landlock_restrict_self, ruleset, 0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    seccomp(block_network)
}

/// `cmd` as a child that confines itself with Landlock and seccomp.
pub fn command(cmd: &str, cwd: &Path, extra: &[PathBuf], network: bool) -> Result<Command, String> {
    let cwd = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let tmp = std::env::var_os("TMPDIR").map(PathBuf::from);
    let fd = ruleset(&cwd, extra, tmp.as_deref())
        .map_err(|e| format!("landlock ruleset: {e} (the kernel may not have Landlock enabled)"))?;
    let block_network = !network;
    let mut child = Command::new("sh");
    child.args(["-c", cmd]).current_dir(&cwd);
    unsafe {
        child.pre_exec(move || {
            let r = engage(fd, block_network);
            let _ = libc::close(fd);
            r
        });
    }
    Ok(child)
}

// ── seccomp ──────────────────────────────────────────────────────────────────

const BPF_LD: u16 = 0x00;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JMP: u16 = 0x05;
const BPF_JEQ: u16 = 0x10;
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

fn seccomp(block_network: bool) -> io::Result<()> {
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
    ];
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
    for nr in deny {
        prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr as u32, 0, 1));
        prog.push(stmt(
            BPF_RET | BPF_K,
            SECCOMP_RET_ERRNO | (libc::EPERM as u32),
        ));
    }
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

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

    #[test]
    fn a_confined_command_can_write_inside_and_not_outside() {
        if available().is_err() {
            return;
        }
        // An absolute path that is not under /tmp (a writable root) and does
        // not follow another test's set_current_dir. A sibling of the
        // workspace must stay unwritable.
        let base = PathBuf::from("/agent/repos/buildwithnexus/target").join(format!(
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
        let mut cmd = command(&script, &dir, &[], true).expect("ruleset");
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
}
