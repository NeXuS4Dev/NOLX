//! Hardware Abstraction Layer.
//!
//! Everything in here knows something specific about the machine. Nothing above this layer
//! is allowed to: the executive talks to [`console`], to CPU abstractions, and to the
//! firmware hand-off, and to nothing else with an address in it.
//!
//! That rule is what makes the kernel portable in principle. It is also why this module
//! carries most of the crate's `unsafe` budget -- talking to ports and to memory-mapped
//! hardware cannot be done in safe Rust, and pretending otherwise by hiding the `unsafe`
//! in a "safe" wrapper would be worse.

pub mod acpi;
pub mod ahci;
pub mod apic;
pub mod blockdev;
pub mod console;
pub mod context;
pub mod cpu;
pub mod dma;
pub mod driver_start;
pub mod e1000;
pub mod hpet;
pub mod interrupts;
pub mod ioapic;
pub mod msix;
pub mod multiboot;
pub mod nvme;
pub mod pci;
pub mod pic;
pub mod ps2;
pub mod rtc;
pub mod storage_volume;
pub mod syscall;
pub mod tables;
pub mod timer;
pub mod virtio_blk;
pub mod virtio_net;
pub mod xhci;

use nolx_hal_core::console::HalConsole;
use nolx_power_core as power;

/// `EFER`, the extended feature enable register.
const MSR_EFER: u32 = 0xC000_0080;
/// Bit 11 of `EFER`: no-execute page protection is enabled, which is what makes bit 63 of a
/// page-table entry an NX bit instead of a reserved bit.
const EFER_NXE: u64 = 1 << 11;

/// Turn on no-execute page protection.
///
/// The kernel's page tables mark data pages no-execute, and an x86-64 processor only honours
/// that bit when `EFER.NXE` is set: without it, bit 63 of a page-table entry is *reserved*, and
/// the first access to an NX-marked page is a page fault with the reserved-bit error bit set --
/// a fault that looks like a corrupt page table rather than a missing feature. The boot layer
/// sets `LME` to enter long mode; NXE is the kernel's own business, and this is the only place
/// it is enabled.
///
/// A processor that does not report NX support is reported rather than silently worked around:
/// there is no way to map a page that is writable but not executable, and the address-space code
/// assumes there is.
fn enable_no_execute(features: &CpuFeatures) {
    if !features.has_nx() {
        return;
    }
    // SAFETY: EFER is a kernel-mode MSR, this runs on the boot processor before any other
    // processor exists and before any page table entry sets bit 63, and the write only changes
    // how that bit is interpreted.
    unsafe {
        let efer = rdmsr(MSR_EFER);
        if efer & EFER_NXE == 0 {
            wrmsr(MSR_EFER, efer | EFER_NXE);
        }
    }
}

/// Initialize processor state, descriptor tables, firmware discovery, and timer controllers.
///
/// Maskable interrupts remain disabled throughout initialization. The selected timer source is
/// armed separately after the rest of the kernel's one-time initialization is complete.
pub fn init(console: &mut dyn HalConsole) {
    let features = cpu::init();
    enable_no_execute(&features);
    tables::init();
    syscall::init();
    pic::init();
    timer::init();

    let _ = writeln!(
        console,
        "cpu: max leaf {:#x}, syscall={}, nx={}, xsave={}, rdrand={}, tsc={}, invariant-tsc={}, apic={}, x2apic={}, tsc-deadline={}, gs-cpu={}",
        features.max_leaf,
        features.has_syscall(),
        features.has_nx(),
        features.has_xsave(),
        features.has_rdrand(),
        features.has_tsc(),
        features.has_invariant_tsc(),
        features.has_apic(),
        features.has_x2apic(),
        features.has_tsc_deadline(),
        cpu::current_cpu_id(),
    );
    if features.has_nx() {
        let _ = writeln!(
            console,
            "cpu: EFER.NXE enabled; no-execute page mappings are in force"
        );
    } else {
        let _ = writeln!(
            console,
            "cpu: WARNING NX is not supported; the kernel cannot mark data pages no-execute"
        );
    }
    let _ = writeln!(console, "hal: GDT/TSS loaded; 256-entry IDT installed");
    let _ = writeln!(
        console,
        "hal: syscall/sysret fast path armed (STAR kernel 0x08/0x10, user 0x1b/0x23)"
    );
    let _ = writeln!(
        console,
        "hal: 8259 PIC remapped to vectors 32..47; all IRQs masked"
    );

    if let Some(platform) = acpi::discover() {
        let _ = writeln!(
            console,
            "acpi: RSDP revision={} root={} local-apics={} io-apics={}",
            platform.rsdp_revision,
            if platform.used_xsdt { "XSDT" } else { "RSDT" },
            platform.madt.local_apics().len(),
            platform.madt.io_apics().len(),
        );
        match platform.fadt {
            Some(fadt) => {
                let _ = writeln!(
                    console,
                    "acpi: PM1a control block at {:#x}, DSDT at {:#x}",
                    fadt.pm1a_control, fadt.dsdt
                );
            }
            None => {
                let _ = writeln!(console, "acpi: no FADT; no sleep control block");
            }
        }
        match platform.s5_sleep_type {
            Some(sleep_type) => {
                let word = power::pm1_sleep_value(sleep_type, true).unwrap_or(0);
                let _ = writeln!(
                    console,
                    "acpi: S5 sleep type {sleep_type}, soft-off word would be {word:#x}, not entered"
                );
            }
            None => {
                let _ = writeln!(console, "acpi: S5 not described in the DSDT");
            }
        }
        match platform.s3_sleep_type {
            Some(sleep_type) => {
                let _ = writeln!(console, "acpi: S3 sleep type {sleep_type}, not entered");
            }
            None => {
                let _ = writeln!(console, "acpi: S3 not described in the DSDT");
            }
        }
        let hpet_frequency = match platform.hpet {
            Some(info) => match hpet::init(&info) {
                Ok(frequency) => {
                    let _ = writeln!(console, "hpet: counter enabled at {frequency} Hz");
                    Some(frequency)
                }
                Err(error) => {
                    let _ = writeln!(console, "hpet: unavailable ({error:?})");
                    None
                }
            },
            None => {
                let _ = writeln!(console, "hpet: firmware table absent");
                None
            }
        };

        match apic::init(&platform.madt, features) {
            Ok(()) => {
                let _ = writeln!(
                    console,
                    "apic: {} enabled; BSP id={} IOAPIC IRQ routes initialized masked",
                    apic::mode_name(),
                    apic::current_apic_id(),
                );
                if let Some(frequency) = hpet_frequency {
                    match apic::calibrate_timers(frequency, features) {
                        Ok(calibration) => {
                            let _ = writeln!(
                                console,
                                "apic: calibrated timer={} Hz tsc={:?} Hz",
                                calibration.lapic_ticks_per_second, calibration.tsc_frequency_hz,
                            );
                        }
                        Err(error) => {
                            let _ = writeln!(
                                console,
                                "apic: timer calibration unavailable ({error:?})"
                            );
                        }
                    }
                }
            }
            Err(error) => {
                let _ = writeln!(
                    console,
                    "apic: unavailable ({error:?}); retaining PIC fallback"
                );
            }
        }
    } else {
        let _ = writeln!(
            console,
            "acpi: no valid MADT found; retaining PIC/PIT fallback"
        );
    }
    let _ = writeln!(
        console,
        "hal: timer fallback prepared at {} Hz; source starts after kernel initialization",
        timer::frequency_hz()
    );
}

/// Sample the CMOS clock as late as possible before starting the monotonic timer.
///
/// The wall-clock anchor is paired with the current monotonic counter; taking this snapshot
/// after slow one-time initialization avoids excluding that boot work from elapsed realtime.
pub fn init_wall_clock(console: &mut dyn HalConsole) {
    if let Some(time) = rtc::init() {
        let _ = writeln!(
            console,
            "wall-clock: CMOS RTC UTC epoch={}s ({:04}-{:02}-{:02} {:02}:{:02}:{:02})",
            time.unix_seconds(),
            time.year(),
            time.month(),
            time.day(),
            time.hour(),
            time.minute(),
            time.second(),
        );
    } else {
        let _ = writeln!(
            console,
            "wall-clock: CMOS RTC unavailable or invalid; realtime unavailable"
        );
    }
}

/// Enable the periodic clock interrupt after all boot-time subsystem setup is complete.
pub fn start_timer() {
    timer::start();
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// The caller must know that `port` is a valid x86 I/O port for this machine and that
/// reading it has no side effect the caller is unprepared for. Some ports latch state on
/// read; reading those twice is a bug.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // The only option here is `nostack`. The `nomem` and `preserves_flags` that a port
    // read looks like it deserves are deliberately absent: together with `nostack` they
    // tell LLVM the block has no observable effect beyond its outputs, and then LLVM is
    // free to hoist the read out of a spin loop -- which turns
    // `while inb(LSR) & THRE == 0 {}` into an infinite loop that reads the port once.
    // Port I/O is a side effect; the compiler must not be told otherwise.
    //
    // SAFETY: the caller asserts the port is valid, and `out("al")` ties the result to the
    // register the instruction writes.
    unsafe { core::arch::asm!("in al, dx", out("al") value, in("dx") port, options(nostack)) };
    value
}

/// Write a byte to an I/O port.
///
/// # Safety
///
/// As [`inb`], and the write may have an immediate device side effect.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // `nostack` only, and the reason is sharper here than for [`inb`]: this block has no
    // output operands at all, so the three options together would describe an instruction
    // with no observable effect whatsoever, which licenses LLVM to delete it. A deleted
    // port write is the quietest failure there is -- the code runs, nothing faults, and
    // nothing is ever transmitted.
    //
    // SAFETY: the caller asserts the port is valid and accepts the write.
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nostack)) };
}

/// Read a 32-bit value from an I/O port.
///
/// # Safety
///
/// As [`inb`]. A 32-bit port access is one instruction, so it is atomic with respect to
/// other CPUs' accesses to the same port; a *pair* of accesses (the PCI configuration address
/// and data ports) is not, and the caller must serialize that pair itself.
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // The same reasoning as [`inb`] applies: a port read is a side effect, so neither `nomem`
    // nor `preserves_flags` is claimed.
    //
    // SAFETY: the caller asserts the port is valid, and `out("eax")` ties the result to the
    // register the instruction writes.
    unsafe { core::arch::asm!("in eax, dx", out("eax") value, in("dx") port, options(nostack)) };
    value
}

/// Write a 32-bit value to an I/O port.
///
/// # Safety
///
/// As [`outb`], and the same pairing caveat as [`inl`].
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: the caller asserts the port is valid and accepts the write.
    unsafe { core::arch::asm!("out dx, eax", in("dx") port, in("eax") value, options(nostack)) };
}

/// Read a 32-bit model-specific register.
///
/// # Safety
///
/// `msr` must be a register that exists on this CPU. Reading an unimplemented MSR raises
/// `#GP`, which in the kernel is fatal.
#[inline]
pub unsafe fn rdmsr(msr: u32) -> u64 {
    let low: u32;
    let high: u32;
    // `nostack` only, for the reason [`inb`] gives.
    //
    // SAFETY: the caller asserts the MSR exists. `rdmsr` returns its result in edx:eax,
    // which the two `out` constraints capture.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nostack),
        )
    };
    (u64::from(high) << 32) | u64::from(low)
}

/// Write a 32-bit model-specific register.
///
/// # Safety
///
/// As [`rdmsr`], and writing some MSRs changes the CPU's mode of operation.
#[inline]
pub unsafe fn wrmsr(msr: u32, value: u64) {
    // `nostack` only: like [`outb`], this has no outputs, so claiming no other effect
    // would license LLVM to delete a write that changes how the CPU behaves.
    //
    // SAFETY: the caller asserts the MSR exists and that `value` is valid for it.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nostack),
        )
    };
}

/// What CPUID told us, read once at boot.
#[derive(Clone, Copy, Debug)]
pub struct CpuFeatures {
    /// Extended feature bits from leaf `0x8000_0001`, `edx`.
    pub extended_edx: u32,
    /// Extended feature bits from leaf `0x8000_0001`, `ecx`.
    pub extended_ecx: u32,
    /// Feature bits from leaf 1, `edx`.
    pub feature_edx: u32,
    /// Feature bits from leaf 1, `ecx`.
    pub feature_ecx: u32,
    /// Invariant-TSC capability bits from leaf `0x8000_0007`, `edx`.
    pub power_edx: u32,
    /// The largest standard CPUID leaf supported.
    pub max_leaf: u32,
    /// The largest extended CPUID leaf supported.
    pub max_ext_leaf: u32,
}

impl CpuFeatures {
    /// Query the CPU.
    ///
    /// # Safety
    ///
    /// Must run with a valid stack; CPUID is otherwise side-effect free.
    pub unsafe fn detect() -> Self {
        // SAFETY: CPUID only writes the four registers named in the constraints.
        let (max_leaf, _, _, _) = unsafe { cpuid(0) };
        let (_, _, feature_ecx, feature_edx) = unsafe { cpuid(1) };
        let (max_ext_leaf, _, _, _) = unsafe { cpuid(0x8000_0000) };
        let (extended_eax, extended_ebx, extended_ecx, extended_edx) =
            if max_ext_leaf >= 0x8000_0001 {
                // SAFETY: the leaf is supported, per the check above.
                unsafe { cpuid(0x8000_0001) }
            } else {
                (0, 0, 0, 0)
            };
        let (_, _, _, power_edx) = if max_ext_leaf >= 0x8000_0007 {
            // SAFETY: the extended power-management leaf is supported, per the check above.
            unsafe { cpuid(0x8000_0007) }
        } else {
            (0, 0, 0, 0)
        };
        let _ = (extended_eax, extended_ebx);
        Self {
            extended_edx,
            extended_ecx,
            feature_edx,
            feature_ecx,
            power_edx,
            max_leaf,
            max_ext_leaf,
        }
    }

    /// `true` if the CPU supports `syscall`/`sysret`, which the POSIX ABI requires.
    #[must_use]
    pub const fn has_syscall(&self) -> bool {
        self.extended_edx & (1 << 11) != 0
    }

    /// `true` if the CPU supports the NX (no-execute) page bit.
    #[must_use]
    pub const fn has_nx(&self) -> bool {
        self.extended_edx & (1 << 20) != 0
    }

    /// `true` if the CPU supports 1 GiB pages.
    #[must_use]
    pub const fn has_1gib_pages(&self) -> bool {
        self.extended_edx & (1 << 26) != 0
    }

    /// `true` if the CPU supports the timestamp counter (CPUID.01H:EDX[4]).
    #[must_use]
    pub const fn has_tsc(&self) -> bool {
        self.feature_edx & (1 << 4) != 0
    }

    /// `true` if the timestamp counter runs at a constant rate (CPUID.80000007H:EDX[8]).
    #[must_use]
    pub const fn has_invariant_tsc(&self) -> bool {
        self.power_edx & (1 << 8) != 0
    }

    /// `true` if the CPU supports the local APIC (CPUID.01H:EDX[9]).
    #[must_use]
    pub const fn has_apic(&self) -> bool {
        self.feature_edx & (1 << 9) != 0
    }

    /// `true` if the CPU supports x2APIC register access (CPUID.01H:ECX[21]).
    #[must_use]
    pub const fn has_x2apic(&self) -> bool {
        self.feature_ecx & (1 << 21) != 0
    }

    /// `true` if the CPU supports the TSC-deadline local APIC timer mode (CPUID.01H:ECX[24]).
    #[must_use]
    pub const fn has_tsc_deadline(&self) -> bool {
        self.feature_ecx & (1 << 24) != 0
    }

    /// `true` if the CPU supports `rdrand`.
    #[must_use]
    pub const fn has_rdrand(&self) -> bool {
        self.feature_ecx & (1 << 30) != 0
    }

    /// `true` if the CPU supports `xsave`, which the FPU context switch depends on.
    #[must_use]
    pub const fn has_xsave(&self) -> bool {
        self.feature_ecx & (1 << 26) != 0
    }
}

/// Execute `cpuid` for `leaf`, returning `(eax, ebx, ecx, edx)`.
///
/// # Safety
///
/// `leaf` must be supported; an unsupported leaf returns zeros in the feature registers,
/// which callers must interpret rather than trust.
#[inline]
pub unsafe fn cpuid(leaf: u32) -> (u32, u32, u32, u32) {
    let eax: u32;
    let ebx: u32;
    let ecx: u32;
    let edx: u32;
    // LLVM reserves `rbx` on x86-64 and refuses it as an inline-asm operand, so CPUID's
    // output there cannot be captured with a register constraint. It has to be moved out by
    // hand: save the callee-saved register, run the instruction, copy its `ebx` into a
    // scratch register the compiler allocated, restore. The push and pop are balanced, so
    // the stack is where the ABI expects on both sides -- which is also why this block
    // cannot be marked `nostack`, and why `nomem` is wrong too, since the push writes
    // memory.
    //
    // SAFETY: CPUID modifies only the four named registers, and `rbx` is restored before
    // the block ends, so nothing outside the declared outputs is left changed.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "mov {ebx:e}, ebx",
            "pop rbx",
            ebx = out(reg) ebx,
            inout("eax") leaf => eax,
            inout("ecx") 0u32 => ecx,
            out("edx") edx,
            options(preserves_flags),
        )
    };
    (eax, ebx, ecx, edx)
}
