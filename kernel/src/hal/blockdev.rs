//! The request path for the block devices. Every read and write the kernel issues to a disk goes
//! through here: it is submitted to the device's queue, dispatched, run, and completed. The queue
//! rules -- per-device queues, merging within a device, round-robin dispatch, and the in-flight
//! limit -- live in `nolx-block-core`, where they are host-tested.
//!
//! The boot path issues one request at a time, so each request is dispatched as soon as it is
//! submitted. The queue lock is never held while a request runs: a request may poll the device for
//! a long time, and the lock disables interrupts.

use core::fmt::Write;
use core::sync::atomic::{AtomicUsize, Ordering};

use nolx_block_core::devices::{DeviceQueues, QueueError};
use nolx_block_core::queue::Request;
use nolx_hal_core::console::HalConsole;

use crate::sync::IrqMutex;

/// The virtio block device's queue index.
pub const VIRTIO: usize = 0;
/// The AHCI SATA port's queue index.
pub const AHCI: usize = 1;
/// The NVMe namespace's queue index.
pub const NVME: usize = 2;
const DEVICES: usize = 3;
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
