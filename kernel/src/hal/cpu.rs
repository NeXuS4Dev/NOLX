//! CPU feature discovery and the bootstrap processor's GS-based local area.

use core::arch::asm;

use crate::sync::SyncCell;
use nolx_hal_core::ipl::Ipl;

use super::{wrmsr, CpuFeatures};

const IA32_GS_BASE: u32 = 0xC000_0101;
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

/// Per-processor data addressed through `%gs` while running in the kernel.
#[repr(C, align(64))]
pub struct PerCpuData {
    /// Self pointer, useful for validating a GS-base load.
    pub self_pointer: u64,
    /// Logical processor number assigned by the kernel.
    pub cpu_id: u32,
    /// Nesting depth of interrupt dispatch on this processor.
    pub interrupt_depth: u32,
    /// Current ring-0 stack top, updated on every context switch.
    pub kernel_stack_top: u64,
    /// Scratch slot holding the interrupted user `%rsp` across a `syscall` entry.
    pub user_rsp_scratch: u64,
    /// Software IPL currently recorded for this processor.
    pub current_ipl: u8,
    /// Features detected on this processor at boot.
    pub features: CpuFeatures,
}

/// `offsetof(PerCpuData, kernel_stack_top)`, mirrored by `syscall.S`.
pub const KERNEL_STACK_TOP_OFFSET: usize = 16;
/// `offsetof(PerCpuData, user_rsp_scratch)`, mirrored by `syscall.S`.
pub const USER_RSP_SCRATCH_OFFSET: usize = 24;

const _: () =
    assert!(core::mem::offset_of!(PerCpuData, kernel_stack_top) == KERNEL_STACK_TOP_OFFSET);
const _: () =
    assert!(core::mem::offset_of!(PerCpuData, user_rsp_scratch) == USER_RSP_SCRATCH_OFFSET);

impl PerCpuData {
    const fn empty() -> Self {
        Self {
            self_pointer: 0,
            cpu_id: 0,
            interrupt_depth: 0,
            kernel_stack_top: 0,
            user_rsp_scratch: 0,
            current_ipl: Ipl::PASSIVE.value(),
            features: CpuFeatures {
                extended_edx: 0,
                extended_ecx: 0,
                feature_edx: 0,
                feature_ecx: 0,
                power_edx: 0,
                max_leaf: 0,
                max_ext_leaf: 0,
            },
        }
    }
}

extern "C" {
    /// Top of the boot stack, also installed as the initial TSS RSP0.
    #[link_name = "boot_stack_top"]
    static BOOT_STACK_TOP: u8;
}

static BOOT_CPU: SyncCell<PerCpuData> = SyncCell::new(PerCpuData::empty());

/// Detect features and initialize the bootstrap processor's GS base.
///
/// NOLX is still single-processor here. AP startup will allocate and initialise one
/// `PerCpuData` block per processor before enabling that processor's interrupts.
pub fn init() -> CpuFeatures {
    // SAFETY: CPUID is supported because the 32-bit boot trampoline verified the ID flag,
    // and CPUID is side-effect free with a valid stack.
    let features = unsafe { CpuFeatures::detect() };

    // SAFETY: this is the one-time write to the bootstrap block, before another CPU or
    // interrupt can observe it. The raw pointer is also the address installed in GS_BASE.
    let base = unsafe {
        let local = BOOT_CPU.get();
        (*local).self_pointer = local as u64;
        (*local).cpu_id = 0;
        (*local).interrupt_depth = 0;
        (*local).kernel_stack_top = core::ptr::addr_of!(BOOT_STACK_TOP) as u64;
        (*local).current_ipl = Ipl::PASSIVE.value();
        (*local).features = features;
        local as u64
    };

    // SAFETY: both MSRs are architecturally defined on x86-64; `features` was detected
    // during boot and `base` points to the aligned, static local block above.
    unsafe {
        wrmsr(IA32_GS_BASE, base);
        wrmsr(IA32_KERNEL_GS_BASE, 0);
    }

    features
}

/// Read the current logical processor number through the GS-relative local block.
#[must_use]
pub fn current_cpu_id() -> u32 {
    let cpu_id: u32;
    // SAFETY: `init` installed a valid GS base before this accessor is used. The offset is
    // derived from the repr(C) layout and the local block remains live for the kernel.
    unsafe {
        asm!(
            "mov {cpu_id:e}, gs:[{offset}]",
            cpu_id = out(reg) cpu_id,
            offset = const core::mem::offset_of!(PerCpuData, cpu_id),
            options(nostack, readonly, preserves_flags),
        );
    }
    cpu_id
}

/// Return the software IPL recorded in this processor's GS-based local block.
#[must_use]
pub fn current_ipl() -> Ipl {
    let raw: u32;
    // SAFETY: `init` installed the BSP's GS base, and the fixed offset names `current_ipl`.
    unsafe {
        asm!(
            "movzx {raw:e}, byte ptr gs:[{offset}]",
            raw = out(reg) raw,
            offset = const core::mem::offset_of!(PerCpuData, current_ipl),
            options(nostack, readonly, preserves_flags),
        );
    }
    Ipl::from_raw(raw as u8).unwrap_or(Ipl::HIGH)
}

/// Raise the software IPL, returning the previous level, or `None` for a forbidden lowering.
///
/// Interrupt gates already clear IF on entry. When the local APIC is active, its TPR is updated
/// alongside the software IPL so lower-priority interrupts remain masked until restoration.
#[must_use]
pub fn raise_ipl(next: Ipl) -> Option<Ipl> {
    let previous = current_ipl();
    if next < previous {
        return None;
    }
    if super::apic::is_enabled() {
        super::apic::set_ipl(next);
    }
    write_current_ipl(next);
    Some(previous)
}

/// Restore a level previously returned by [`raise_ipl`], after the interrupt has been acked.
pub fn restore_ipl(previous: Ipl) {
    if super::apic::is_enabled() {
        super::apic::set_ipl(previous);
    }
    write_current_ipl(previous);
}

fn write_current_ipl(level: Ipl) {
    // SAFETY: `init` installed the BSP's GS base; the byte offset names a writable per-CPU
    // field and the ISR runs with maskable interrupts disabled while changing it.
    unsafe {
        asm!(
            "mov byte ptr gs:[{offset}], al",
            in("al") level.value(),
            offset = const core::mem::offset_of!(PerCpuData, current_ipl),
            options(nostack, preserves_flags),
        );
    }
}

/// Whether maskable interrupts are enabled on this processor.
#[must_use]
pub fn interrupts_enabled() -> bool {
    let flags: u64;
    // SAFETY: reading RFLAGS through the stack has no side effects, and `pushfq`/`pop` are
    // available at every privilege level.
    unsafe {
        asm!("pushfq", "pop {}", out(reg) flags, options(nomem));
    }
    flags & (1 << 9) != 0
}

/// Publish the ring-0 stack a user-mode entry lands on: `TSS.RSP0` for interrupts and
/// exceptions, and the GS scratch slot the `syscall` entry stub reads.
///
/// Both names are the same stack -- the current thread's kernel stack top -- and updating
/// them together is what keeps a `syscall`, an IRQ, and an exception from disagreeing
/// about where the kernel frame lives.
pub fn set_kernel_stack_top(top: u64) {
    // T2 forensics: a published zero top is exactly the failure mode that sent the next
    // syscall entry to %rsp = 0. Catch it at the source.
    assert!(top != 0, "set_kernel_stack_top(0)");
    // SAFETY: `init` installed the BSP's GS base and the offset names a writable per-CPU
    // field owned by the scheduler's context-switch path, which runs with interrupts off.
    unsafe {
        asm!(
            "mov gs:[{offset}], {top}",
            top = in(reg) top,
            offset = const KERNEL_STACK_TOP_OFFSET,
            options(nostack, preserves_flags),
        );
    }
    super::tables::set_rsp0(top);
}

/// The ring-0 stack top a `syscall` from user mode will switch to.
#[must_use]
pub fn kernel_stack_top() -> u64 {
    let top: u64;
    // SAFETY: as `set_kernel_stack_top`; the field is always a valid stack top or zero.
    unsafe {
        asm!(
            "mov {top}, gs:[{offset}]",
            top = out(reg) top,
            offset = const KERNEL_STACK_TOP_OFFSET,
            options(nostack, readonly, preserves_flags),
        );
    }
    top
}

/// Maskable interrupts, disabled for as long as the guard lives.
///
/// This is the primitive behind [`crate::sync::IrqLock`]: a critical section that must not be
/// entered by an interrupt handler runs with IF clear, and the guard restores the flag to
/// whatever it was -- so a lock taken inside an interrupt handler does not accidentally enable
/// interrupts on the way out.
pub struct InterruptGuard {
    /// Whether IF was set when the guard was taken.
    enabled: bool,
}

impl InterruptGuard {
    /// Disable maskable interrupts, remembering whether they were on.
    #[must_use]
    pub fn disable() -> Self {
        let enabled = interrupts_enabled();
        // SAFETY: `cli` clears IF. It changes nothing else, and the guard restores the
        // previous flag when it is dropped.
        unsafe { asm!("cli", options(nomem, nostack)) }
        Self { enabled }
    }

    /// Restore IF and halt atomically with respect to maskable interrupt delivery.
    ///
    /// The guard must have been acquired with interrupts enabled, and the caller
    /// must have an armed interrupt source. Keeping IF clear until STI/HLT avoids
    /// losing a wakeup between dropping the guard and executing a separate HLT.
    pub fn halt_until_interrupt(mut self) {
        assert!(
            self.enabled,
            "halting requires a previously enabled interrupt source"
        );
        self.enabled = false;
        // SAFETY: ring-0 caller holds IF clear through this guard and promises an
        // armed interrupt source. STI's interrupt shadow covers the adjacent HLT.
        // The consumed guard must not restore IF a second time on return. Keep
        // the compiler memory clobber so protected writes precede the wakeup.
        unsafe { asm!("sti", "hlt", options(nostack)) };
    }
}

impl Drop for InterruptGuard {
    fn drop(&mut self) {
        if self.enabled {
            // SAFETY: interrupts were enabled when the guard was taken, so restoring IF is
            // what makes the guard's scope transparent to its caller.
            unsafe { asm!("sti", options(nomem, nostack)) }
        }
    }
}
