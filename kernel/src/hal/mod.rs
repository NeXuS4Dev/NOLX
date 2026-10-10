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

pub mod console;
pub mod multiboot;

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
    // SAFETY: the caller asserts the port is valid. `in` touches no memory and the
    // `"={al}"` constraint ties the result to the register the instruction writes.
    unsafe { core::arch::asm!("in al, dx", out("al") value, in("dx") port, options(nomem, nostack, preserves_flags)) };
    value
}

/// Write a byte to an I/O port.
///
/// # Safety
///
/// As [`inb`], and the write may have an immediate device side effect.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: the caller asserts the port is valid and accepts the write.
    unsafe { core::arch::asm!("out dx, al", in("dx") port, in("al") value, options(nomem, nostack, preserves_flags)) };
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
    // SAFETY: the caller asserts the MSR exists. `rdmsr` returns its result in edx:eax,
    // which the two `out` constraints capture.
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
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
    // SAFETY: the caller asserts the MSR exists and that `value` is valid for it.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
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
        let _ = (extended_eax, extended_ebx);
        Self {
            extended_edx,
            extended_ecx,
            feature_edx,
            feature_ecx,
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
