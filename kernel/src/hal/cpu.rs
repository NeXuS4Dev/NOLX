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
    /// Current ring-0 stack top, later updated on a context switch.
    pub kernel_stack_top: u64,
    /// Software IPL currently recorded for this processor.
    pub current_ipl: u8,
    /// Features detected on this processor at boot.
    pub features: CpuFeatures,
}

impl PerCpuData {
    const fn empty() -> Self {
        Self {
            self_pointer: 0,
            cpu_id: 0,
            interrupt_depth: 0,
            kernel_stack_top: 0,
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
