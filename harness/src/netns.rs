//! One private network namespace per session (roadmap 2.2).
//!
//! A sandboxed command must not reach the internet, the local network or
//! the host's own services directly: only the host checker (egress.rs).
//! Landlock's network rule holds a command by port, so on the host network
//! the checker's port on another address still connects (SEC-2). Here every
//! command instead joins one network namespace made for the session, which
//! holds nothing but a loopback interface and the checker's listener.
//!
//! The namespace is made by a holder: a child forked from bwn that enters a
//! new user and network namespace (the user mapped to itself), brings up
//! `lo`, binds a listener on `127.0.0.1` and sends bwn three descriptors
//! over a socketpair: the listener and its own user and network namespaces.
//! bwn serves the listener with the checker and keeps the namespace
//! descriptors for the session, so the namespace lives as long as bwn,
//! whatever happens to the holder. A command joins with `setns` before its
//! own mount namespace (userns.rs). Servers a command starts stay reachable
//! from the next command, and from bwn's own tools through [`Session::dial`],
//! which asks the holder for a socket made inside the namespace.
//!
//! The holder is a copy of bwn's memory, keys included, living in the
//! commands' user namespace, so it makes itself non-dumpable first: a
//! command can't ptrace it or read its memory. It dies with bwn
//! (`PR_SET_PDEATHSIG`) and exits when bwn closes its end.
//!
//! Everything the holder needs is computed before `fork`; it makes raw
//! syscalls only.

use std::ffi::CString;
use std::io;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::{Mutex, OnceLock};

/// The session's namespace.
pub struct Session {
    user: OwnedFd,
    net: OwnedFd,
    /// The checker's port on `127.0.0.1` inside the namespace.
    pub port: u16,
    /// The holder, one request at a time.
    holder: Mutex<Holder>,
}

struct Holder {
    /// bwn's end of the socketpair.
    fd: OwnedFd,
    #[cfg_attr(not(test), allow(dead_code))]
    pid: libc::pid_t,
}

static SESSION: OnceLock<Result<Session, String>> = OnceLock::new();

/// The session's namespace, made on first use. Err says why it can't be.
pub fn session() -> Result<&'static Session, String> {
    SESSION
        .get_or_init(|| {
            let (s, listener) = start()?;
            crate::egress::serve_listener(listener, true)
                .map_err(|e| format!("cannot serve the checker in the namespace: {e}"))?;
            Ok(s)
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Whether the session's namespace exists (commands have run in it).
pub fn active() -> bool {
    SESSION.get().is_some_and(|r| r.is_ok())
}

/// A connection to a loopback address for bwn's own tools: inside the
/// session's namespace if it exists and something listens there, else on
/// this machine (a server the user started outside bwn).
pub fn dial_loopback(addr: SocketAddr) -> io::Result<TcpStream> {
    if let Some(Ok(s)) = SESSION.get() {
        if let Ok(t) = s.dial(addr) {
            return Ok(t);
        }
    }
    TcpStream::connect_timeout(&addr, std::time::Duration::from_secs(10))
}

/// The namespace descriptors a command joins: the session's, or in a test,
/// the ones [`use_for_test`] set on this thread.
pub fn join_fds() -> Result<(RawFd, RawFd), String> {
    #[cfg(test)]
    if let Some(fds) = TEST_JOIN.with(|c| c.get()) {
        return Ok(fds);
    }
    Ok(session()?.fds())
}

#[cfg(test)]
thread_local! {
    static TEST_JOIN: std::cell::Cell<Option<(RawFd, RawFd)>> = const { std::cell::Cell::new(None) };
}

/// Commands built on this thread join `s` instead of the session's.
#[cfg(test)]
pub(crate) fn use_for_test(s: Option<&Session>) {
    TEST_JOIN.with(|c| c.set(s.map(Session::fds)));
}

/// A namespace of its own, served like the session's, for a test that
/// must not disturb the shared one.
#[cfg(test)]
pub(crate) fn start_for_test() -> Session {
    let (s, listener) = start().expect("netns");
    crate::egress::serve_listener(listener, true).expect("serve");
    s
}

impl Session {
    /// The user and network namespace descriptors a command joins.
    pub fn fds(&self) -> (RawFd, RawFd) {
        (self.user.as_raw_fd(), self.net.as_raw_fd())
    }

    /// The holder's pid (tests kill it).
    #[cfg(test)]
    pub(crate) fn holder_pid(&self) -> libc::pid_t {
        self.holder.lock().unwrap_or_else(|e| e.into_inner()).pid
    }

    /// A TCP connection to `addr` made inside the namespace, for bwn's own
    /// tools reaching a server a command started. A holder that has died (a
    /// command can kill it: same user) is replaced by one that joins the
    /// namespace bwn still holds.
    pub fn dial(&self, addr: SocketAddr) -> io::Result<TcpStream> {
        let mut h = self.holder.lock().unwrap_or_else(|e| e.into_inner());
        let fd = match ask(&h, addr) {
            Ok(fd) => fd,
            Err(_) => {
                let (fd, pid, _) = spawn(Some(self.fds())).map_err(io::Error::other)?;
                *h = Holder { fd, pid };
                ask(&h, addr)?
            }
        };
        drop(h);
        connect(fd, addr)
    }
}

// One socket from the holder, made inside the namespace.
fn ask(h: &Holder, addr: SocketAddr) -> io::Result<OwnedFd> {
    let ask = if addr.is_ipv6() { b'6' } else { b'4' };
    // SAFETY: one byte from a live stack buffer.
    let n = unsafe { libc::write(h.fd.as_raw_fd(), [ask].as_ptr().cast(), 1) };
    if n != 1 {
        return Err(io::Error::other("the namespace holder is gone"));
    }
    recv_fds(h.fd.as_raw_fd(), 1)?
        .into_iter()
        .next()
        .ok_or_else(|| io::Error::other("the namespace holder made no socket"))
}

// connect(2) on a socket the holder made, then hand it to std.
fn connect(fd: OwnedFd, addr: SocketAddr) -> io::Result<TcpStream> {
    let (storage, len) = sockaddr(addr);
    // SAFETY: storage holds a valid sockaddr of `len` bytes.
    let rc = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&storage as *const libc::sockaddr_storage).cast(),
            len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(TcpStream::from(fd))
}

fn sockaddr(addr: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is valid.
    let mut s: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = match addr {
        SocketAddr::V4(a) => {
            let sin = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: a.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from(*a.ip()).to_be(),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: sockaddr_storage is larger than sockaddr_in.
            unsafe { std::ptr::write((&mut s as *mut libc::sockaddr_storage).cast(), sin) };
            std::mem::size_of::<libc::sockaddr_in>()
        }
        SocketAddr::V6(a) => {
            let sin6 = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: a.port().to_be(),
                sin6_flowinfo: a.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: a.ip().octets(),
                },
                sin6_scope_id: a.scope_id(),
            };
            // SAFETY: sockaddr_storage is larger than sockaddr_in6.
            unsafe { std::ptr::write((&mut s as *mut libc::sockaddr_storage).cast(), sin6) };
            std::mem::size_of::<libc::sockaddr_in6>()
        }
    };
    (s, len as libc::socklen_t)
}

fn start() -> Result<(Session, TcpListener), String> {
    let (fd, pid, got) = spawn(None)?;
    let mut got = got.into_iter();
    let (Some(listener), Some(user), Some(net)) = (got.next(), got.next(), got.next()) else {
        return Err("the namespace holder sent too few descriptors".into());
    };
    let listener = TcpListener::from(listener);
    let port = listener
        .local_addr()
        .map_err(|e| format!("the namespace listener: {e}"))?
        .port();
    Ok((
        Session {
            user,
            net,
            port,
            holder: Mutex::new(Holder { fd, pid }),
        },
        listener,
    ))
}

// Fork a holder: a new namespace (`join` None; it sends the listener and
// the two namespace descriptors) or one joining `join` (it sends none).
fn spawn(join: Option<(RawFd, RawFd)>) -> Result<(OwnedFd, libc::pid_t, Vec<OwnedFd>), String> {
    // SAFETY: getters with no arguments.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let uid_map = CString::new(format!("{uid} {uid} 1")).map_err(|e| e.to_string())?;
    let gid_map = CString::new(format!("{gid} {gid} 1")).map_err(|e| e.to_string())?;
    let mut pair = [0 as RawFd; 2];
    // SAFETY: pair is a valid two-int buffer.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
            0,
            pair.as_mut_ptr(),
        )
    } != 0
    {
        return Err(format!("socketpair: {}", io::Error::last_os_error()));
    }
    // SAFETY: both fds are fresh and owned here.
    let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(pair[0]), OwnedFd::from_raw_fd(pair[1])) };
    // SAFETY: the child runs only raw syscalls (holder) and never returns.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(format!("fork: {}", io::Error::last_os_error()));
    }
    if pid == 0 {
        // SAFETY: in the forked child; see holder.
        unsafe { holder(theirs.as_raw_fd(), &uid_map, &gid_map, join) }
    }
    drop(theirs);
    let want = if join.is_some() { 0 } else { 3 };
    let got = recv_fds(ours.as_raw_fd(), want);
    let got = match got {
        Ok(f) if f.len() == want => f,
        other => {
            let mut status = 0;
            // SAFETY: pid is our child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
            let code = if libc::WIFEXITED(status) {
                libc::WEXITSTATUS(status)
            } else {
                0
            };
            let why = if code > 0 {
                io::Error::from_raw_os_error(code).to_string()
            } else {
                match other {
                    Err(e) => e.to_string(),
                    Ok(_) => "the holder sent too few descriptors".into(),
                }
            };
            return Err(format!(
                "cannot make the session's network namespace: {why}"
            ));
        }
    };
    // The holder runs for the session; reap it when it ends.
    std::thread::Builder::new()
        .name("bwn-netns-reap".into())
        .spawn(move || {
            let mut status = 0;
            // SAFETY: pid is our child.
            unsafe { libc::waitpid(pid, &mut status, 0) };
        })
        .map_err(|e| e.to_string())?;
    Ok((ours, pid, got))
}

const SIOCGIFFLAGS: libc::c_ulong = 0x8913;
const SIOCSIFFLAGS: libc::c_ulong = 0x8914;

// The forked child. Exits with an errno on failure, 0 when bwn closes its
// end. Async-signal-safe: raw syscalls and stack buffers only.
unsafe fn holder(
    sock: RawFd,
    uid_map: &CString,
    gid_map: &CString,
    join: Option<(RawFd, RawFd)>,
) -> ! {
    let fail = || {
        libc::_exit(
            io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(1)
                .clamp(1, 255),
        )
    };
    // Not dumpable: a command in this user namespace can't ptrace the
    // holder or read /proc/<pid>/mem (it is a copy of bwn's memory).
    libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    // Keep none of bwn's descriptors (its connections, its files) but our
    // end of the socketpair and the namespaces to join. It exits when bwn
    // closes its end, which the kernel does when bwn exits. (Not
    // PR_SET_PDEATHSIG: that fires when the forking *thread* exits.)
    close_all_but(&[sock, join.map_or(-1, |j| j.0), join.map_or(-1, |j| j.1)]);
    if let Some((user, net)) = join {
        if libc::setns(user, libc::CLONE_NEWUSER) != 0 || libc::setns(net, libc::CLONE_NEWNET) != 0
        {
            fail();
        }
        libc::close(user);
        libc::close(net);
        if send_fds(sock, &[]).is_err() {
            fail();
        }
        serve_sockets(sock);
    }
    if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) != 0 {
        fail();
    }
    if write_file(c"/proc/self/setgroups", c"deny").is_err()
        || write_file(c"/proc/self/uid_map", uid_map).is_err()
        || write_file(c"/proc/self/gid_map", gid_map).is_err()
    {
        fail();
    }
    // Bring up lo.
    let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
    if s < 0 {
        fail();
    }
    let mut ifr = [0u8; 40];
    ifr[..2].copy_from_slice(b"lo");
    if libc::ioctl(s, SIOCGIFFLAGS as _, ifr.as_mut_ptr()) != 0 {
        fail();
    }
    let flags = i16::from_ne_bytes([ifr[16], ifr[17]]) | libc::IFF_UP as i16;
    ifr[16..18].copy_from_slice(&flags.to_ne_bytes());
    if libc::ioctl(s, SIOCSIFFLAGS as _, ifr.as_mut_ptr()) != 0 {
        fail();
    }
    libc::close(s);
    // The checker's listener, on 127.0.0.1 inside.
    let l = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
    if l < 0 {
        fail();
    }
    let sin = libc::sockaddr_in {
        sin_family: libc::AF_INET as libc::sa_family_t,
        sin_port: 0,
        sin_addr: libc::in_addr {
            s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
        },
        sin_zero: [0; 8],
    };
    if libc::bind(
        l,
        (&sin as *const libc::sockaddr_in).cast(),
        std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
    ) != 0
        || libc::listen(l, 128) != 0
    {
        fail();
    }
    let user = libc::open(
        c"/proc/self/ns/user".as_ptr(),
        libc::O_RDONLY | libc::O_CLOEXEC,
    );
    let net = libc::open(
        c"/proc/self/ns/net".as_ptr(),
        libc::O_RDONLY | libc::O_CLOEXEC,
    );
    if user < 0 || net < 0 || send_fds(sock, &[l, user, net]).is_err() {
        fail();
    }
    libc::close(l);
    libc::close(user);
    libc::close(net);
    serve_sockets(sock)
}

// Close every descriptor from 3 up except `keep`. Stack only.
unsafe fn close_all_but(keep: &[RawFd]) {
    let mut keep_sorted = [-1 as RawFd; 3];
    let n = keep.len().min(3);
    keep_sorted[..n].copy_from_slice(&keep[..n]);
    keep_sorted.sort_unstable();
    let mut from: libc::c_uint = 3;
    for &k in &keep_sorted {
        if k < 3 {
            continue;
        }
        let k = k as libc::c_uint;
        if k > from {
            close_range(from, k - 1);
        }
        from = k + 1;
    }
    close_range(from, libc::c_uint::MAX);
}

unsafe fn close_range(first: libc::c_uint, last: libc::c_uint) {
    if libc::syscall(libc::SYS_close_range, first, last, 0) != 0 {
        // Before kernel 5.9: close one at a time, up to a sane limit.
        let mut fd = first;
        while fd <= last && fd < 65_536 {
            libc::close(fd as libc::c_int);
            fd += 1;
        }
    }
}

// Serve socket requests until bwn closes its end.
unsafe fn serve_sockets(sock: RawFd) -> ! {
    loop {
        let mut b = [0u8; 1];
        if libc::read(sock, b.as_mut_ptr().cast(), 1) != 1 {
            libc::_exit(0);
        }
        let fam = if b[0] == b'6' {
            libc::AF_INET6
        } else {
            libc::AF_INET
        };
        let t = libc::socket(fam, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
        let sent = if t < 0 {
            send_fds(sock, &[])
        } else {
            let r = send_fds(sock, &[t]);
            libc::close(t);
            r
        };
        if sent.is_err() {
            libc::_exit(0);
        }
    }
}

unsafe fn write_file(path: &std::ffi::CStr, data: &std::ffi::CStr) -> io::Result<()> {
    let fd = libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let bytes = data.to_bytes();
    let n = libc::write(fd, bytes.as_ptr().cast(), bytes.len());
    libc::close(fd);
    if n != bytes.len() as isize {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[repr(C, align(8))]
struct Cmsg([u8; 64]);

// One byte of payload, with up to three descriptors. Stack only.
unsafe fn send_fds(sock: RawFd, fds: &[RawFd]) -> io::Result<()> {
    let mut byte = *b"F";
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut buf = Cmsg([0; 64]);
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    if !fds.is_empty() {
        let bytes = std::mem::size_of_val(fds) as libc::c_uint;
        msg.msg_control = buf.0.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(bytes) as _;
        let c = libc::CMSG_FIRSTHDR(&msg);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(bytes) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr(), libc::CMSG_DATA(c).cast(), fds.len());
    }
    if libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) != 1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn recv_fds(sock: RawFd, max: usize) -> io::Result<Vec<OwnedFd>> {
    let mut byte = [0u8];
    let mut iov = libc::iovec {
        iov_base: byte.as_mut_ptr().cast(),
        iov_len: 1,
    };
    let mut buf = Cmsg([0; 64]);
    // SAFETY: msg points at live buffers for the call; the kernel writes
    // at most msg_controllen bytes of control data.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = buf.0.as_mut_ptr().cast();
        msg.msg_controllen = buf.0.len() as _;
        let n = libc::recvmsg(sock, &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Err(io::Error::other("the namespace holder closed"));
        }
        let mut out = Vec::new();
        let mut c = libc::CMSG_FIRSTHDR(&msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c).cast::<RawFd>();
                let count = ((*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize)
                    / std::mem::size_of::<RawFd>();
                for i in 0..count {
                    out.push(OwnedFd::from_raw_fd(*data.add(i)));
                }
            }
            c = libc::CMSG_NXTHDR(&msg, c);
        }
        out.truncate(max);
        Ok(out)
    }
}
