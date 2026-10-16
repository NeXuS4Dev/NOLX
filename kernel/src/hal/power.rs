//! Low-level ACPI S5 request with validated legacy control ports and ACPI-mode enablement.
//! This is not a substitute for system-wide device quiescence or an S3 resume implementation.

use nolx_hal_core::console::HalConsole;
use nolx_power_core::{pm1_sleep_control, Fadt};

/// Why no power-state request could be issued.
#[derive(Debug)]
pub enum Error {
    /// ACPI/FADT/S5 data was absent.
    Firmware,
    /// The platform requires an unsupported control-block format or PM1b sleep type.
    Unsupported,
    /// ACPI-mode enablement did not complete within its bounded wait.
    AcpiMode,
}

unsafe fn read_control(port: u16) -> u16 {
    let value: u16;
    // SAFETY: callers supply a validated FADT legacy PM1 control port with width >= 2.
    unsafe { core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nostack)) };
    value
}
unsafe fn write_control(port: u16, value: u16) {
    // SAFETY: callers supply the validated PM1 control port and a preserved control word.
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack)) };
}

fn enable_acpi(fadt: &Fadt) -> Result<u16, Error> {
    // SAFETY: the caller checked FADT PM1 control width and the parser validated its I/O port.
    let current = unsafe { read_control(fadt.pm1a_control) };
    if current & 1 != 0 {
        return Ok(current);
    }
    let port = fadt.smi_command.ok_or(Error::Unsupported)?;
    if fadt.acpi_enable == 0 {
        return Err(Error::Unsupported);
    }
    // SAFETY: the SMI command is a validated 16-bit port explicitly designated by firmware.
    unsafe { super::outb(port, fadt.acpi_enable) };
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    for index in 0..100_000_000u32 {
        // SAFETY: same validated control port as the first read above.
        let current = unsafe { read_control(fadt.pm1a_control) };
        if current & 1 != 0 {
            return Ok(current);
        }
        match (clock, super::hpet::counter()) {
            (Some((start, hz)), Some(now))
                if super::hpet::elapsed_ticks(start, now) >= hz.saturating_mul(2) =>
            {
                break
            }
            (None, _) if index >= 1_000_000 => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    Err(Error::AcpiMode)
}

/// Issue an actual ACPI S5 request, preserving unrelated control bits.
/// An `Ok` return means the request was issued; platform shutdown is externally observed.
///
/// # Safety
/// The caller must flush all storage, stop device DMA and other CPUs, and prevent new work.
/// No resume is possible after this request. The dedicated QEMU harness supplies a machine
/// with no active DMA devices; this primitive does not claim to implement that global policy.
pub unsafe fn request_s5_quiesced(console: &mut dyn HalConsole) -> Result<(), Error> {
    let tables = super::acpi::discover().ok_or(Error::Firmware)?;
    let fadt = tables.fadt.ok_or(Error::Firmware)?;
    let sleep_type = tables.s5_sleep_type.ok_or(Error::Firmware)?;
    // We currently decode only SLP_TYPa. Never guess a distinct PM1b type.
    if fadt.pm1b_control.is_some() || fadt.pm1_control_bytes < 2 {
        return Err(Error::Unsupported);
    }
    let current = enable_acpi(&fadt)?;
    let word = pm1_sleep_control(current, sleep_type, true).ok_or(Error::Unsupported)?;
    let _ = writeln!(
        console,
        "power: requesting ACPI S5 via PM1a {:#x}, control {word:#x}",
        fadt.pm1a_control
    );
    console.flush();
    let _interrupts = super::cpu::InterruptGuard::disable();
    // SAFETY: firmware control data is validated and the caller guaranteed system quiescence.
    unsafe { write_control(fadt.pm1a_control, word) };
    Ok(())
}

/// A conservative guard for the dedicated single-segment, single-CPU QEMU power test only.
/// This is deliberately not presented as production device-power policy.
#[cfg(feature = "hal-power-test-s5")]
pub fn test_vm_has_active_busmaster() -> bool {
    use nolx_pci_core::ConfigSpace;
    let inventory = nolx_pci_core::enumerate(&mut super::pci::ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .any(|d| super::pci::ConfigAccess.read_u32(d.bdf, 4) & 4 != 0)
}
