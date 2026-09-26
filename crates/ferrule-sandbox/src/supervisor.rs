//! Linux: the Unix-socket allowlist ([`crate::unix`]) as a seccomp
//! user-notification supervisor.
//!
//! The command's seccomp filter turns every `connect(2)` into a
//! notification instead of running it. The child hands the notification
//! fd to ferrule over a socketpair before it execs, and a thread here
//! answers each call: it takes a copy of the command's socket
//! (`pidfd_getfd`), reads the address out of its memory once, decides, and
//! makes the connection itself on that copy — never
//! `SECCOMP_USER_NOTIF_FLAG_CONTINUE`, which would let the kernel re-read
//! an address (or a descriptor number) the command could have swapped
//! since. A pathname socket is opened `O_PATH` first and connected through
//! `/proc/self/fd/N`, so the file that was checked is the file that's
//! reached, symlinks and all. A refusal is `EACCES`.
//!
//! Needs Linux 5.6 (`pidfd_getfd`); the listener's hang-up, which ends a
//! supervisor thread when the command's last process exits, is 5.8.
//! [`probe`] runs the whole path once per process.
//!
//! What it can't see: a process that has left the command's tree (a
//! daemon reparented to init) under Yama `ptrace_scope` ≥ 1 can't be
//! inspected, so its connects fail closed (`EPERM`); the server sees
//! ferrule's pid in `SO_PEERCRED`; `sendto`/`sendmsg` with an address on
//! an unconnected datagram socket isn't a `connect` and isn't checked; and
//! a datagram socket the command made in the workspace is refused, since
//! there's no listener to ask who owns it (list it to allow it).

use crate::unix::{UnixSockets, Verdict};
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

/// `sizeof(struct sockaddr_storage)`: the kernel refuses a longer address.
const MAX_ADDR: usize = 128;

/// A socketpair whose parent end a thread reads notification fds from, one
/// per spawn of the command; the child end goes into the `pre_exec` hook.
/// The thread exits when the child end's last copy closes (the `Command`
/// is dropped).
pub(crate) fn start(allow: Arc<UnixSockets>) -> io::Result<OwnedFd> {
    let mut fds = [0 as RawFd; 2];
    // SAFETY: `fds` has room for the two descriptors.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: two fresh descriptors from socketpair(2).
    let (parent, child) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    std::thread::Builder::new()
        .name("ferrule-unix-sockets".into())
        .spawn(move || accept_listeners(parent, allow))?;
    Ok(child)
}

fn accept_listeners(parent: OwnedFd, allow: Arc<UnixSockets>) {
    loop {
        match recv_listener(&parent) {
            Ok(Some((listener, root))) => {
                let allow = allow.clone();
                let spawned = std::thread::Builder::new()
                    .name("ferrule-unix-supervisor".into())
                    .spawn(move || serve(listener, root, allow));
                if let Err(e) = spawned {
                    // The listener went with the closure: the command's
                    // connects fail rather than hang.
                    tracing::warn!("sandbox: no unix-socket supervisor thread: {e}");
                }
            }
            Ok(None) => return,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => {
                tracing::warn!("sandbox: unix-socket handoff failed: {e}");
                return;
            }
        }
    }
}

/// Called in the child between fork and exec: sends `listener` and the
/// child's pid (the root of the command's process tree) to the parent.
/// Only raw syscalls on the stack.
///
/// # Safety
/// `sock` and `listener` must be open descriptors.
pub(crate) unsafe fn send_listener(sock: RawFd, listener: RawFd) -> io::Result<()> {
    let mut payload = libc::getpid().to_ne_bytes();
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut space = [0u64; 4]; // ≥ CMSG_SPACE(sizeof(int)), aligned
    let mut msg: libc::msghdr = std::mem::zeroed();
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = space.as_mut_ptr().cast();
    msg.msg_controllen = libc::CMSG_SPACE(4) as _;
    let cmsg = libc::CMSG_FIRSTHDR(&msg);
    (*cmsg).cmsg_level = libc::SOL_SOCKET;
    (*cmsg).cmsg_type = libc::SCM_RIGHTS;
    (*cmsg).cmsg_len = libc::CMSG_LEN(4) as _;
    std::ptr::write_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>(), listener);
    if libc::sendmsg(sock, &msg, libc::MSG_NOSIGNAL) < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// The next notification fd and the pid that sent it; `None` at EOF.
fn recv_listener(sock: &OwnedFd) -> io::Result<Option<(OwnedFd, i32)>> {
    let mut payload = [0u8; 4];
    let mut iov = libc::iovec {
        iov_base: payload.as_mut_ptr().cast(),
        iov_len: payload.len(),
    };
    let mut space = [0u64; 4];
    // SAFETY: every pointer in `msg` is to a live local of the right size.
    unsafe {
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = space.as_mut_ptr().cast();
        msg.msg_controllen = std::mem::size_of_val(&space) as _;
        let n = libc::recvmsg(sock.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC);
        if n < 0 {
            return Err(io::Error::last_os_error());
        }
        if n == 0 {
            return Ok(None);
        }
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "no descriptor in the handoff",
            ));
        }
        let fd = std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<RawFd>());
        Ok(Some((
            OwnedFd::from_raw_fd(fd),
            i32::from_ne_bytes(payload),
        )))
    }
}

/// Answers one command's notifications until its last process is gone.
fn serve(listener: OwnedFd, root: i32, allow: Arc<UnixSockets>) {
    let listener = Arc::new(listener);
    loop {
        let mut pfd = libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&mut pfd, 1, -1) } < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if pfd.revents & libc::POLLIN != 0 {
            // SAFETY: the kernel wants a zeroed struct and fills it in.
            let mut notif: libc::seccomp_notif = unsafe { std::mem::zeroed() };
            // SAFETY: `notif` is the ioctl's argument type.
            if unsafe {
                libc::ioctl(
                    listener.as_raw_fd(),
                    libc::SECCOMP_IOCTL_NOTIF_RECV,
                    &mut notif,
                )
            } != 0
            {
                match io::Error::last_os_error().raw_os_error() {
                    // Gone before we got to it, or a signal.
                    Some(libc::ENOENT) | Some(libc::EINTR) => continue,
                    _ => return,
                }
            }
            // A connect can block (a full backlog, a slow TCP handshake):
            // each gets its own thread, so one never holds up the rest.
            let (l, a) = (listener.clone(), allow.clone());
            let spawned = std::thread::Builder::new()
                .name("ferrule-unix-connect".into())
                .spawn(move || answer(&l, &notif, root, &a));
            if spawned.is_err() {
                respond(&listener, notif.id, -libc::EAGAIN);
            }
            continue;
        }
        if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return;
        }
    }
}

fn answer(listener: &OwnedFd, notif: &libc::seccomp_notif, root: i32, allow: &UnixSockets) {
    let error = match connect_for(listener, notif, root, allow) {
        Ok(()) => 0,
        Err(errno) => -errno,
    };
    respond(listener, notif.id, error);
}

fn respond(listener: &OwnedFd, id: u64, error: i32) {
    let resp = libc::seccomp_notif_resp {
        id,
        val: 0,
        error,
        flags: 0,
    };
    // SAFETY: `resp` is the ioctl's argument type. ENOENT (the caller died
    // or was interrupted) needs no handling.
    unsafe {
        libc::ioctl(listener.as_raw_fd(), libc::SECCOMP_IOCTL_NOTIF_SEND, &resp);
    }
}

fn errno() -> i32 {
    io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

/// Makes the connect the command asked for, or says why not (an errno).
fn connect_for(
    listener: &OwnedFd,
    notif: &libc::seccomp_notif,
    root: i32,
    allow: &UnixSockets,
) -> Result<(), i32> {
    // The calling thread: its memory and cwd are the ones the call means.
    let pid = notif.pid as i32;
    // pidfd_open wants the thread group (a thread id is EINVAL before
    // PIDFD_THREAD); the fd table is the group's.
    let tgid = tgid(pid).ok_or(libc::ESRCH)?;
    // SAFETY: plain syscalls; each result is checked before use.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, tgid, 0) };
    if pidfd < 0 {
        return Err(errno());
    }
    // SAFETY: a fresh descriptor.
    let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) };
    let mem = File::open(format!("/proc/{pid}/mem"))
        .map_err(|e| e.raw_os_error().unwrap_or(libc::EPERM))?;
    // Both opened while the notification is still pending, so they're the
    // caller's and not a recycled pid's.
    if !id_valid(listener, notif.id) {
        return Err(libc::EINTR);
    }
    let [fd_arg, addr_ptr, addr_len, ..] = notif.data.args;
    // SAFETY: as above.
    let sock =
        unsafe { libc::syscall(libc::SYS_pidfd_getfd, pidfd.as_raw_fd(), fd_arg as RawFd, 0) };
    if sock < 0 {
        return Err(errno());
    }
    // SAFETY: a fresh descriptor, sharing the command's open socket.
    let sock = unsafe { OwnedFd::from_raw_fd(sock as RawFd) };
    let len = addr_len as usize;
    if len > MAX_ADDR {
        return Err(libc::EINVAL);
    }
    let mut addr = [0u8; MAX_ADDR];
    mem.read_exact_at(&mut addr[..len], addr_ptr)
        .map_err(|_| libc::EFAULT)?;
    let addr = &addr[..len];

    let family = addr.get(..2).map(|f| u16::from_ne_bytes([f[0], f[1]]));
    if domain(&sock) != Some(libc::AF_UNIX) || family != Some(libc::AF_UNIX as u16) || len <= 2 {
        // Not a Unix socket (its family was fixed at socket()), or an
        // address the kernel will refuse on its own.
        return connect_raw(&sock, addr);
    }
    let path = &addr[2..];
    if path[0] == 0 {
        let name = &path[1..];
        if allow.allows_abstract(name) {
            return connect_raw(&sock, addr);
        }
        tracing::warn!(
            "sandbox: refused a connect to the abstract socket @{} (not on the unix-socket allowlist, docs/egress.md)",
            String::from_utf8_lossy(name)
        );
        return Err(libc::EACCES);
    }
    let path = &path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())];
    let target = open_socket(pid, path)?;
    let real = std::fs::read_link(format!("/proc/self/fd/{}", target.0.as_raw_fd()))
        .unwrap_or_else(|_| PathBuf::from(std::ffi::OsStr::from_bytes(path)));
    let ok = match allow.check_path(&real, target.1) {
        Verdict::Allow => true,
        Verdict::IfOwn => listened_by_command(&target.0, root)?,
        Verdict::Deny => false,
    };
    if !ok {
        tracing::warn!(
            "sandbox: refused a connect to {} (not on the unix-socket allowlist, docs/egress.md)",
            real.display()
        );
        return Err(libc::EACCES);
    }
    connect_path(&sock, &target.0)
}

/// The thread group of thread `tid`, from `/proc/<tid>/status`.
fn tgid(tid: i32) -> Option<i32> {
    let status = std::fs::read_to_string(format!("/proc/{tid}/status")).ok()?;
    status
        .lines()
        .find_map(|l| l.strip_prefix("Tgid:"))?
        .trim()
        .parse()
        .ok()
}

fn id_valid(listener: &OwnedFd, id: u64) -> bool {
    // SAFETY: `id` is the ioctl's argument type.
    unsafe {
        libc::ioctl(
            listener.as_raw_fd(),
            libc::SECCOMP_IOCTL_NOTIF_ID_VALID,
            &id,
        ) == 0
    }
}

fn domain(sock: &OwnedFd) -> Option<i32> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `v` and `len` match SO_DOMAIN's int.
    let r = unsafe {
        libc::getsockopt(
            sock.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_DOMAIN,
            (&mut v as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    (r == 0).then_some(v)
}

fn connect_raw(sock: &OwnedFd, addr: &[u8]) -> Result<(), i32> {
    // SAFETY: `addr` is `addr.len()` readable bytes; the kernel copies it.
    let r = unsafe {
        libc::connect(
            sock.as_raw_fd(),
            addr.as_ptr().cast(),
            addr.len() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

/// The socket file at `path` as the command sees it (relative to its cwd),
/// `O_PATH`, with its link count. Symlinks are followed: what's checked is
/// where they lead.
fn open_socket(pid: i32, path: &[u8]) -> Result<(OwnedFd, u64), i32> {
    let mut full = Vec::new();
    if path.first() != Some(&b'/') {
        full.extend_from_slice(format!("/proc/{pid}/cwd/").as_bytes());
    }
    full.extend_from_slice(path);
    let c = CString::new(full).map_err(|_| libc::EINVAL)?;
    // SAFETY: a valid C string.
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(errno());
    }
    // SAFETY: fresh descriptor.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: `st` is written by fstat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(errno());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(libc::ECONNREFUSED);
    }
    Ok((fd, st.st_nlink as u64))
}

fn proc_fd_addr(target: &OwnedFd) -> libc::sockaddr_un {
    // SAFETY: all-zero is a valid sockaddr_un.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path = format!("/proc/self/fd/{}", target.as_raw_fd());
    for (dst, src) in sa.sun_path.iter_mut().zip(path.bytes()) {
        *dst = src as libc::c_char;
    }
    sa
}

/// Connects `sock` to the socket file `target` was opened on.
fn connect_path(sock: &OwnedFd, target: &OwnedFd) -> Result<(), i32> {
    let sa = proc_fd_addr(target);
    // SAFETY: `sa` is a complete sockaddr_un.
    let r = unsafe {
        libc::connect(
            sock.as_raw_fd(),
            (&sa as *const libc::sockaddr_un).cast(),
            std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
        )
    };
    if r == 0 {
        Ok(())
    } else {
        Err(errno())
    }
}

/// Whether the process listening on `target` belongs to the command:
/// `root` itself, a descendant, or in its process group. Asked with a
/// throwaway connection of our own, which the server sees open and close.
fn listened_by_command(target: &OwnedFd, root: i32) -> Result<bool, i32> {
    for kind in [libc::SOCK_STREAM, libc::SOCK_SEQPACKET] {
        // SAFETY: plain syscall.
        let s = unsafe {
            libc::socket(
                libc::AF_UNIX,
                kind | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                0,
            )
        };
        if s < 0 {
            return Err(errno());
        }
        // SAFETY: fresh descriptor.
        let s = unsafe { OwnedFd::from_raw_fd(s) };
        match connect_path(&s, target) {
            Ok(()) => {}
            Err(libc::EPROTOTYPE) => continue,
            // Nobody listening, a full backlog: the command's own connect
            // would get the same.
            Err(e) => return Err(e),
        }
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` match SO_PEERCRED.
        if unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        } != 0
        {
            return Err(errno());
        }
        return Ok(in_tree(cred.pid, root));
    }
    // A datagram socket: there's no peer to ask who owns it.
    Ok(false)
}

/// `pid` is `root`, under it, or in its process group (a shell tool
/// command runs as a group led by `root`).
fn in_tree(mut pid: i32, root: i32) -> bool {
    for _ in 0..256 {
        if pid == root {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        let Some((ppid, pgrp)) = stat_ids(pid) else {
            return false;
        };
        if pgrp == root {
            return true;
        }
        pid = ppid;
    }
    false
}

/// `(ppid, pgrp)` from `/proc/<pid>/stat`, after the parenthesised name
/// (which can itself hold spaces and parens).
fn stat_ids(pid: i32) -> Option<(i32, i32)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace().skip(1); // state
    let ppid = fields.next()?.parse().ok()?;
    let pgrp = fields.next()?.parse().ok()?;
    Some((ppid, pgrp))
}

/// Whether the supervisor works here, tried once per process: a child
/// under the filter must reach an allowed socket and be refused another.
pub(crate) fn probe(abi: u32) -> Result<(), String> {
    static RESULT: OnceLock<Result<(), String>> = OnceLock::new();
    RESULT.get_or_init(|| run_probe(abi)).clone()
}

fn run_probe(abi: u32) -> Result<(), String> {
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    if !crate::linux::SECCOMP_SUPPORTED {
        return Err("the seccomp filter is only built for x86_64 and aarch64".into());
    }
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "ferrule-unix-probe-{}-{}",
        std::process::id(),
        N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).map_err(|e| format!("probe dir: {e}"))?;
    let result = (|| {
        let dir = dunce::canonicalize(&dir).map_err(|e| e.to_string())?;
        let (ok, no) = (dir.join("ok.sock"), dir.join("no.sock"));
        let _l1 = UnixListener::bind(&ok).map_err(|e| format!("probe socket: {e}"))?;
        let _l2 = UnixListener::bind(&no).map_err(|e| format!("probe socket: {e}"))?;
        let allow = UnixSockets {
            files: vec![ok.clone()],
            ..UnixSockets::default()
        };
        let (ok_sa, no_sa) = (sockaddr(&ok)?, sockaddr(&no)?);
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        crate::linux::apply(&mut cmd, abi, true, &[], &[], Some(Arc::new(allow)))
            .map_err(|e| e.to_string())?;
        // SAFETY: raw syscalls on addresses built above.
        unsafe {
            cmd.pre_exec(move || {
                let try_connect = |sa: &libc::sockaddr_un| {
                    let s = libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                    if s < 0 {
                        return Err(io::Error::last_os_error());
                    }
                    let r = libc::connect(
                        s,
                        (sa as *const libc::sockaddr_un).cast(),
                        std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t,
                    );
                    let err = io::Error::last_os_error();
                    libc::close(s);
                    if r == 0 {
                        Ok(())
                    } else {
                        Err(err)
                    }
                };
                try_connect(&ok_sa)?;
                match try_connect(&no_sa) {
                    Err(e) if e.raw_os_error() == Some(libc::EACCES) => Ok(()),
                    // Distinct, so the parent can say what went wrong.
                    _ => Err(io::Error::from_raw_os_error(libc::ENOTRECOVERABLE)),
                }
            });
        }
        match cmd.status() {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("probe command exited with {s}")),
            Err(e) if e.raw_os_error() == Some(libc::ENOTRECOVERABLE) => {
                Err("a socket off the allowlist was not refused".into())
            }
            Err(e) if e.raw_os_error() == Some(libc::EPERM) => Err(
                "the supervisor may not take the command's socket (pidfd_getfd was refused: \
                 inside a container, Docker's default seccomp profile does this without \
                 CAP_SYS_PTRACE)"
                    .into(),
            ),
            Err(e) => Err(format!(
                "the supervisor didn't answer ({e}); it needs Linux 5.6+ and ptrace access \
                 to the command"
            )),
        }
    })();
    let _ = std::fs::remove_dir_all(&dir);
    result
}

fn sockaddr(path: &std::path::Path) -> Result<libc::sockaddr_un, String> {
    // SAFETY: all-zero is a valid sockaddr_un.
    let mut sa: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let bytes = path.as_os_str().as_bytes();
    if bytes.len() >= sa.sun_path.len() {
        return Err(format!(
            "{} is too long for a socket address",
            path.display()
        ));
    }
    sa.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, &src) in sa.sun_path.iter_mut().zip(bytes) {
        *dst = src as libc::c_char;
    }
    Ok(sa)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_ids_survive_a_name_with_parens() {
        let me = std::process::id() as i32;
        let (ppid, _) = stat_ids(me).unwrap();
        assert!(ppid > 0);
        assert!(in_tree(me, me));
        assert!(!in_tree(1, me));
        assert_eq!(tgid(me), Some(me));
        // A test runs on a thread of its own: its id isn't the group's.
        let tid = unsafe { libc::gettid() };
        assert_eq!(tgid(tid), Some(me));
    }
}
