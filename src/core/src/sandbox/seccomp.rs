//! The classic-BPF seccomp filter that bubblewrap installs with `--seccomp 3`.
//!
//! It closes local IPC that the mount view alone can't: a sandboxed command can't create an
//! `AF_UNIX` socket with `socket()` (so it can't connect to the session bus, docker.sock or an
//! abstract socket on a shared network namespace), and can't use io_uring, whose operations
//! bypass seccomp.
//!
//! Rules, in order:
//! - a syscall from any architecture other than the native one (i386 on x86-64, arm32 on arm64)
//!   kills the process, since its numbers mean different syscalls;
//! - on x86-64, any x32 syscall (`nr & 0x4000_0000`) kills the process, for the same reason
//!   (this includes nr -1, which only a ptrace tracer sets to skip a syscall);
//! - `socket(AF_UNIX, …)` fails with `EPERM`, other socket families are allowed;
//! - `io_uring_setup`, `io_uring_enter` and `io_uring_register` fail with `ENOSYS`, which programs
//!   treat as "no io_uring here" and fall back;
//! - everything else is allowed. `socketpair` stays allowed, because many programs need it. Its
//!   stream and seqpacket sockets are already connected and can't connect anywhere else, but a
//!   `SOCK_DGRAM` pair can still `sendto()` any datagram socket the sandbox can name: a pathname
//!   one inside the project, or (only with the network on) an abstract one on the host.
//!
//! Only x86-64 and arm64 are supported. On any other architecture [`program`] returns `None` and
//! the Linux sandbox is unavailable there.
#![allow(dead_code)] // Used by D5 (`prepare` and the probe).

use libc::sock_filter;

// uapi/linux/audit.h: AUDIT_ARCH_* = EM_* | __AUDIT_ARCH_64BIT (for 64-bit ABIs) | __AUDIT_ARCH_LE.
// The libc crate has the EM_* values but not these.
const AUDIT_ARCH_64BIT: u32 = 0x8000_0000;
const AUDIT_ARCH_LE: u32 = 0x4000_0000;
pub(crate) const AUDIT_ARCH_X86_64: u32 = libc::EM_X86_64 as u32 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE;
pub(crate) const AUDIT_ARCH_AARCH64: u32 =
    libc::EM_AARCH64 as u32 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE;
pub(crate) const AUDIT_ARCH_I386: u32 = libc::EM_386 as u32 | AUDIT_ARCH_LE;
pub(crate) const AUDIT_ARCH_ARM: u32 = libc::EM_ARM as u32 | AUDIT_ARCH_LE;

/// The audit architecture of the syscalls this build makes, or `None` where the sandbox is unsupported.
pub(crate) const NATIVE_ARCH: Option<u32> = if cfg!(target_arch = "x86_64") {
    Some(AUDIT_ARCH_X86_64)
} else if cfg!(all(target_arch = "aarch64", target_endian = "little")) {
    Some(AUDIT_ARCH_AARCH64)
} else {
    None
};

/// Marks an x32 syscall number on x86-64.
pub(crate) const X32_SYSCALL_BIT: u32 = 0x4000_0000;

// struct seccomp_data { int nr; __u32 arch; __u64 instruction_pointer; __u64 args[6]; }
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;
/// The low 32 bits of args[0]. The kernel reads `socket`'s domain as an `int`, so the high bits
/// are ignored there and must be ignored here too.
const OFFSET_ARG0_LOW: u32 = 16 + if cfg!(target_endian = "big") { 4 } else { 0 };

/// The filter for this build's architecture, or `None` where the Linux sandbox is unsupported.
pub(crate) fn program() -> Option<Vec<sock_filter>> {
    NATIVE_ARCH.map(|arch| build(arch, cfg!(target_arch = "x86_64")))
}

/// Layout (x86-64; arm64 has no x32 check):
/// ```text
///  0  ld  arch          ; native? else KILL
///  1  jeq native
///  2  ld  nr
///  3  jset X32_BIT      ; KILL
///  4  jeq socket        ; to SOCKET
///  5  jeq io_uring_setup / 6 enter / 7 register ; to ENOSYS
///  8  ret ALLOW
///  9  SOCKET: ld args[0] (low 32 bits)
/// 10  jeq AF_UNIX       ; EPERM else ALLOW
/// 11  ret ERRNO(EPERM)  / 12 ret ERRNO(ENOSYS) / 13 ret KILL_PROCESS / 14 ret ALLOW
/// ```
fn build(native: u32, x86_64: bool) -> Vec<sock_filter> {
    let uring = [
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
    ];
    let socket = 3 + usize::from(x86_64) + 1 + uring.len() + 1;
    let eperm = socket + 2;
    let (enosys, kill, allow) = (eperm + 1, eperm + 2, eperm + 3);

    let mut p = Vec::with_capacity(allow + 1);
    p.push(load(OFFSET_ARCH));
    p.push(jump(libc::BPF_JEQ, native, p.len() + 1, kill, p.len()));
    p.push(load(OFFSET_NR));
    if x86_64 {
        p.push(jump(
            libc::BPF_JSET,
            X32_SYSCALL_BIT,
            kill,
            p.len() + 1,
            p.len(),
        ));
    }
    p.push(jump(
        libc::BPF_JEQ,
        libc::SYS_socket as u32,
        socket,
        p.len() + 1,
        p.len(),
    ));
    for nr in uring {
        p.push(jump(libc::BPF_JEQ, nr as u32, enosys, p.len() + 1, p.len()));
    }
    p.push(ret(libc::SECCOMP_RET_ALLOW));
    debug_assert_eq!(p.len(), socket);
    p.push(load(OFFSET_ARG0_LOW));
    p.push(jump(
        libc::BPF_JEQ,
        libc::AF_UNIX as u32,
        eperm,
        allow,
        p.len(),
    ));
    p.push(ret(errno(libc::EPERM)));
    p.push(ret(errno(libc::ENOSYS)));
    p.push(ret(libc::SECCOMP_RET_KILL_PROCESS));
    p.push(ret(libc::SECCOMP_RET_ALLOW));
    debug_assert_eq!(p.len(), allow + 1);
    p
}

fn instruction(code: u32, k: u32, jt: u8, jf: u8) -> sock_filter {
    sock_filter {
        // Every BPF opcode fits in 16 bits; the libc constants are u32 only for convenience.
        code: code as u16,
        jt,
        jf,
        k,
    }
}
fn load(offset: u32) -> sock_filter {
    instruction(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, offset, 0, 0)
}
fn ret(value: u32) -> sock_filter {
    instruction(libc::BPF_RET | libc::BPF_K, value, 0, 0)
}
/// A conditional jump at index `at` to the absolute indices `jt`/`jf`. BPF offsets are relative
/// to the next instruction and only jump forward; this program is far shorter than the 255 a
/// jump can reach.
fn jump(op: u32, k: u32, jt: usize, jf: usize, at: usize) -> sock_filter {
    let offset = |to: usize| (to - at - 1) as u8;
    instruction(libc::BPF_JMP | op | libc::BPF_K, k, offset(jt), offset(jf))
}

fn errno(code: i32) -> u32 {
    libc::SECCOMP_RET_ERRNO | (code as u32 & libc::SECCOMP_RET_DATA)
}

/// The bytes bubblewrap reads from the `--seccomp` fd: each instruction as native-endian
/// `{u16 code, u8 jt, u8 jf, u32 k}`, the in-memory layout of `struct sock_filter`.
pub(crate) fn to_bytes(program: &[sock_filter]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(program.len() * 8);
    for instruction in program {
        bytes.extend_from_slice(&instruction.code.to_ne_bytes());
        bytes.push(instruction.jt);
        bytes.push(instruction.jf);
        bytes.extend_from_slice(&instruction.k.to_ne_bytes());
    }
    bytes
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use libc::{SECCOMP_RET_ALLOW as ALLOW, SECCOMP_RET_KILL_PROCESS as KILL};

    const NATIVE: u32 = match NATIVE_ARCH {
        Some(arch) => arch,
        None => 0,
    };
    fn eperm() -> u32 {
        libc::SECCOMP_RET_ERRNO | libc::EPERM as u32
    }
    fn enosys() -> u32 {
        libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32
    }

    /// `struct seccomp_data` as the kernel lays it out for this machine.
    fn data(arch: u32, nr: u32, args: &[u64]) -> [u8; 64] {
        let mut d = [0u8; 64];
        d[0..4].copy_from_slice(&nr.to_ne_bytes());
        d[4..8].copy_from_slice(&arch.to_ne_bytes());
        d[8..16].copy_from_slice(&0x7f00_dead_beefu64.to_ne_bytes());
        for (i, arg) in args.iter().enumerate() {
            d[16 + 8 * i..24 + 8 * i].copy_from_slice(&arg.to_ne_bytes());
        }
        d
    }

    /// A tiny classic-BPF interpreter: the subset the kernel's seccomp checker allows that this
    /// filter could use. Anything else (or a load or jump the kernel would reject) panics.
    fn run(program: &[sock_filter], data: &[u8; 64]) -> u32 {
        assert!(!program.is_empty() && program.len() <= 4096);
        let mut a: u32 = 0;
        let mut pc = 0usize;
        loop {
            let i = program.get(pc).expect("ran off the end of the program");
            let code = u32::from(i.code);
            pc += 1;
            let mut branch = |taken: bool| {
                pc += usize::from(if taken { i.jt } else { i.jf });
                assert!(pc < program.len(), "jump out of the program");
            };
            match code {
                c if c == libc::BPF_LD | libc::BPF_W | libc::BPF_ABS => {
                    let k = i.k as usize;
                    assert!(
                        k.is_multiple_of(4) && k + 4 <= data.len(),
                        "bad load at {k}"
                    );
                    a = u32::from_ne_bytes(data[k..k + 4].try_into().unwrap());
                }
                c if c == libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K => branch(a == i.k),
                c if c == libc::BPF_JMP | libc::BPF_JSET | libc::BPF_K => branch(a & i.k != 0),
                c if c == libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K => branch(a >= i.k),
                c if c == libc::BPF_JMP | libc::BPF_JGT | libc::BPF_K => branch(a > i.k),
                c if c == libc::BPF_RET | libc::BPF_K => return i.k,
                other => panic!("unsupported opcode {other:#x}"),
            }
        }
    }

    fn eval(program: &[sock_filter], arch: u32, nr: impl TryInto<u32>, args: &[u64]) -> u32 {
        let nr = nr.try_into().ok().unwrap();
        run(program, &data(arch, nr, args))
    }

    #[test]
    fn audit_arch_values_match_the_kernel() {
        assert_eq!(AUDIT_ARCH_X86_64, 0xC000_003E);
        assert_eq!(AUDIT_ARCH_AARCH64, 0xC000_00B7);
        assert_eq!(AUDIT_ARCH_I386, 0x4000_0003);
        assert_eq!(AUDIT_ARCH_ARM, 0x4000_0028);
        #[cfg(target_arch = "x86_64")]
        assert_eq!(NATIVE_ARCH, Some(AUDIT_ARCH_X86_64));
        #[cfg(target_arch = "aarch64")]
        assert_eq!(NATIVE_ARCH, Some(AUDIT_ARCH_AARCH64));
    }

    #[test]
    fn seccomp_blocks_af_unix_io_uring_x32_and_foreign_arch() {
        let p = program().unwrap();
        let unix = libc::AF_UNIX as u64;
        assert_eq!(eval(&p, NATIVE, libc::SYS_socket, &[unix]), eperm());
        // The kernel reads the domain as an int, so high bits must not hide AF_UNIX.
        assert_eq!(
            eval(&p, NATIVE, libc::SYS_socket, &[1 << 32 | unix]),
            eperm()
        );
        assert_eq!(
            eval(&p, NATIVE, libc::SYS_socket, &[libc::AF_INET as u64]),
            ALLOW
        );
        assert_eq!(
            eval(&p, NATIVE, libc::SYS_socket, &[libc::AF_INET6 as u64]),
            ALLOW
        );
        assert_eq!(
            eval(&p, NATIVE, libc::SYS_socket, &[libc::AF_NETLINK as u64]),
            ALLOW
        );
        for nr in [
            libc::SYS_io_uring_setup,
            libc::SYS_io_uring_enter,
            libc::SYS_io_uring_register,
        ] {
            assert_eq!(eval(&p, NATIVE, nr, &[0]), enosys());
        }
        assert_eq!(eval(&p, NATIVE, libc::SYS_socketpair, &[unix]), ALLOW);
        assert_eq!(eval(&p, NATIVE, libc::SYS_read, &[0]), ALLOW);
        // i386 socket() is 359 and socketcall() is 102; arm32 socket() is 281. Every syscall from
        // a foreign ABI dies, whatever its number.
        let other_64_bit = AUDIT_ARCH_X86_64 ^ AUDIT_ARCH_AARCH64 ^ NATIVE;
        for arch in [AUDIT_ARCH_I386, AUDIT_ARCH_ARM, other_64_bit] {
            for nr in [359u32, 102, 281, 0, libc::SYS_read as u32] {
                assert_eq!(eval(&p, arch, nr, &[unix]), KILL, "arch {arch:#x} nr {nr}");
            }
        }
        #[cfg(target_arch = "x86_64")]
        assert_eq!(eval(&p, NATIVE, X32_SYSCALL_BIT | 41, &[unix]), KILL);
    }

    #[test]
    fn x86_64_layout_kills_every_x32_syscall() {
        // Checked on every build host, not only x86-64 ones. The syscall numbers are this host's,
        // which is fine: this test is about the x32 check and the jump layout around it.
        let p = build(AUDIT_ARCH_X86_64, true);
        let unix = libc::AF_UNIX as u64;
        for nr in [0u32, 41, 53, 425, 0x3fff_ffff] {
            assert_eq!(
                eval(&p, AUDIT_ARCH_X86_64, X32_SYSCALL_BIT | nr, &[unix]),
                KILL
            );
        }
        assert_eq!(eval(&p, AUDIT_ARCH_X86_64, u32::MAX, &[]), KILL);
        assert_eq!(
            eval(&p, AUDIT_ARCH_X86_64, libc::SYS_socket, &[unix]),
            eperm()
        );
        assert_eq!(
            eval(&p, AUDIT_ARCH_X86_64, libc::SYS_io_uring_enter, &[]),
            enosys()
        );
        assert_eq!(
            eval(&p, AUDIT_ARCH_X86_64, libc::SYS_socketpair, &[unix]),
            ALLOW
        );
        assert_eq!(eval(&p, AUDIT_ARCH_I386, 359, &[unix]), KILL);
        assert_eq!(
            eval(&p, AUDIT_ARCH_AARCH64, libc::SYS_socket, &[unix]),
            KILL
        );
        // The arm64 layout has no x32 check but the same rules otherwise.
        let p = build(AUDIT_ARCH_AARCH64, false);
        assert_eq!(p.len(), build(AUDIT_ARCH_X86_64, true).len() - 1);
        assert_eq!(
            eval(&p, AUDIT_ARCH_AARCH64, libc::SYS_socket, &[unix]),
            eperm()
        );
        assert_eq!(eval(&p, AUDIT_ARCH_ARM, 281, &[unix]), KILL);
    }

    #[test]
    fn to_bytes_is_the_native_sock_filter_layout() {
        let p = program().unwrap();
        let bytes = to_bytes(&p);
        assert_eq!(bytes.len(), p.len() * 8);
        for (i, chunk) in p.iter().zip(bytes.chunks_exact(8)) {
            assert_eq!(u16::from_ne_bytes([chunk[0], chunk[1]]), i.code);
            assert_eq!((chunk[2], chunk[3]), (i.jt, i.jf));
            assert_eq!(u32::from_ne_bytes(chunk[4..8].try_into().unwrap()), i.k);
        }
        // The first instruction loads the architecture word: BPF_LD|BPF_W|BPF_ABS, k = 4.
        #[cfg(target_endian = "little")]
        assert_eq!(&bytes[..8], &[0x20, 0, 0, 0, 4, 0, 0, 0]);
    }
}
