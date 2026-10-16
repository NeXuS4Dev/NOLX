//! Support for drivers whose bring-up runs as a framework start operation.
//!
//! The framework's start callback is a plain function with no console and no arguments. So a
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
    with_registry(|registry| {
        if registry.find_driver(driver.name).is_none() {
            registry.add_driver(driver)?;
        }
        let dev = registry.add_device(id)?;
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
    with_registry(|registry| registry.stop(dev))
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
    let devices = started().unwrap();
    let mut tested = 0;
    for (handle, name) in devices.into_iter().flatten() {
        let (disk, pages) = match name {
            "virtio-blk" => (Some(super::blockdev::VIRTIO), 2),
            "ahci" => (Some(super::blockdev::AHCI), 2),
            "nvme" => (Some(super::blockdev::NVME), 6),
            "e1000" => (None, 11),
            "virtio-net" => (None, 4),
            _ => continue,
        };
        assert_eq!(remove(handle), Err(Error::Busy));
        for cycle in 1..=3 {
            let before = super::dma::live_pages();
            stop(handle).expect("driver hardware quiesce failed");
            assert_eq!(state(handle), Ok(State::Stopped));
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
            assert_eq!(super::dma::live_pages(), before, "restart leaked DMA pages");
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
        let _ = writeln!(console, "driver: {name} Removed, restart refused");
    }
    assert!(tested > 0, "lifecycle hardware test had no devices");
    let _ = writeln!(
        console,
        "driver: lifecycle selftest complete, {tested} device(s), DMA pages pinned {}",
        super::dma::live_pages()
    );
}
