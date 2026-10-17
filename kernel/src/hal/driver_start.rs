//! Support for drivers whose bring-up runs as a framework start operation.
//!
//! The framework's start callback receives a device handle but no console. So a
//! driver's bring-up writes its output into a [`Log`], the start callback keeps the log and the
//! result in the driver's static state, and the probe replays the log on the real console once the
//! framework has returned. The queue lock is never held while the bring-up runs.

use core::fmt::{self, Write};

use nolx_df_core::{Driver, Error, Registry};
use nolx_hal_core::console::HalConsole;

/// Bytes kept for one bring-up's output. Output past this is dropped, and the replay says so.
const CAPACITY: usize = 4096;

/// One bring-up's output, kept until the probe prints it.
pub struct Log {
    buf: [u8; CAPACITY],
    len: usize,
    cut: bool,
}

impl Log {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; CAPACITY],
            len: 0,
            cut: false,
        }
    }

    /// Write the kept output to the console, then the cut notice if output was dropped.
    pub fn replay(&self, console: &mut dyn HalConsole) {
        // The buffer only ever ends on a character boundary, so the slice is valid UTF-8.
        if let Ok(text) = core::str::from_utf8(&self.buf[..self.len]) {
            let _ = console.write_str(text);
        }
        if self.cut {
            let _ = writeln!(console, "driver output cut at {CAPACITY} bytes");
        }
        console.flush();
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = CAPACITY - self.len;
        // Keep whole characters only, so the kept text stays valid UTF-8.
        let take = (0..=room.min(text.len()))
            .rev()
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(0);
        self.buf[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        if take < text.len() {
            self.cut = true;
        }
        Ok(())
    }
}

impl HalConsole for Log {
    fn flush(&mut self) {}
}

/// The persistent system registry. Checkout releases the IRQ lock before any callback.
const DRIVERS: usize = 8;
/// The maximum persistent device records (failed/removed records retain diagnostic identity).
pub const DEVICES: usize = 16;
static REGISTRY: crate::sync::IrqMutex<Option<Registry<DRIVERS, DEVICES>>> =
    crate::sync::IrqMutex::new(Some(Registry::new()));

fn with_registry<T>(
    op: impl FnOnce(&mut Registry<DRIVERS, DEVICES>) -> Result<T, Error>,
) -> Result<T, Error> {
    if super::cpu::current_ipl() != nolx_hal_core::ipl::Ipl::PASSIVE {
        return Err(Error::Busy);
    }
    let mut registry = REGISTRY.lock().take().ok_or(Error::Busy)?;
    let result = op(&mut registry);
    *REGISTRY.lock() = Some(registry);
    result
}

/// Register (once by name), discover, bind and start a device in the persistent registry.
/// Recursive/concurrent framework operations are refused with Busy, never spun on in callbacks.
pub fn attach(driver: Driver, id: nolx_df_core::DeviceId) -> Result<usize, Error> {
    attach_prepared(driver, id, |_| Ok(()))
}

/// Initialize per-handle transport identity before binding/starting, outside the IRQ lock.
pub(crate) fn attach_prepared(
    driver: Driver,
    id: nolx_df_core::DeviceId,
    prepare: impl FnOnce(usize) -> Result<(), Error>,
) -> Result<usize, Error> {
    with_registry(|registry| {
        if registry.find_driver(driver.name).is_none() {
            registry.add_driver(driver)?;
        }
        let dev = registry.add_device(id)?;
        prepare(dev)?;
        registry.bind(dev)?;
        registry.start(dev)?;
        Ok(dev)
    })
}

/// Inspect a device after its original probe callback and stack have returned.
pub fn state(dev: usize) -> Result<nolx_df_core::State, Error> {
    with_registry(|registry| registry.state(dev))
}
/// Restart a bound/stopped device through its original registered dispatch table.
pub fn start(dev: usize) -> Result<(), Error> {
    with_registry(|registry| registry.start(dev))
}
/// Quiesce a started device through its original dispatch table.
pub fn stop(dev: usize) -> Result<(), Error> {
    with_registry(|registry| {
        registry.stop(dev)?;
        // SAFETY: successful native stop has detached IRQ-visible register users and transport
        // state, and the checked-out registry excludes concurrent public I/O/start/stop.
        unsafe {
            crate::mm::mmio::release_owned(dev);
        }
        Ok(())
    })
}
/// Remove a non-running device. Running devices must first successfully stop.
pub fn remove(dev: usize) -> Result<(), Error> {
    with_registry(|registry| registry.remove(dev))
}

/// A stable snapshot of started handles and their driver names, without retaining a registry lock.
pub fn started() -> Result<[Option<(usize, &'static str)>; DEVICES], Error> {
    with_registry(|registry| {
        let mut result = [None; DEVICES];
        for (dev, slot) in result.iter_mut().enumerate() {
            if registry.state(dev) == Ok(nolx_df_core::State::Started) {
                *slot = Some((dev, registry.driver_name(dev)?));
            }
        }
        Ok(result)
    })
}
/// Report persistent started devices; the snapshot outlives the probe that discovered them.
pub fn report(console: &mut dyn HalConsole) {
    if let Ok(devices) = started() {
        for (dev, name) in devices.into_iter().flatten() {
            let _ = writeln!(console, "driver: persistent handle {dev}, {name}, Started");
        }
    }
}

/// Serialize public device I/O against lifecycle callbacks without holding an IRQ lock.
/// A concurrent/reentrant stop must return Busy before changing the framework state.
pub(crate) fn device_operation<T, E>(
    name: &str,
    op: impl FnOnce() -> Result<T, E>,
    refused: impl FnOnce(Error) -> E,
) -> Result<T, E> {
    with_registry(|registry| {
        let running = (0..DEVICES).any(|dev| {
            registry.state(dev) == Ok(nolx_df_core::State::Started)
                && registry.driver_name(dev) == Ok(name)
        });
        if !running {
            return Err(Error::UnknownDevice);
        }
        Ok(op())
    })
    .map_err(refused)?
}

/// Sector-transport specialization of lifecycle exclusion.
pub(crate) fn device_io<T>(
    name: &str,
    op: impl FnOnce() -> Result<T, nolxfs::DevError>,
) -> Result<T, nolxfs::DevError> {
    device_operation(name, op, |error| {
        nolxfs::DevError::Io(if error == Error::Busy { 16 } else { 19 })
    })
}

/// Admit I/O against the selected handle, never another started instance of the same driver.
pub(crate) fn device_io_handle<T>(
    dev: usize,
    op: impl FnOnce() -> Result<T, nolxfs::DevError>,
) -> Result<T, nolxfs::DevError> {
    with_registry(|registry| {
        if registry.state(dev) != Ok(nolx_df_core::State::Started) {
            return Err(Error::UnknownDevice);
        }
        Ok(op())
    })
    .map_err(|error| nolxfs::DevError::Io(if error == Error::Busy { 16 } else { 19 }))?
}

/// Hardware lifecycle test: persistent handles, callback exclusion, restart and DMA reclamation.
#[cfg(feature = "hal-driver-lifecycle-test")]
pub fn lifecycle_selftest(console: &mut dyn HalConsole) {
    use nolx_df_core::{DeviceId, Dispatch, Match, State};
    fn reenter(dev: usize) -> bool {
        let mut sector = [0; 512];
        state(dev) == Err(Error::Busy)
            && super::blockdev::read_sectors(0, 0, &mut sector) == Err(nolxfs::DevError::Io(16))
    }
    static RULES: &[Match] = &[Match::by_ids(0xffff, 0xfffe)];
    let synthetic = attach(
        Driver {
            name: "framework-reentry-test",
            matches: RULES,
            ops: Dispatch {
                probe: |_| true,
                start: reenter,
                stop: |_| true,
            },
        },
        DeviceId {
            vendor: 0xffff,
            device: 0xfffe,
            class: 0xff,
            subclass: 0,
            prog_if: 0,
        },
    )
    .expect("framework reentry exclusion failed");
    stop(synthetic).unwrap();
    remove(synthetic).unwrap();
    let _ = writeln!(
        console,
        "driver: recursive framework and public I/O callbacks refused with Busy"
    );
    super::nvme::deadline_selftest(console);
    crate::mm::mmio::lifecycle_selftest(console);
    #[cfg(feature = "hal-driver-multi-test")]
    multi_selftest(console);
    let devices = started().unwrap();
    let mut tested = 0;
    for (handle, name) in devices.into_iter().flatten() {
        let (disk, pages) = match name {
            "virtio-blk" => (
                Some(super::blockdev::device(handle).expect("missing instance binding")),
                2,
            ),
            "ahci" => (
                Some(super::blockdev::device(handle).expect("missing AHCI binding")),
                2,
            ),
            "nvme" => (
                Some(super::blockdev::device(handle).expect("missing NVMe binding")),
                6,
            ),
            "e1000" => (None, 11),
            "virtio-net" => (None, 4),
            _ => continue,
        };
        assert_eq!(remove(handle), Err(Error::Busy));
        for cycle in 1..=3 {
            let before = super::dma::live_pages();
            let mappings_before = crate::mm::mmio::live_mappings();
            stop(handle).expect("driver hardware quiesce failed");
            assert_eq!(state(handle), Ok(State::Stopped));
            assert!(
                crate::mm::mmio::live_mappings() < mappings_before,
                "driver did not release MMIO mappings"
            );
            assert_eq!(
                super::dma::live_pages() + pages,
                before,
                "driver did not reclaim all owned DMA"
            );
            if let Some(disk) = disk {
                let mut sector = [0; 512];
                assert_eq!(
                    super::blockdev::read_sectors(disk, 0, &mut sector),
                    Err(nolxfs::DevError::Io(19))
                );
            } else if name == "virtio-net" {
                assert!(matches!(
                    super::virtio_net::send_frame(&[0; 60]),
                    Err(super::virtio_net::Failure::NoDevice)
                ));
            }
            assert_eq!(stop(handle), Err(Error::BadState));
            let restarted = start(handle);
            if restarted.is_err() && name == "e1000" {
                super::e1000::lifecycle_diagnostic(console);
            }
            restarted.expect("driver restart failed");
            assert_eq!(state(handle), Ok(State::Started));
            assert_eq!(
                crate::mm::mmio::live_mappings(),
                mappings_before,
                "restart leaked MMIO mappings"
            );
            assert_eq!(super::dma::live_pages(), before, "restart leaked DMA pages");
            #[cfg(feature = "hal-driver-multi-test")]
            if disk.is_some() {
                verify_multi_instance(handle);
            }
            if let Some(disk) = disk {
                super::blockdev::bounds_selftest(disk);
                let mut sector = [0; 512];
                super::blockdev::read_sectors(disk, 0, &mut sector)
                    .expect("restarted disk read failed");
                assert_eq!(&sector[..b"NOLX-BLK-SECTOR0".len()], b"NOLX-BLK-SECTOR0");
                let written = [0xc0 + cycle as u8; 512];
                super::blockdev::write_sectors(disk, 1700, &written)
                    .expect("retained queued write failed");
                super::blockdev::flush(disk).expect("retained flush failed");
                super::blockdev::read_sectors(disk, 1700, &mut sector)
                    .expect("retained readback failed");
                assert_eq!(sector, written);
            }
            let _ = writeln!(console, "driver: {name} cycle {cycle}: Started -> Stopped -> Started, {pages} DMA pages reclaimed and reacquired, I/O verified");
        }
        stop(handle).unwrap();
        remove(handle).unwrap();
        assert_eq!(state(handle), Ok(State::Removed));
        assert_eq!(start(handle), Err(Error::BadState));
        tested += 1;
        let _ = writeln!(
            console,
            "driver: {name} MMIO mappings released and reused across all three cycles"
        );
        let _ = writeln!(console, "driver: {name} Removed, restart refused");
    }
    assert!(tested > 0, "lifecycle hardware test had no devices");
    let _ = writeln!(
        console,
        "driver: lifecycle selftest complete, {tested} device(s), DMA pages pinned {}",
        super::dma::live_pages()
    );
}

#[cfg(feature = "hal-driver-multi-test")]
static MULTI_TAGS: [core::sync::atomic::AtomicU8; DEVICES] =
    [const { core::sync::atomic::AtomicU8::new(0) }; DEVICES];

#[cfg(feature = "hal-driver-multi-test")]
fn verify_multi_instance(handle: usize) {
    use core::sync::atomic::Ordering;
    let tag = MULTI_TAGS[handle].load(Ordering::Acquire);
    assert_ne!(tag, 0, "missing instance identity");
    let disk = super::blockdev::device(handle).unwrap();
    let mut data = [0; 512];
    super::blockdev::read_sectors(disk, 1740, &mut data).unwrap();
    assert_eq!(
        data, [tag; 512],
        "controller identity changed after peer I/O/restart"
    );
    super::blockdev::read_sectors(disk, 1750, &mut data).unwrap();
    assert_eq!(data, [tag ^ 0xa0; 512], "I/O crossed controller boundaries");
}

/// Distinct host-seeded media identities prove binding/queue/IRQ isolation, not just counts.
#[cfg(feature = "hal-driver-multi-test")]
fn multi_selftest(console: &mut dyn HalConsole) {
    use core::sync::atomic::Ordering;
    let devices = started().unwrap();
    let dma_before_probe = super::dma::live_pages();
    let mappings_before_probe = crate::mm::mmio::live_mappings();
    super::virtio_blk::probe(console);
    super::ahci::probe(console);
    super::nvme::probe(console);
    assert_eq!(
        started().unwrap(),
        devices,
        "duplicate enumeration created another handle"
    );
    assert_eq!(super::dma::live_pages(), dma_before_probe);
    assert_eq!(crate::mm::mmio::live_mappings(), mappings_before_probe);
    let _ = writeln!(
        console,
        "driver: duplicate storage enumeration preserved handles and resources"
    );
    let storage: [Option<(usize, &'static str)>; DEVICES] =
        devices.map(|entry| entry.filter(|(handle, _)| super::blockdev::device(*handle).is_some()));
    let count = storage.iter().flatten().count();
    assert!(
        count >= 2,
        "multi-controller fixture needs at least two storage devices"
    );
    let mut seen = [false; 256];
    for &(handle, name) in storage.iter().flatten() {
        let disk = super::blockdev::device(handle).unwrap();
        let mut data = [0; 512];
        super::blockdev::read_sectors(disk, 1740, &mut data).unwrap();
        let tag = data[0];
        assert!(
            tag != 0 && !seen[usize::from(tag)],
            "two framework handles selected the same media"
        );
        assert_eq!(data, [tag; 512]);
        seen[usize::from(tag)] = true;
        MULTI_TAGS[handle].store(tag, Ordering::Release);
        super::blockdev::write_sectors(disk, 1750, &[tag ^ 0xa0; 512]).unwrap();
        super::blockdev::flush(disk).unwrap();
        let _ = writeln!(
            console,
            "driver: multi instance {handle} {name}, block {disk}, media tag {tag}"
        );
    }
    for &(victim, _) in storage.iter().flatten() {
        let dma = super::dma::live_pages();
        let mappings = crate::mm::mmio::live_mappings();
        stop(victim).unwrap();
        let disk = super::blockdev::device(victim).unwrap();
        assert_eq!(
            super::blockdev::read_sectors(disk, 1740, &mut [0; 512]),
            Err(nolxfs::DevError::Io(19))
        );
        assert_eq!(
            super::blockdev::write_sectors(disk, 1750, &[0; 512]),
            Err(nolxfs::DevError::Io(19))
        );
        for &(peer, _) in storage.iter().flatten() {
            if peer != victim {
                verify_multi_instance(peer);
            }
        }
        start(victim).unwrap();
        verify_multi_instance(victim);
        assert_eq!(super::dma::live_pages(), dma);
        assert_eq!(crate::mm::mmio::live_mappings(), mappings);
    }
    let _ = writeln!(console, "driver: multi-controller isolation verified: {count} storage devices, unique media, stopped-target refusal, peer I/O and restart identity");
}
