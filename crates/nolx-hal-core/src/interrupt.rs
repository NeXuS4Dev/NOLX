//! Register frame shared by the x86-64 assembly stubs and Rust interrupt dispatcher.

/// Saved state at the start of the common long-mode interrupt handler.
///
/// Assembly pushes the general-purpose registers in reverse field order, then the vector
/// and normalized error code. The CPU's 64-bit interrupt frame follows with RIP, CS, RFLAGS,
/// RSP, and SS; long mode saves the old stack pair unconditionally.
#[repr(C)]
pub struct InterruptFrame {
    /// Saved R15.
    pub r15: u64,
    /// Saved R14.
    pub r14: u64,
    /// Saved R13.
    pub r13: u64,
    /// Saved R12.
    pub r12: u64,
    /// Saved R11.
    pub r11: u64,
    /// Saved R10.
    pub r10: u64,
    /// Saved R9.
    pub r9: u64,
    /// Saved R8.
    pub r8: u64,
    /// Saved RBP.
    pub rbp: u64,
    /// Saved RDI.
    pub rdi: u64,
    /// Saved RSI.
    pub rsi: u64,
    /// Saved RDX.
    pub rdx: u64,
    /// Saved RCX.
    pub rcx: u64,
    /// Saved RBX.
    pub rbx: u64,
    /// Saved RAX.
    pub rax: u64,
    /// Exception or external-interrupt vector inserted by the assembly stub.
    pub vector: u64,
    /// Processor error code, or zero for vectors without one.
    pub error_code: u64,
    /// Instruction pointer at the interruption point.
    pub instruction_pointer: u64,
    /// Code-segment selector at the interruption point.
    pub code_segment: u64,
    /// Processor flags at the interruption point.
    pub flags: u64,
    /// Stack pointer at the interruption point.
    pub stack_pointer: u64,
    /// Stack-segment selector at the interruption point.
    pub stack_segment: u64,
}

const _: () = assert!(core::mem::size_of::<InterruptFrame>() == 22 * 8);

#[cfg(test)]
mod tests {
    use super::*;
    use core::mem::{align_of, offset_of, size_of};

    #[test]
    fn interrupt_frame_matches_the_assembly_push_and_cpu_frame_layout() {
        assert_eq!(size_of::<InterruptFrame>(), 176);
        assert_eq!(align_of::<InterruptFrame>(), 8);
        assert_eq!(offset_of!(InterruptFrame, r15), 0);
        assert_eq!(offset_of!(InterruptFrame, rax), 112);
        assert_eq!(offset_of!(InterruptFrame, vector), 120);
        assert_eq!(offset_of!(InterruptFrame, error_code), 128);
        assert_eq!(offset_of!(InterruptFrame, instruction_pointer), 136);
        assert_eq!(offset_of!(InterruptFrame, code_segment), 144);
        assert_eq!(offset_of!(InterruptFrame, flags), 152);
        assert_eq!(offset_of!(InterruptFrame, stack_pointer), 160);
        assert_eq!(offset_of!(InterruptFrame, stack_segment), 168);
    }
}
