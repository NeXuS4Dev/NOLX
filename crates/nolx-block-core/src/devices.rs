//! Per-device request queues.
//!
//! Each attached block device has its own queue of requests, so a merge never joins requests for two
//! devices. Dispatch is round-robin across the devices that have work and are under their in-flight
//! limit, so a device with a long queue or a slow backend cannot starve the others. A device's limit
//! is set when it is attached, and a request counts as in flight from dispatch until the driver
//! reports it complete.

use crate::queue::{Direction, PushError, Queue, Request};

/// Why a per-device queue operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueError {
    /// The device index is out of range, or the device is not attached.
    UnknownDevice,
    /// The device is already attached.
    AlreadyAttached,
    /// An in-flight limit of zero would never dispatch, so it is refused.
    ZeroLimit,
    /// The request is empty or larger than the size limit.
    Invalid,
    /// The device's queue has no free slot for a request it cannot merge.
    Full,
    /// The device still has queued or in-flight requests, so it cannot be detached.
    Busy,
    /// Completion was reported for a device with nothing in flight.
    NotInFlight,
}

/// A request handed to a driver, and the device it is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Dispatched {
    /// The device index.
    pub device: usize,
    /// The request.
    pub request: Request,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Slot<const DEPTH: usize> {
    queue: Queue<DEPTH>,
    limit: u32,
    in_flight: u32,
    attached: bool,
}

/// Queues for `DEVICES` devices, each holding up to `DEPTH` requests.
#[derive(Clone, Copy, Debug)]
pub struct DeviceQueues<const DEVICES: usize, const DEPTH: usize> {
    slots: [Slot<DEPTH>; DEVICES],
    next: usize,
}

impl<const DEVICES: usize, const DEPTH: usize> Default for DeviceQueues<DEVICES, DEPTH> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const DEVICES: usize, const DEPTH: usize> DeviceQueues<DEVICES, DEPTH> {
    /// No device attached.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [Slot {
                queue: Queue::new(),
                limit: 0,
                in_flight: 0,
                attached: false,
            }; DEVICES],
            next: 0,
        }
    }

    /// Attach device `dev`, allowing it at most `limit` requests in flight.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for an index out of range, [`QueueError::ZeroLimit`] for a zero
    /// limit, [`QueueError::AlreadyAttached`] for a device that is attached.
    pub fn attach(&mut self, dev: usize, limit: u32) -> Result<(), QueueError> {
        let slot = self.slots.get_mut(dev).ok_or(QueueError::UnknownDevice)?;
        if slot.attached {
            return Err(QueueError::AlreadyAttached);
        }
        if limit == 0 {
            return Err(QueueError::ZeroLimit);
        }
        slot.attached = true;
        slot.limit = limit;
        Ok(())
    }

    /// Detach device `dev`. It must have no queued and no in-flight requests.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for a device that is not attached; [`QueueError::Busy`] when it
    /// still has work.
    pub fn detach(&mut self, dev: usize) -> Result<(), QueueError> {
        let slot = self.attached_slot_mut(dev)?;
        if !slot.queue.is_empty() || slot.in_flight != 0 {
            return Err(QueueError::Busy);
        }
        slot.attached = false;
        slot.limit = 0;
        Ok(())
    }

    /// Queue `request` on device `dev`, merging it into that device's queue when possible.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for a device that is not attached; [`QueueError::Invalid`] and
    /// [`QueueError::Full`] as [`Queue::push`] reports them.
    pub fn submit(
        &mut self,
        dev: usize,
        request: Request,
        max_sectors: u32,
    ) -> Result<(), QueueError> {
        let slot = self.attached_slot_mut(dev)?;
        slot.queue
            .push(request, max_sectors)
            .map_err(|error| match error {
                PushError::Invalid => QueueError::Invalid,
                PushError::Full => QueueError::Full,
            })
    }

    /// Hand the next request to a driver. Devices are visited in round-robin order, starting after the
    /// device served last; a device is skipped when its queue is empty or it is at its limit.
    pub fn dispatch(&mut self) -> Option<Dispatched> {
        for offset in 0..DEVICES {
            let device = (self.next + offset) % DEVICES;
            let slot = &mut self.slots[device];
            if !slot.attached || slot.queue.is_empty() || slot.in_flight >= slot.limit {
                continue;
            }
            let request = slot.queue.pop()?;
            slot.in_flight += 1;
            self.next = (device + 1) % DEVICES;
            return Some(Dispatched { device, request });
        }
        None
    }

    /// Report that one request dispatched to `dev` has finished.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for a device that is not attached; [`QueueError::NotInFlight`]
    /// when nothing is in flight for it.
    pub fn complete(&mut self, dev: usize) -> Result<(), QueueError> {
        let slot = self.attached_slot_mut(dev)?;
        if slot.in_flight == 0 {
            return Err(QueueError::NotInFlight);
        }
        slot.in_flight -= 1;
        Ok(())
    }

    /// How many requests are queued for `dev`.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for a device that is not attached.
    pub fn pending(&self, dev: usize) -> Result<usize, QueueError> {
        Ok(self.attached_slot(dev)?.queue.len())
    }

    /// How many requests are in flight for `dev`.
    ///
    /// # Errors
    /// [`QueueError::UnknownDevice`] for a device that is not attached.
    pub fn in_flight(&self, dev: usize) -> Result<u32, QueueError> {
        Ok(self.attached_slot(dev)?.in_flight)
    }

    fn attached_slot(&self, dev: usize) -> Result<&Slot<DEPTH>, QueueError> {
        self.slots
            .get(dev)
            .filter(|slot| slot.attached)
            .ok_or(QueueError::UnknownDevice)
    }

    fn attached_slot_mut(&mut self, dev: usize) -> Result<&mut Slot<DEPTH>, QueueError> {
        self.slots
            .get_mut(dev)
            .filter(|slot| slot.attached)
            .ok_or(QueueError::UnknownDevice)
    }
}

/// A read request, for tests and callers that only need a direction.
#[must_use]
pub const fn read(lba: u64, sectors: u32) -> Request {
    Request {
        direction: Direction::Read,
        lba,
        sectors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX: u32 = 256;

    #[test]
    fn a_device_must_be_attached_once_with_a_nonzero_limit_to_take_work() {
        let mut queues: DeviceQueues<2, 4> = DeviceQueues::new();
        assert_eq!(queues.attach(2, 1), Err(QueueError::UnknownDevice));
        assert_eq!(queues.attach(0, 0), Err(QueueError::ZeroLimit));
        assert_eq!(queues.attach(0, 1), Ok(()));
        assert_eq!(queues.attach(0, 1), Err(QueueError::AlreadyAttached));
        assert_eq!(
            queues.submit(1, read(0, 1), MAX),
            Err(QueueError::UnknownDevice)
        );
        assert_eq!(queues.submit(0, read(0, 1), MAX), Ok(()));
        assert_eq!(
            queues.detach(0),
            Err(QueueError::Busy),
            "a request is queued"
        );
        let dispatched = queues.dispatch();
        assert_eq!(dispatched.map(|d| d.device), Some(0));
        assert_eq!(
            queues.detach(0),
            Err(QueueError::Busy),
            "a request is in flight"
        );
        assert_eq!(queues.complete(0), Ok(()));
        assert_eq!(queues.detach(0), Ok(()));
        assert_eq!(
            queues.submit(0, read(0, 1), MAX),
            Err(QueueError::UnknownDevice)
        );
        assert_eq!(queues.dispatch(), None);
    }

    #[test]
    fn requests_for_one_device_never_merge_with_another_devices() {
        let mut queues: DeviceQueues<2, 4> = DeviceQueues::new();
        queues.attach(0, 8).unwrap();
        queues.attach(1, 8).unwrap();
        queues.submit(0, read(0, 8), MAX).unwrap();
        queues.submit(1, read(8, 8), MAX).unwrap();
        assert_eq!(queues.pending(0), Ok(1));
        assert_eq!(queues.pending(1), Ok(1), "adjacent, but another device's");
        queues.submit(0, read(8, 8), MAX).unwrap();
        assert_eq!(queues.pending(0), Ok(1), "merged within device 0");
    }

    #[test]
    fn dispatch_is_round_robin_and_skips_empty_devices() {
        let mut queues: DeviceQueues<3, 4> = DeviceQueues::new();
        for dev in 0..3 {
            queues.attach(dev, 10).unwrap();
        }
        queues.submit(0, read(0, 1), MAX).unwrap();
        queues.submit(0, read(100, 1), MAX).unwrap();
        queues.submit(1, read(200, 1), MAX).unwrap();
        queues.submit(2, read(300, 1), MAX).unwrap();
        queues.submit(2, read(400, 1), MAX).unwrap();
        let mut order = [usize::MAX; 5];
        for slot in order.iter_mut() {
            *slot = queues.dispatch().unwrap().device;
        }
        assert_eq!(order, [0, 1, 2, 0, 2]);
        assert_eq!(queues.dispatch(), None);
    }

    #[test]
    fn a_device_at_its_limit_is_skipped_until_it_completes() {
        let mut queues: DeviceQueues<2, 4> = DeviceQueues::new();
        queues.attach(0, 1).unwrap();
        queues.attach(1, 4).unwrap();
        queues.submit(0, read(0, 1), MAX).unwrap();
        queues.submit(0, read(100, 1), MAX).unwrap();
        queues.submit(1, read(500, 1), MAX).unwrap();
        assert_eq!(queues.dispatch().map(|d| d.device), Some(0));
        assert_eq!(
            queues.dispatch().map(|d| d.device),
            Some(1),
            "the other device is not held up"
        );
        assert_eq!(queues.dispatch(), None, "device 0 is at its limit");
        assert_eq!(queues.in_flight(0), Ok(1));
        assert_eq!(queues.complete(0), Ok(()));
        assert_eq!(queues.dispatch().map(|d| d.device), Some(0));
        assert_eq!(queues.complete(0), Ok(()));
        assert_eq!(queues.complete(0), Err(QueueError::NotInFlight));
    }

    #[test]
    fn a_full_queue_refuses_its_own_device_only() {
        let mut queues: DeviceQueues<2, 2> = DeviceQueues::new();
        queues.attach(0, 8).unwrap();
        queues.attach(1, 8).unwrap();
        queues.submit(0, read(0, 1), MAX).unwrap();
        queues.submit(0, read(100, 1), MAX).unwrap();
        assert_eq!(queues.submit(0, read(200, 1), MAX), Err(QueueError::Full));
        assert_eq!(queues.submit(0, read(0, 0), MAX), Err(QueueError::Invalid));
        assert_eq!(
            queues.submit(1, read(0, 1), MAX),
            Ok(()),
            "device 1 has room"
        );
        assert_eq!(queues.pending(0), Ok(2));
    }
}
