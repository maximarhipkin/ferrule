//! Linux backend: Landlock for the filesystem, seccomp for the network.
//!
//! Everything that allocates or can fail interestingly — opening the rule
//! paths, building the ruleset, assembling the BPF program — happens in the
//! parent. The `pre_exec` hook, which runs between fork and exec where only
//! async-signal-safe calls are allowed, makes exactly three syscalls:
//! `PR_SET_NO_NEW_PRIVS` (required by both mechanisms without
//! CAP_SYS_ADMIN), the optional seccomp filter, and
//! `landlock_restrict_self`.
//!
//! libc doesn't carry the Landlock ABI, so the syscall numbers, structs and
//! access bits are spelled out from `include/uapi/linux/landlock.h`.

use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

// Same numbers on every architecture: Landlock postdates the syscall table split.
const SYS_LANDLOCK_CREATE_RULESET: libc::c_long = 444;
const SYS_LANDLOCK_ADD_RULE: libc::c_long = 445;
const SYS_LANDLOCK_RESTRICT_SELF: libc::c_long = 446;
const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

const EXECUTE: u64 = 1 << 0;
const WRITE_FILE: u64 = 1 << 1;
const READ_FILE: u64 = 1 << 2;
const READ_DIR: u64 = 1 << 3;
// 1<<4 REMOVE_DIR … 1<<12 MAKE_SYM: ABI 1's directory-only rights.
const ABI1_ALL: u64 = (1 << 13) - 1;
const REFER: u64 = 1 << 13; // ABI 2
const TRUNCATE: u64 = 1 << 14; // ABI 3
const IOCTL_DEV: u64 = 1 << 15; // ABI 5

/// A rule on a non-directory may only grant these; anything else is EINVAL.
const FILE_RIGHTS: u64 = EXECUTE | WRITE_FILE | READ_FILE | TRUNCATE | IOCTL_DEV;

#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// The kernel's Landlock ABI version, or a sentence on why it has none.
pub fn abi() -> Result<u32, String> {
    // SAFETY: the version query takes no pointer and returns an int.
    let r = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        )
    };
    if r >= 1 {
        return Ok(r as u32);
    }
    let err = io::Error::last_os_error();
    Err(match err.raw_os_error() {
        Some(libc::ENOSYS) => "the kernel has no Landlock (needs Linux 5.13+ with CONFIG_SECURITY_LANDLOCK)".into(),
        Some(libc::EOPNOTSUPP) => "Landlock is built in but disabled at boot (add `landlock` to the `lsm=` kernel parameter)".into(),
        _ => format!("Landlock ABI probe failed: {err}"),
    })
}

/// Every filesystem right this ABI knows. Handling all of them is what makes
/// the ruleset deny-by-default: an unhandled right is simply not enforced.
fn handled_fs(abi: u32) -> u64 {
    let mut rights = ABI1_ALL;
    if abi >= 2 {
        rights |= REFER;
    }
    if abi >= 3 {
        rights |= TRUNCATE;
    }
    if abi >= 5 {
        rights |= IOCTL_DEV;
    }
    rights
}

fn ruleset(abi: u32, writable: &[PathBuf]) -> io::Result<OwnedFd> {
    let handled = handled_fs(abi);
    let attr = RulesetAttr {
        handled_access_fs: handled,
        handled_access_net: 0,
        scoped: 0,
    };
    // Pass only the fields this ABI has; the rest are zero either way.
    let size: usize = match abi {
        1..=3 => 8,
        4 | 5 => 16,
        _ => 24,
    };
    // SAFETY: `attr` outlives the call and `size` never exceeds it.
    let fd = unsafe {
        libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const RulesetAttr,
            size,
            0u32,
        )
    };
    if fd < 0 {
        return Err(annotate(
            io::Error::last_os_error(),
            "landlock_create_ruleset",
        ));
    }
    // SAFETY: a fresh descriptor the kernel just handed us (already O_CLOEXEC).
    let ruleset = unsafe { OwnedFd::from_raw_fd(fd as i32) };

    // Reads and exec everywhere: the agent needs compilers, interpreters, libs.
    allow(&ruleset, Path::new("/"), EXECUTE | READ_FILE | READ_DIR)?;
    // `cmd > /dev/null` opens with O_TRUNC, hence TRUNCATE.
    allow(
        &ruleset,
        Path::new("/dev/null"),
        (READ_FILE | WRITE_FILE | TRUNCATE | IOCTL_DEV) & handled,
    )?;
    for root in writable {
        allow(&ruleset, root, handled)?;
    }
    Ok(ruleset)
}

fn allow(ruleset: &OwnedFd, path: &Path, mut access: u64) -> io::Result<()> {
    if !path.is_dir() {
        access &= FILE_RIGHTS;
    }
    let c_path = CString::new(path.as_os_str().as_bytes())?;
    // SAFETY: valid C string; O_PATH opens without needing read permission.
    let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(annotate(
            io::Error::last_os_error(),
            &path.display().to_string(),
        ));
    }
    // SAFETY: fresh descriptor from open(2).
    let parent = unsafe { OwnedFd::from_raw_fd(fd) };
    let rule = PathBeneathAttr {
        allowed_access: access,
        parent_fd: parent.as_raw_fd(),
    };
    // SAFETY: `rule` and both descriptors outlive the call.
    let r = unsafe {
        libc::syscall(
            SYS_LANDLOCK_ADD_RULE,
            ruleset.as_raw_fd(),
            LANDLOCK_RULE_PATH_BENEATH,
            &rule as *const PathBeneathAttr,
            0u32,
        )
    };
    if r != 0 {
        return Err(annotate(
            io::Error::last_os_error(),
            &format!("landlock_add_rule {}", path.display()),
        ));
    }
    Ok(())
}

fn annotate(err: io::Error, what: &str) -> io::Error {
    io::Error::new(err.kind(), format!("{what}: {err}"))
}

/// Arrange for `cmd` to confine itself to `writable` (plus, when `network`
/// is false, no sockets but Unix ones) right before it execs.
pub fn apply(cmd: &mut Command, abi: u32, network: bool, writable: &[PathBuf]) -> io::Result<()> {
    let ruleset = ruleset(abi, writable)?;
    let filter = match (network, seccomp::deny_network()) {
        (true, _) => None,
        (false, Some(f)) => Some(f),
        (false, None) => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "no seccomp filter for this architecture",
            ))
        }
    };
    // SAFETY: the hook only makes raw syscalls on data prepared above — no
    // allocation, no locks — which is what a post-fork child may do.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
                0 as libc::c_ulong,
            ) != 0
            {
                return Err(io::Error::last_os_error());
            }
            if let Some(filter) = &filter {
                let prog = libc::sock_fprog {
                    len: filter.len() as u16,
                    filter: filter.as_ptr() as *mut libc::sock_filter,
                };
                if libc::prctl(
                    libc::PR_SET_SECCOMP,
                    libc::SECCOMP_MODE_FILTER as libc::c_ulong,
                    &prog as *const libc::sock_fprog,
                ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset.as_raw_fd(), 0u32) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

pub const SECCOMP_SUPPORTED: bool = cfg!(any(target_arch = "x86_64", target_arch = "aarch64"));

/// The network filter: kill on a foreign syscall ABI, refuse `socket()` for
/// every domain but AF_UNIX (local IPC keeps working), and refuse
/// `io_uring_setup`, whose rings can open sockets without a `socket()` call.
mod seccomp {
    #[cfg(target_arch = "x86_64")]
    pub(super) const ARCH: u32 = 0xC000_003E; // AUDIT_ARCH_X86_64
    #[cfg(target_arch = "x86_64")]
    pub(super) const SYS_SOCKET: u32 = 41;
    #[cfg(target_arch = "aarch64")]
    pub(super) const ARCH: u32 = 0xC000_00B7; // AUDIT_ARCH_AARCH64
    #[cfg(target_arch = "aarch64")]
    pub(super) const SYS_SOCKET: u32 = 198;
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub(super) const SYS_IO_URING_SETUP: u32 = 425;

    // struct seccomp_data { int nr; u32 arch; u64 ip; u64 args[6]; }
    pub(super) const OFF_NR: u32 = 0;
    pub(super) const OFF_ARCH: u32 = 4;
    pub(super) const OFF_ARG0: u32 = 16; // low half on little-endian
    /// x32 syscalls reuse x86_64 numbers with this bit set; the arch field
    /// can't tell them apart, so they're refused wholesale.
    pub(super) const X32_BIT: u32 = 0x4000_0000;

    fn stmt(code: u32, k: u32) -> libc::sock_filter {
        libc::sock_filter {
            code: code as u16,
            jt: 0,
            jf: 0,
            k,
        }
    }

    fn jump(code: u32, k: u32) -> libc::sock_filter {
        stmt(libc::BPF_JMP | code | libc::BPF_K, k)
    }

    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    pub fn deny_network() -> Option<Vec<libc::sock_filter>> {
        use libc::{BPF_ABS, BPF_JEQ, BPF_JGE, BPF_K, BPF_LD, BPF_RET, BPF_W};
        let load = |off| stmt(BPF_LD | BPF_W | BPF_ABS, off);
        let mut p = vec![
            load(OFF_ARCH),
            jump(BPF_JEQ, ARCH),
            stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_KILL_PROCESS),
            load(OFF_NR),
        ];
        p[1].jt = 1; // arch matches: skip the kill
        let x32 = cfg!(target_arch = "x86_64").then(|| {
            p.push(jump(BPF_JGE, X32_BIT));
            p.len() - 1
        });
        let socket = p.len();
        p.push(jump(BPF_JEQ, SYS_SOCKET));
        p.push(load(OFF_ARG0));
        let domain = p.len();
        p.push(jump(BPF_JEQ, libc::AF_UNIX as u32));
        let uring = p.len();
        p.push(jump(BPF_JEQ, SYS_IO_URING_SETUP));
        let allow = p.len();
        p.push(stmt(BPF_RET | BPF_K, libc::SECCOMP_RET_ALLOW));
        let deny = p.len();
        p.push(stmt(
            BPF_RET | BPF_K,
            libc::SECCOMP_RET_ERRNO | libc::EPERM as u32,
        ));

        // Jump offsets count instructions after the jump itself.
        let to = |from: usize, target: usize| (target - from - 1) as u8;
        if let Some(i) = x32 {
            p[i].jt = to(i, deny);
        }
        p[socket].jf = to(socket, uring); // not socket(): A still holds nr
        p[domain].jt = to(domain, allow);
        p[domain].jf = to(domain, deny);
        p[uring].jt = to(uring, deny);
        p[uring].jf = to(uring, allow);
        Some(p)
    }

    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    pub fn deny_network() -> Option<Vec<libc::sock_filter>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny classic-BPF interpreter covering the opcodes the filter uses,
    /// so the jump arithmetic is checked on every build, not only where the
    /// kernel test can run.
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn run(prog: &[libc::sock_filter], arch: u32, nr: u32, arg0: u32) -> u32 {
        let (mut pc, mut a) = (0usize, 0u32);
        loop {
            let ins = prog[pc];
            let code = ins.code as u32;
            pc += 1;
            match code {
                c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    a = match ins.k {
                        seccomp::OFF_NR => nr,
                        seccomp::OFF_ARCH => arch,
                        seccomp::OFF_ARG0 => arg0,
                        other => panic!("unexpected load offset {other}"),
                    }
                }
                c if c == libc::BPF_RET | libc::BPF_K => return ins.k,
                c if c == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => {
                    pc += (if a == ins.k { ins.jt } else { ins.jf }) as usize
                }
                c if c == libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K => {
                    pc += (if a >= ins.k { ins.jt } else { ins.jf }) as usize
                }
                other => panic!("unexpected opcode {other:#x}"),
            }
        }
    }

    #[test]
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    fn network_filter_decisions() {
        let prog = seccomp::deny_network().unwrap();
        let arch = seccomp::ARCH;
        let deny = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
        let socket = seccomp::SYS_SOCKET;
        assert_eq!(
            run(&prog, arch, socket, libc::AF_UNIX as u32),
            libc::SECCOMP_RET_ALLOW
        );
        assert_eq!(run(&prog, arch, socket, libc::AF_INET as u32), deny);
        assert_eq!(run(&prog, arch, socket, libc::AF_INET6 as u32), deny);
        assert_eq!(run(&prog, arch, socket, libc::AF_NETLINK as u32), deny);
        assert_eq!(run(&prog, arch, seccomp::SYS_IO_URING_SETUP, 0), deny);
        assert_eq!(
            run(&prog, arch, 0, 0),
            libc::SECCOMP_RET_ALLOW,
            "read(2) passes"
        );
        assert_eq!(
            run(&prog, arch, 1, libc::AF_INET as u32),
            libc::SECCOMP_RET_ALLOW,
            "only socket() looks at arg0"
        );
        assert_eq!(
            run(&prog, 0x4000_0003, socket, libc::AF_UNIX as u32),
            libc::SECCOMP_RET_KILL_PROCESS,
            "foreign arch"
        );
        if cfg!(target_arch = "x86_64") {
            assert_eq!(
                run(&prog, arch, seccomp::X32_BIT | 41, libc::AF_UNIX as u32),
                deny,
                "x32 ABI"
            );
        }
    }

    #[test]
    fn handled_rights_grow_with_the_abi() {
        assert_eq!(handled_fs(1), ABI1_ALL);
        assert_eq!(handled_fs(2) & REFER, REFER);
        assert_eq!(handled_fs(4) & (TRUNCATE | IOCTL_DEV), TRUNCATE);
        assert_eq!(handled_fs(8), ABI1_ALL | REFER | TRUNCATE | IOCTL_DEV);
    }
}
