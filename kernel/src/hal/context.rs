//! x86-64 kernel-thread stack bootstrap and low-level context helpers.
//!
//! The common interrupt epilogue restores a saved [`InterruptFrame`] and executes `iretq`.
//! This module constructs the same frame for a brand-new ring-0 thread, so first dispatch
//! and resumed dispatch use one assembly path.

use core::arch::asm;
use core::mem::size_of;

use super::interrupts::InterruptFrame;

/// Why a kernel thread's first interrupt frame could not be placed on its stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextInitError {
    /// The stack base is not aligned for x86-64 ABI entry.
    MisalignedStack,
    /// The stack is too small for the saved registers and synthetic interrupt return frame.
    StackTooSmall,
    /// The stack end address overflowed while computing an aligned top.
    AddressOverflow,
}

/// Build the synthetic interrupt frame at the top of a stable kernel stack.
///
/// The stack must be writable, 16-byte aligned, at least 512 bytes long, and remain allocated
/// for the whole thread lifetime. `entry` runs at ring 0 with interrupts enabled; `argument`
/// is placed in RDI according to the x86-64 C calling convention.
pub fn initialize_kernel_stack(
    stack: &mut [u8],
    entry: extern "C" fn() -> !,
    argument: usize,
) -> Result<*mut InterruptFrame, ContextInitError> {
    let base = stack.as_mut_ptr() as usize;
    if base & 0xF != 0 {
        return Err(ContextInitError::MisalignedStack);
    }
    let unaligned_top = base
        .checked_add(stack.len())
        .ok_or(ContextInitError::AddressOverflow)?;
    let top = unaligned_top & !0xF;
    let frame_bytes = size_of::<InterruptFrame>() + size_of::<u64>();
    let frame_address = top
        .checked_sub(frame_bytes)
        .ok_or(ContextInitError::StackTooSmall)?;
    if stack.len() < 512 || frame_address < base {
        return Err(ContextInitError::StackTooSmall);
    }

    let frame = InterruptFrame {
        r15: 0,
        r14: 0,
        r13: 0,
        r12: 0,
        r11: 0,
        r10: 0,
        r9: 0,
        r8: 0,
        rbp: 0,
        rdi: argument as u64,
        rsi: 0,
        rdx: 0,
        rcx: 0,
        rbx: 0,
        rax: 0,
        vector: 0,
        error_code: 0,
        instruction_pointer: entry as usize as u64,
        code_segment: 0x08,
        flags: 0x202,
        // Same-CPL IRETQ restores RIP/CS/RFLAGS. Keeping RSP at top-24 leaves a normal
        // 16-byte-aligned call frame (RSP == 8 mod 16) when the kernel thread first enters.
        stack_pointer: (top - 3 * size_of::<u64>()) as u64,
        stack_segment: 0x10,
    };
    let frame_pointer = frame_address as *mut InterruptFrame;

    // SAFETY: `frame_address` is 8-byte aligned, lies fully inside the exclusive `stack`
    // slice, and was checked to leave the synthetic-frame plus alignment word in bounds.
    // The caller owns the slice and does not publish the pointer until initialization ends.
    unsafe { frame_pointer.write(frame) };
    Ok(frame_pointer)
}

/// Halt the current ring-0 thread until a maskable interrupt arrives.
///
/// The caller must have IF set and at least one interrupt source unmasked. The M5 kernel
/// workers satisfy this by starting only after the clock vector has been armed.
pub(crate) fn halt_for_interrupt() {
    // SAFETY: this helper is called only by ring-0 kernel threads with interrupts enabled;
    // the armed timer guarantees HLT will resume and deliver a scheduler tick.
    unsafe { asm!("hlt", options(nomem, nostack)) };
}
