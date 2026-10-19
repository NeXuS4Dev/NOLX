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
    /// Flush, controller stop, retained DMA or PCI bus-master readback refused shutdown.
    Devices,
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

/// Freeze native device admission, flush storage, stop registered controllers and audit DMA.
/// BSP-only, segment-zero profile. This does not commit higher-layer dirty caches, park other
/// CPUs, stop firmware DMA, or implement S3. Callers must establish those wider preconditions.
/// Once registry checkout begins, failure leaves admission closed; a Busy refusal before checkout
/// does not start a shutdown. There is intentionally no automatic restart after failure.
pub fn quiesce_devices(console: &mut dyn HalConsole) -> Result<usize, Error> {
    let stopped = super::driver_start::quiesce(console).map_err(|_| Error::Devices)?;
    super::ps2::quiesce();
    if super::dma::live_pages() != 0 || !pci_dma_inactive()? {
        return Err(Error::Devices);
    }
    crate::mm::audit().map_err(|_| Error::Devices)?;
    let _ = writeln!(console, "power: quiesced {stopped} controller(s), DMA pages 0, complete segment-zero PCI bus-master readback inactive");
    Ok(stopped)
}

fn pci_dma_inactive() -> Result<bool, Error> {
    use nolx_pci_core::ConfigSpace;
    let platform = super::acpi::discover().ok_or(Error::Firmware)?;
    if platform
        .mcfg
        .is_some_and(|m| m.allocations().iter().any(|a| a.segment != 0))
    {
        return Err(Error::Unsupported);
    }
    // Scan all segment-zero buses, not merely the root-zero bridge tree. Refuse truncated or
    // malformed inventories; never disable an unowned bus master just to make the test pass.
    let inventory = nolx_pci_core::enumerate_range(&mut super::pci::ConfigAccess, 0, 255);
    if inventory.overflowed || inventory.refused_bridges != 0 {
        return Err(Error::Unsupported);
    }
    Ok(inventory
        .as_slice()
        .iter()
        .all(|d| super::pci::ConfigAccess.read_u32(d.bdf, 4) & 4 == 0))
}

/// Dedicated six-controller shutdown evidence; not a production executive shutdown dispatcher.
#[cfg(feature = "hal-power-quiesce-test")]
pub fn quiesce_selftest(console: &mut dyn HalConsole) {
    use super::driver_start;
    let running = driver_start::started().unwrap();
    let handles: alloc::vec::Vec<_> = running.into_iter().flatten().collect();
    assert_eq!(
        handles.len(),
        6,
        "shutdown fixture must exercise all six native drivers"
    );
    for name in ["virtio-blk", "ahci", "nvme", "e1000", "virtio-net", "xhci"] {
        assert!(handles.iter().any(|(_, n)| *n == name));
    }
    let mut infos = [None; 16];
    super::input::enumerate(&mut infos).unwrap();
    let readers: alloc::vec::Vec<_> = infos
        .into_iter()
        .flatten()
        .filter(|i| i.connected)
        .map(|i| (i, super::input::open(i.id).unwrap()))
        .collect();
    // These reserved pre-partition sectors are written after the boot filesystem checks.
    // Only the coordinator's later durable barrier makes this fixture's new writes durable.
    for &(dev, name) in &handles {
        let tag = match name {
            "virtio-blk" => 0xd1,
            "ahci" => 0xd2,
            "nvme" => 0xd3,
            _ => continue,
        };
        super::blockdev::write_sectors(super::blockdev::device(dev).unwrap(), 1780, &[tag; 512])
            .unwrap();
    }
    #[cfg(feature = "hal-power-quiesce-unowned-test")]
    let unowned = {
        use nolx_pci_core::ConfigSpace;
        let inventory = nolx_pci_core::enumerate(&mut super::pci::ConfigAccess);
        let bdf = inventory
            .as_slice()
            .iter()
            .find(|d| d.class == 3)
            .unwrap()
            .bdf;
        let command = super::pci::ConfigAccess.read_u32(bdf, 4) & 0xffff;
        super::pci::ConfigAccess.write_u32(bdf, 4, command | 4);
        assert_ne!(super::pci::ConfigAccess.read_u32(bdf, 4) & 4, 0);
        bdf
    };
    let pages = super::dma::live_pages();
    let result = quiesce_devices(console);
    let mut sector = [0; 512];
    assert_eq!(
        super::blockdev::read_sectors(0, 0, &mut sector),
        Err(nolxfs::DevError::Io(16))
    );
    for &(dev, _) in &handles {
        assert_eq!(driver_start::start(dev), Err(nolx_df_core::Error::Busy));
        assert_eq!(driver_start::recover(dev), Err(nolx_df_core::Error::Busy));
    }
    assert!(matches!(
        super::virtio_net::send_frame_on(0, &[0; 60]),
        Err(super::virtio_net::Failure::Busy)
    ));
    assert!(matches!(
        super::e1000::send_frame_on(0, &[0; 60]),
        Err(super::e1000::Failure::Busy)
    ));
    assert!(matches!(
        super::xhci::poll_on(0, console),
        Err(super::xhci::Failure::Busy)
    ));
    assert_eq!(
        driver_start::quiesce(console),
        Err(nolx_df_core::Error::Busy)
    );
    if result.is_ok() || cfg!(feature = "hal-power-quiesce-unowned-test") {
        for (prior, reader) in readers {
            let id = prior.id;
            let info = super::input::info(id).unwrap();
            assert!(!info.connected && info.snapshot.keys == [0; 4] && info.snapshot.buttons == 0);
            let mut neutral = prior.snapshot.keys == [0; 4] && prior.snapshot.buttons == 0;
            loop {
                match super::input::read(reader).unwrap() {
                    super::input::Read::Event(record) => {
                        assert_eq!(record.device, id);
                        match record.event.kind {
                            nolx_input_core::events::Kind::Key(key) => {
                                assert!(!key.pressed);
                                neutral |= key.usage == 4;
                            }
                            nolx_input_core::events::Kind::Pointer(pointer) => {
                                assert_eq!(pointer.buttons, 0);
                                neutral = true;
                            }
                        }
                    }
                    super::input::Read::Disconnected => break,
                    other => panic!("shutdown reader failed to retire: {other:?}"),
                }
            }
            assert!(neutral, "held USB input did not deliver neutral retirement");
            assert_eq!(
                super::input::read(reader),
                Ok(super::input::Read::Disconnected)
            );
            super::input::close(reader).unwrap();
        }
    }
    #[cfg(feature = "hal-power-quiesce-unowned-test")]
    {
        use nolx_pci_core::ConfigSpace;
        assert!(result.is_err());
        assert!(pages > 0);
        assert_eq!(super::dma::live_pages(), 0);
        assert_ne!(
            super::pci::ConfigAccess.read_u32(unowned, 4) & 4,
            0,
            "coordinator must not commandeer an unowned device"
        );
        let _ = writeln!(console, "power-test: PASS unowned refusal, registered DMA 0 but foreign PCI master unchanged, no S5, shutdown admission remains closed");
    }
    #[cfg(not(feature = "hal-power-quiesce-unowned-test"))]
    if cfg!(feature = "hal-power-quiesce-flush-fault-test") {
        assert!(result.is_err());
        assert_eq!(super::dma::live_pages(), pages);
        for &(dev, _) in &handles {
            assert_eq!(driver_start::state(dev), Ok(nolx_df_core::State::Started));
        }
        let _ = writeln!(console, "power-test: PASS flush refusal, all {pages} DMA pages retained, no stop or S5, shutdown admission remains closed");
    } else if cfg!(feature = "hal-power-quiesce-stop-fault-test") {
        assert!(result.is_err());
        assert!(super::dma::live_pages() > 0);
        let (dev, _) = handles.iter().find(|(_, name)| *name == "xhci").unwrap();
        assert_eq!(driver_start::state(*dev), Ok(nolx_df_core::State::Failed));
        assert_eq!(driver_start::remove(*dev), Err(nolx_df_core::Error::Busy));
        let _ = writeln!(console, "power-test: PASS stop refusal, DMA/MMIO pinned, no S5, shutdown admission remains closed");
    } else {
        assert_eq!(result.unwrap(), 6);
        assert!(pages > 0 && super::dma::live_pages() == 0);
        for &(dev, _) in &handles {
            assert_eq!(driver_start::state(dev), Ok(nolx_df_core::State::Stopped));
        }
        let _ = writeln!(console, "power-test: durable barriers and all six stops verified, new I/O/start refused, DMA {pages} -> 0; entering S5");
        // SAFETY: dedicated BSP-only PC/Q35 fixture; all users completed and filesystem/cache
        // selftests committed before idle. Admission is permanently frozen, every native driver
        // stopped, PS/2 retired, no DMA pages remain and all segment-zero PCI masters read inactive.
        unsafe {
            request_s5_quiesced(console).expect("checked S5 request failed");
        }
    }
}

/// Wait for real held USB key/button state before the shutdown hardware fixture proceeds.
#[cfg(feature = "hal-power-quiesce-test")]
pub fn quiesce_test_ready(console: &mut dyn HalConsole) -> bool {
    static ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !ARMED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        let _ = writeln!(
            console,
            "power-test: armed, require held USB key and mouse button"
        );
    }
    let mut infos = [None; 16];
    super::input::enumerate(&mut infos).unwrap();
    let mut key = false;
    let mut button = false;
    for info in infos
        .into_iter()
        .flatten()
        .filter(|i| i.connected && matches!(i.location, super::input::Location::Usb { .. }))
    {
        key |= info.snapshot.held(4);
        button |= info.snapshot.buttons == 1;
    }
    key && button
}
