//! The `syscall`/`sysret` fast path: MSRs, selectors, and the Rust side of the entry stub.
//!
//! One instruction crosses the boundary in each direction:
//!
//! * `syscall` saves the user `%rip` in `%rcx` and `%rflags` in `%r11`, masks the flags in
//!   `SFMASK`, loads `%cs`/`%ss` from `STAR[47:32]`, and jumps to `LSTAR` with `%rsp`
//!   unchanged -- the entry stub switches to the thread's kernel stack itself.
//! * `sysretq` undoes it: `%rcx` becomes `%rip`, `%r11` becomes `%rflags`, `%cs`/`%ss`
//!   come from `STAR[63:48]`, and the privilege level is forced to 3.
//!
//! `SFMASK` masks IF as well as TF and DF. That is deliberate: between the `syscall`
//! instruction and the stub's `swapgs` the processor is already in ring 0 but still holds
//! the *user* GS base, and an interrupt in that window would run the handler against user
//! `%gs`. Masking IF closes the window; the stub re-enables nothing because the handlers
//! run with interrupts disabled and return through a frame that restores the user's IF.

use core::arch::asm;

/// `IA32_STAR`: the selector pairs `syscall` and `sysret` load.
///
/// Bits 32..48 name the kernel code selector (0x08), whose +8 companion is kernel data
/// (0x10). Bits 48..64 are the *base* of the user pair: +8 is user data (0x18) and +16 is
/// user code (0x20), the order `sysret` requires and `boot/boot.S` defines.
const MSR_STAR: u32 = 0xC000_0081;
/// `IA32_LSTAR`: the ring-0 entry point of `syscall`.
const MSR_LSTAR: u32 = 0xC000_0082;
/// `IA32_SFMASK`: the `rflags` bits `syscall` clears on entry.
const MSR_SFMASK: u32 = 0xC000_0084;
/// `EFER`, whose bit 0 (`SCE`) enables the `syscall`/`sysret` instructions at all.
const MSR_EFER: u32 = 0xC000_0080;
const EFER_SCE: u64 = 1;

/// Kernel code selector for `syscall` entries.
pub const KERNEL_CODE_SELECTOR: u64 = 0x08;
/// Kernel data selector for `syscall` entries.
pub const KERNEL_DATA_SELECTOR: u64 = 0x10;
/// User data selector: `sysret` derives it as `STAR[63:48] + 8`.
pub const USER_DATA_SELECTOR: u64 = 0x18;
/// User code selector: `sysret` derives it as `STAR[63:48] + 16`.
pub const USER_CODE_SELECTOR: u64 = 0x20;
/// The selectors as the CPU pushes them on a privilege change: RPL forced to 3.
pub const USER_DATA_RPL3: u64 = USER_DATA_SELECTOR | 3;
/// See [`USER_DATA_RPL3`].
pub const USER_CODE_RPL3: u64 = USER_CODE_SELECTOR | 3;

/// The `vector` a syscall frame carries, so the shared restore path can recognise it.
pub const SYSCALL_FRAME_VECTOR: u64 = 0x80;

/// `rflags` bits `syscall` clears on entry: TF (8), IF (9), and DF (10).
const SFMASK: u64 = (1 << 8) | (1 << 9) | (1 << 10);

extern "C" {
    /// The `syscall` entry stub in `syscall.S`.
    fn nolx_syscall_entry();
}

/// Program `STAR`/`LSTAR`/`SFMASK`, enable `EFER.SCE`, and arm the fast path.
///
/// Runs once on the boot processor with interrupts disabled, after the GDT is live and
/// before any ring-3 thread exists.
pub fn init() {
    let star = (KERNEL_CODE_SELECTOR << 32) | (KERNEL_DATA_SELECTOR << 48);
    // SAFETY: all four MSRs are architecturally defined on x86-64. `LSTAR` names a linked
    // assembly symbol, `STAR` matches the GDT `boot/boot.S` defines, and this is the
    // one-time bring-up on the boot processor with interrupts disabled.
    unsafe {
        wrmsr(MSR_STAR, star);
        wrmsr(MSR_LSTAR, nolx_syscall_entry as *const () as usize as u64);
        wrmsr(MSR_SFMASK, SFMASK);
        let efer = rdmsr(MSR_EFER);
        if efer & EFER_SCE == 0 {
            wrmsr(MSR_EFER, efer | EFER_SCE);
        }
    }
}

use super::{rdmsr, wrmsr};

/// The register state a `syscall` carries, decoded from the shared trap frame.
///
/// This is the kernel's view of `crates/nolx-syscall`'s convention: number in `rax`,
/// arguments in `rdi, rsi, rdx, r10, r8, r9`, result back in `rax`.
#[derive(Clone, Copy, Debug)]
pub struct SyscallRequest {
    /// The syscall number.
    pub number: u64,
    /// The six argument registers, in ABI order.
    pub args: [u64; 6],
}

/// Whether a trap frame is a `syscall` frame rather than an interrupt or exception.
#[must_use]
pub const fn is_syscall_frame(vector: u64) -> bool {
    vector == SYSCALL_FRAME_VECTOR
}

/// The stack pointer value `swapgs` left in the per-CPU scratch slot, if a `syscall` is in
/// flight on this processor. Used only by tests and diagnostics.
#[must_use]
pub fn scratch_user_rsp() -> u64 {
    let value: u64;
    // SAFETY: the GS base is the kernel per-CPU block for as long as this code runs in
    // ring 0 outside the entry window `SFMASK` protects.
    unsafe {
        asm!(
            "mov {value}, gs:[{offset}]",
            value = out(reg) value,
            offset = const crate::hal::cpu::USER_RSP_SCRATCH_OFFSET,
            options(nostack, readonly, preserves_flags),
        );
    }
    value
}
