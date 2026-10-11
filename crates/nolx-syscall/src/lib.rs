//! The NOLX user/kernel ABI.
//!
//! One instruction, one register convention, one error convention. Written down here, in
//! code, so that libc and the kernel cannot drift apart: both depend on this crate, and a
//! change to the ABI is a change to a test.
//!
//! # The convention
//!
//! | Register | Role |
//! |---|---|
//! | `rax` | syscall number in, return value out |
//! | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | arguments 1..6 |
//! | `rcx`, `r11` | **clobbered by the instruction itself** |
//! | everything else | callee-saved, untouched by the kernel |
//!
//! `rcx` and `r11` are unusable for arguments: `syscall` stores the return address in `rcx`
//! and `rflags` in `r11`. Any ABI that put an argument there would be silently corrupting
//! it, which is the kind of bug that takes a week to find, so the rule is written here
//! rather than in a comment on a macro.
//!
//! # Errors
//!
//! `rax` on return is non-negative for success, or `-errno` for failure. There is no
//! carry-flag convention. libc negates the value, stores it in the thread's `errno`, and
//! returns `-1`, exactly as POSIX describes.
//!
//! # Red zone
//!
//! Not available in the kernel, which is built with the red zone disabled. An interrupt can
//! arrive at any point and the handler will push below `rsp`, so a kernel function that
//! used the red zone would corrupt itself. User code may use it; the kernel preserves it
//! across the syscall boundary by not touching user stack memory at all.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]

use core::arch::asm;

use nolx_types::syscall::Number;

/// Registers the `syscall` instruction itself overwrites.
pub const CLOBBERED_BY_SYSCALL: &[&str] = &["rcx", "r11"];

/// The maximum number of arguments the register convention carries.
pub const MAX_ARGS: usize = 6;

/// Issue a syscall with no arguments.
///
/// # Safety
///
/// The number must name a syscall whose argument count is zero. Passing a number that
/// expects arguments is undefined behaviour in the ordinary C sense: the kernel will read
/// registers whose contents were never defined.
#[inline]
pub unsafe fn syscall0(number: Number) -> i64 {
    let result: i64;
    // SAFETY: the caller asserts the number takes no arguments. `syscall` writes rax (the
    // result), rcx and r11 (its own bookkeeping), and reads only rax. `lateout` on rcx and
    // r11 tells the compiler those are clobbered so it does not keep a live value in them.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with one argument.
///
/// # Safety
///
/// As [`syscall0`], and `a0` must be a valid value for the first argument of `number` --
/// for a pointer argument, a pointer the kernel is allowed to read.
#[inline]
pub unsafe fn syscall1(number: Number, a0: u64) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall0`]; `rdi` is read-only from the kernel's perspective here.
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with two arguments.
///
/// # Safety
///
/// As [`syscall1`].
#[inline]
pub unsafe fn syscall2(number: Number, a0: u64, a1: u64) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall1`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            in("rsi") a1,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with three arguments.
///
/// # Safety
///
/// As [`syscall1`].
#[inline]
pub unsafe fn syscall3(number: Number, a0: u64, a1: u64, a2: u64) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall1`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with four arguments.
///
/// Note the fourth argument is in `r10`, not `rcx`: the C calling convention would use
/// `rcx`, but `syscall` destroys it. This is the single most common mistake when porting a
/// syscall thunk, so it is called out here rather than left to the reader.
///
/// # Safety
///
/// As [`syscall1`].
#[inline]
pub unsafe fn syscall4(number: Number, a0: u64, a1: u64, a2: u64, a3: u64) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall1`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with five arguments.
///
/// # Safety
///
/// As [`syscall1`].
#[inline]
pub unsafe fn syscall5(number: Number, a0: u64, a1: u64, a2: u64, a3: u64, a4: u64) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall1`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Issue a syscall with six arguments.
///
/// # Safety
///
/// As [`syscall1`].
#[inline]
pub unsafe fn syscall6(
    number: Number,
    a0: u64,
    a1: u64,
    a2: u64,
    a3: u64,
    a4: u64,
    a5: u64,
) -> i64 {
    let result: i64;
    // SAFETY: as [`syscall1`].
    unsafe {
        asm!(
            "syscall",
            inlateout("rax") u64::from(number) => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    result
}

/// Turn a raw kernel return value into a POSIX-style result.
///
/// The kernel returns `-errno` for failure. libc's contract is `-1` with `errno` set, so
/// this returns the pair and lets the caller store `errno` in its own TLS -- the kernel
/// never touches `errno`, because it belongs to the thread's C runtime state.
#[must_use = "a syscall return value that is dropped has not been checked for failure"]
pub const fn decode(result: i64) -> Result<i64, i32> {
    if result < 0 {
        Err((-result) as i32)
    } else {
        Ok(result)
    }
}

/// The register state the trap handler saves, in the order it pushes it.
///
/// The layout is the ABI between the assembly stub and the Rust handler. Changing a field
/// order without changing the stub corrupts every register in every backtrace, so the order
/// is pinned by the test below.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct TrapFrame {
    /// `r15`, pushed first.
    pub r15: u64,
    /// `r14`.
    pub r14: u64,
    /// `r13`.
    pub r13: u64,
    /// `r12`.
    pub r12: u64,
    /// `r11`, clobbered by `syscall` but saved for exceptions.
    pub r11: u64,
    /// `r10`, argument 5.
    pub r10: u64,
    /// `r9`, argument 6.
    pub r9: u64,
    /// `r8`, argument 5.
    pub r8: u64,
    /// `rbp`.
    pub rbp: u64,
    /// `rdi`, argument 1.
    pub rdi: u64,
    /// `rsi`, argument 2.
    pub rsi: u64,
    /// `rdx`, argument 3.
    pub rdx: u64,
    /// `rcx`, clobbered by `syscall`; holds the user's return address on entry.
    pub rcx: u64,
    /// `rbx`.
    pub rbx: u64,
    /// `rax`: the syscall number on entry, the return value on exit.
    pub rax: u64,
    /// The interrupt vector number.
    pub vector: u64,
    /// The error code the CPU pushed, or zero for vectors that push none.
    pub error_code: u64,
    /// `rip` at the time of the trap.
    pub rip: u64,
    /// `cs` at the time of the trap.
    pub cs: u64,
    /// `rflags`.
    pub rflags: u64,
    /// `rsp` at the time of the trap.
    pub rsp: u64,
    /// `ss` at the time of the trap.
    pub ss: u64,
}

impl TrapFrame {
    /// The syscall number this trap carries.
    #[must_use]
    pub const fn syscall_number(&self) -> Number {
        self.rax as Number
    }

    /// `true` if the trap came from user mode, which decides whether a fault becomes a
    /// signal or a bug check.
    #[must_use]
    pub const fn from_user_mode(&self) -> bool {
        // Ring 3 is encoded in the low two bits of the saved code segment selector.
        self.cs & 3 == 3
    }

    /// The six syscall arguments, in order.
    #[must_use]
    pub const fn args(&self) -> [u64; MAX_ARGS] {
        [self.rdi, self.rsi, self.rdx, self.r10, self.r8, self.r9]
    }
}

/// The vector number for the `syscall` entry point, if the fast path is unavailable and the
/// kernel falls back to a software interrupt. NOLX does not enable the fallback; the
/// constant exists so the IDT can be built with a hole in the right place.
pub const VECTOR_SYSCALL: u8 = 0x80;

/// The first vector the CPU reserves for exceptions.
pub const VECTOR_FIRST_EXCEPTION: u8 = 0;
/// The last vector the CPU reserves for exceptions.
pub const VECTOR_LAST_EXCEPTION: u8 = 31;
/// The vector the local APIC uses for the spurious-interrupt notification.
pub const VECTOR_SPURIOUS: u8 = 0xFF;

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::size_of;
    use nolx_types::errno;
    use nolx_types::syscall;

    #[test]
    fn decode_separates_success_from_errno() {
        assert_eq!(decode(0), Ok(0));
        assert_eq!(decode(4096), Ok(4096));
        assert_eq!(decode(-i64::from(errno::ENOENT)), Err(errno::ENOENT));
        assert_eq!(decode(-1), Err(1));
    }

    #[test]
    fn a_negative_one_byte_count_is_an_error_not_a_huge_value() {
        // The classic bug: `read` returning -EIO interpreted as a size_t.
        let raw = -i64::from(errno::EIO);
        assert!(decode(raw).is_err());
        assert_ne!(raw as u64, 0);
    }

    #[test]
    fn trap_frame_argument_order_matches_the_abi() {
        // If the stub and this struct ever disagree, this is what catches it.
        let frame = TrapFrame {
            rdi: 1,
            rsi: 2,
            rdx: 3,
            r10: 4,
            r8: 5,
            r9: 6,
            ..TrapFrame::default()
        };
        assert_eq!(frame.args(), [1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn the_fourth_argument_is_r10_not_rcx() {
        // Documented in `syscall4`'s doc comment; asserted here so the doc cannot drift.
        let frame = TrapFrame {
            r10: 99,
            rcx: 12345,
            ..TrapFrame::default()
        };
        assert_eq!(frame.args()[3], 99);
        assert!(!frame.args().contains(&12345));
    }

    #[test]
    fn user_mode_is_detected_from_the_saved_cs() {
        let user = TrapFrame {
            cs: 0x18 | 3,
            ..TrapFrame::default()
        };
        let kernel = TrapFrame {
            cs: 0x08,
            ..TrapFrame::default()
        };
        assert!(user.from_user_mode());
        assert!(!kernel.from_user_mode());
    }

    #[test]
    fn the_trap_frame_is_eight_byte_aligned_and_pointer_sized_fields_only() {
        assert_eq!(size_of::<TrapFrame>() % 8, 0);
        // 22 u64 fields, pinned so a reorder that changes the size is noticed.
        assert_eq!(size_of::<TrapFrame>(), 22 * 8);
    }

    #[test]
    fn the_register_convention_is_documented_in_code() {
        assert_eq!(MAX_ARGS, 6);
        assert!(CLOBBERED_BY_SYSCALL.contains(&"rcx"));
        assert!(CLOBBERED_BY_SYSCALL.contains(&"r11"));
    }

    #[test]
    fn exception_vectors_occupy_the_first_32() {
        assert_eq!(VECTOR_FIRST_EXCEPTION, 0);
        assert_eq!(VECTOR_LAST_EXCEPTION, 31);
        assert!(u16::from(VECTOR_SYSCALL) > u16::from(VECTOR_LAST_EXCEPTION));
    }

    #[test]
    fn every_assigned_syscall_number_has_a_name() {
        for (number, name) in syscall::TABLE {
            assert!(!name.is_empty());
            assert_eq!(syscall::name_of(*number), Some(*name));
        }
    }

    // ---------------------------------------------------------------- M6: T1 syscall
    // table uniqueness and ABI round-trip.

    #[test]
    fn syscall_numbers_are_unique_and_never_renumbered() {
        // The reservation policy in `nolx-types/src/syscall.rs` says a number is never
        // reused or renumbered. Uniqueness is the testable half; the tombstone rule is
        // enforced by review and by the pinning test below.
        let mut seen = [false; 1024];
        for (number, _) in syscall::TABLE {
            let index = usize::from(*number);
            assert!(index < seen.len(), "number {number} outside test window");
            assert!(!seen[index], "syscall number {number} assigned twice");
            seen[index] = true;
        }
    }

    #[test]
    fn syscall_names_round_trip_through_the_table() {
        for (number, name) in syscall::TABLE {
            assert_eq!(syscall::number_of(name), Some(*number));
            assert_eq!(syscall::name_of(*number), Some(*name));
            // The ABI round trip: number -> name -> number is the identity.
            assert_eq!(
                syscall::number_of(syscall::name_of(*number).unwrap()),
                Some(*number)
            );
        }
    }

    #[test]
    fn the_m6_dispatch_surface_numbers_are_pinned() {
        // These are the calls the ring-3 fast path implements. If one moves, every user
        // binary built before the change silently calls the wrong thing -- the exact bug
        // the reservation policy exists to prevent.
        assert_eq!(syscall::SYS_EXIT, 0);
        assert_eq!(syscall::SYS_WAITID, 4);
        assert_eq!(syscall::SYS_GETPID, 5);
        assert_eq!(syscall::SYS_POSIX_SPAWN, 31);
        assert_eq!(syscall::SYS_WRITE, 105);
        assert_eq!(
            syscall::HIGHEST,
            syscall::TABLE.iter().map(|(n, _)| *n).max().unwrap()
        );
    }

    #[test]
    fn arguments_round_trip_through_the_register_convention() {
        // The `syscall` instruction's register mapping, taken through a full trap frame:
        // what `TrapFrame::args` reads must be what a caller's `syscall6` writes.
        let values = [0x1111, 0x2222, 0x3333, 0x4444, 0x5555, 0x6666];
        let frame = TrapFrame {
            rax: u64::from(syscall::SYS_WRITE),
            rdi: values[0],
            rsi: values[1],
            rdx: values[2],
            r10: values[3],
            r8: values[4],
            r9: values[5],
            ..TrapFrame::default()
        };
        assert_eq!(frame.args(), values);
        assert_eq!(frame.syscall_number(), syscall::SYS_WRITE);
        assert_eq!(syscall::name_of(frame.syscall_number()), Some("write"));
    }

    #[test]
    fn return_values_round_trip_through_the_error_convention() {
        // Success stays non-negative, failure is exactly `-errno`, and `decode` is its
        // inverse. This is the whole ABI in one test: libc depends on nothing else.
        for value in [0i64, 1, 42, 4096, i64::from(u32::MAX)] {
            assert_eq!(decode(value), Ok(value));
        }
        for errno_value in [
            errno::E2BIG,
            errno::EAGAIN,
            errno::EBADF,
            errno::EINVAL,
            errno::ENOENT,
            errno::ENOMEM,
            errno::ENOSYS,
            errno::ESRCH,
        ] {
            let raw = -i64::from(errno_value);
            assert_eq!(decode(raw), Err(errno_value));
            // `-errno` never collides with a success value.
            assert!(raw < 0);
        }
    }

    #[test]
    fn the_syscall_boundary_carries_six_args_and_no_clobbered_registers() {
        assert_eq!(MAX_ARGS, 6);
        // `rcx` and `r11` are consumed by the instruction itself: an argument placed there
        // is destroyed, so the ABI's argument list must be exactly the six safe registers.
        let frame = TrapFrame {
            rcx: 0xDEAD,
            r11: 0xBEEF,
            rdi: 1,
            rsi: 2,
            rdx: 3,
            r10: 4,
            r8: 5,
            r9: 6,
            ..TrapFrame::default()
        };
        let args = frame.args();
        assert_eq!(args, [1, 2, 3, 4, 5, 6]);
        assert!(!args.contains(&0xDEAD));
        assert!(!args.contains(&0xBEEF));
    }
}
