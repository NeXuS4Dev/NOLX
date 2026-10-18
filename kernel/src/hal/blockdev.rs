//! The request path for the block devices. Every read and write the kernel issues to a disk goes
//! through here: it is submitted to the device's queue, dispatched, run, and completed. The queue
//! rules -- per-device queues, merging within a device, round-robin dispatch, and the in-flight
//! limit -- live in `nolx-block-core`, where they are host-tested.
//!
//! The boot path issues one request at a time, so each request is dispatched as soon as it is
//! submitted. The queue lock is never held while a request runs: a request may poll the device for
//! a long time, and the lock disables interrupts.

use core::sync::atomic::{AtomicUsize, Ordering};

use nolx_block_core::devices::{DeviceQueues, QueueError};
use nolx_block_core::queue::Request;
use nolx_hal_core::console::HalConsole;

use crate::sync::IrqMutex;

/// Preferred block ID for the first virtio controller.
pub const VIRTIO: usize = 0;
/// Preferred block ID for the first AHCI controller.
pub const AHCI: usize = 1;
/// Preferred block ID for the first NVMe controller.
pub const NVME: usize = 2;
const DEVICES: usize = super::driver_start::DEVICES + 3;
/// Requests a device's queue holds before it refuses one it cannot merge.
const DEPTH: usize = 8;
/// Requests a device may have in flight at once. The boot path needs one.
const IN_FLIGHT_LIMIT: u32 = 1;
/// The largest request, in sectors. Larger transfers are refused by the queue.
const MAX_SECTORS: u32 = 256;

static QUEUES: IrqMutex<DeviceQueues<DEVICES, DEPTH>> = IrqMutex::new(DeviceQueues::new());
static DISPATCHED: AtomicUsize = AtomicUsize::new(0);
static COMPLETED: AtomicUsize = AtomicUsize::new(0);

/// Run one request on device `dev`. The device is attached on its first request. `op` performs the
/// transfer, and its result is returned once the request has been completed.
///
/// # Errors
/// A queue refusal, converted by the caller's error type, or the error `op` returned.
pub fn run<T, E: From<QueueError>>(
    dev: usize,
    request: Request,
    op: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let dispatched = {
        let mut queues = QUEUES.lock();
        match queues.attach(dev, IN_FLIGHT_LIMIT) {
            Ok(()) | Err(QueueError::AlreadyAttached) => {}
            Err(error) => return Err(error.into()),
        }
        queues.submit(dev, request, MAX_SECTORS)?;
        queues.dispatch()
    };
    // The boot path has one request outstanding, so the queue hands back this request. Anything
    // else would mean a second request was in flight, which this path does not allow.
    let Some(next) = dispatched else {
        return Err(QueueError::Busy.into());
    };
    if next.device != dev || next.request != request {
        return Err(QueueError::Busy.into());
    }
    DISPATCHED.fetch_add(1, Ordering::Relaxed);

    let result = op();
    QUEUES.lock().complete(dev)?;
    COMPLETED.fetch_add(1, Ordering::Relaxed);
    result
}

/// Report how many requests went through the queues, and that each one completed.
pub fn report(console: &mut dyn HalConsole) {
    let dispatched = DISPATCHED.load(Ordering::Relaxed);
    let completed = COMPLETED.load(Ordering::Relaxed);
    let _ = writeln!(
        console,
        "blockdev: {dispatched} requests dispatched, {completed} completed"
    );
}

/// Detach a completely drained queue after its controller has stopped.
pub(crate) fn detach(dev: usize) -> Result<(), QueueError> {
    QUEUES.lock().detach(dev)
}

static BINDINGS: IrqMutex<(
    nolx_block_core::devices::DeviceBindings<DEVICES>,
    [Option<usize>; super::driver_start::DEVICES],
)> = IrqMutex::new((
    nolx_block_core::devices::DeviceBindings::new(),
    [None; super::driver_start::DEVICES],
));

/// Reserve a stable block ID before starting a storage instance. Preferred IDs remain 0/1/2.
pub(crate) fn bind_storage(handle: usize, backend: usize) -> Result<usize, QueueError> {
    if handle >= super::driver_start::DEVICES || backend > NVME {
        return Err(QueueError::UnknownDevice);
    }
    let mut bindings = BINDINGS.lock();
    if bindings.1[handle].is_some_and(|old| old != backend) {
        return Err(QueueError::UnknownDevice);
    }
    let id = bindings.0.bind(handle, backend, 3)?;
    bindings.1[handle] = Some(backend);
    Ok(id)
}
/// Stable queue identity for one framework handle, including while stopped/removed.
pub(crate) fn device(handle: usize) -> Option<usize> {
    BINDINGS.lock().0.find(handle)
}

fn with_device<T>(
    dev: usize,
    op: impl FnOnce(&mut dyn nolxfs::sector::SectorDevice) -> Result<T, nolxfs::DevError>,
) -> Result<T, nolxfs::DevError> {
    if super::cpu::current_ipl() != nolx_hal_core::ipl::Ipl::PASSIVE {
        return Err(nolxfs::DevError::Io(16));
    }
    let (owner, backend) = {
        let bindings = BINDINGS.lock();
        let owner = bindings.0.owner(dev);
        (owner, owner.and_then(|owner| bindings.1[owner]))
    };
    if let Some(handle) = owner {
        return super::driver_start::device_io_handle(handle, || match backend {
            Some(VIRTIO) => super::virtio_blk::with_disk(handle, op),
            Some(AHCI) => super::ahci::with_disk(handle, op),
            Some(NVME) => super::nvme::with_disk(handle, op),
            _ => Err(nolxfs::DevError::Io(19)),
        });
    }
    // Preserve Busy admission even for absent legacy devices during framework callbacks.
    let name = match dev {
        VIRTIO => "virtio-blk",
        AHCI => "ahci",
        NVME => "nvme",
        _ => return Err(nolxfs::DevError::Io(19)),
    };
    super::driver_start::device_io(name, || Err(nolxfs::DevError::Io(19)))
}
/// Read whole sectors through a retained controller after its probe has returned.
pub fn read_sectors(dev: usize, first: u64, out: &mut [u8]) -> Result<(), nolxfs::DevError> {
    with_device(dev, |disk| {
        nolxfs::sector::check_sector_io(disk.sector_count(), first, out.len())?;
        disk.read_sectors(first, out)
    })
}
/// Write whole sectors through the persistent device's normal queued path.
pub fn write_sectors(dev: usize, first: u64, data: &[u8]) -> Result<(), nolxfs::DevError> {
    with_device(dev, |disk| {
        nolxfs::sector::check_sector_io(disk.sector_count(), first, data.len())?;
        disk.write_sectors(first, data)
    })
}
/// Issue a durable barrier against a retained controller, without holding its state lock.
pub fn flush(dev: usize) -> Result<(), nolxfs::DevError> {
    with_device(dev, |disk| disk.flush())
}

/// Exercise retained hardware after all framework probe callbacks have returned.
pub fn retained_selftest(console: &mut dyn HalConsole) {
    for dev in 0..DEVICES {
        let backend = {
            let bindings = BINDINGS.lock();
            bindings.0.owner(dev).and_then(|owner| bindings.1[owner])
        };
        let name = match backend {
            Some(VIRTIO) => "virtio-blk",
            Some(AHCI) => "ahci",
            Some(NVME) => "nvme",
            _ => continue,
        };
        let mut sector = [0; 512];
        match read_sectors(dev, 0, &mut sector) {
            Ok(()) => {
                assert_eq!(
                    &sector[..b"NOLX-BLK-SECTOR0".len()],
                    b"NOLX-BLK-SECTOR0",
                    "persistent disk data mismatch"
                );
                let _ = writeln!(
                    console,
                    "blockdev: retained {name} read verified after probe return"
                );
            }
            Err(nolxfs::DevError::Io(19)) => {}
            Err(error) => panic!("persistent disk {name} failed: {error:?}"),
        }
    }
}

/// Invalid retained requests must never reach hardware or partially transfer a prefix.
#[cfg(feature = "hal-driver-lifecycle-test")]
pub(crate) fn bounds_selftest(dev: usize) {
    use nolxfs::DevError;
    let capacity = with_device(dev, |disk| Ok(disk.sector_count())).unwrap();
    let before = DISPATCHED.load(Ordering::Acquire);
    let mut sector = [0; 512];
    assert_eq!(
        read_sectors(dev, u64::MAX, &mut sector),
        Err(DevError::OutOfRange { block: u64::MAX })
    );
    assert_eq!(
        write_sectors(dev, u64::MAX, &sector),
        Err(DevError::OutOfRange { block: u64::MAX })
    );
    assert_eq!(
        read_sectors(dev, capacity, &mut sector),
        Err(DevError::OutOfRange { block: capacity })
    );
    assert_eq!(
        read_sectors(dev, 0, &mut []),
        Err(DevError::BadBufferLen { got: 0, want: 512 })
    );
    assert_eq!(
        write_sectors(dev, 0, &[0; 513]),
        Err(DevError::BadBufferLen {
            got: 513,
            want: 512
        })
    );
    assert_eq!(
        read_sectors(dev, capacity - 1, &mut [0; 1024]),
        Err(DevError::OutOfRange {
            block: capacity - 1
        })
    );
    assert_eq!(
        DISPATCHED.load(Ordering::Acquire),
        before,
        "invalid request reached hardware"
    );
}

/// Coordinator-only durable barrier while the framework is exclusively checked out and frozen.
/// This preserves the driver's normal queued FLUSH path without recursively entering admission.
pub(super) fn shutdown_flush(handle: usize) -> Result<(), nolxfs::DevError> {
    let backend = BINDINGS.lock().1.get(handle).copied().flatten();
    match backend {
        Some(VIRTIO) => super::virtio_blk::with_disk(handle, |disk| disk.flush()),
        Some(AHCI) => super::ahci::with_disk(handle, |disk| disk.flush()),
        Some(NVME) => super::nvme::with_disk(handle, |disk| disk.flush()),
        _ => Err(nolxfs::DevError::Io(19)),
    }
}
