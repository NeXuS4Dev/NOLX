//! Bounded device-owned input objects with independent reader queues and explicit resync.
//! No allocation, callbacks or synchronization: the kernel serializes this registry.
//! Resync repairs held keys/buttons; discarded relative displacement is reported as loss,
//! not reconstructed as fictitious pointer motion. IDs are scoped to a registry instance.
use crate::{
    events::{Event, EventQueue, Kind, PointerEvent, Source},
    KeyEvent,
};

/// Opaque registry-local device identity; never a USB slot number or FIFO position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId {
    slot: usize,
    generation: u64,
}
impl DeviceId {
    /// Registry slot, for diagnostics only.
    pub const fn slot(self) -> usize {
        self.slot
    }
    /// Non-wrapping incarnation, for diagnostics only.
    pub const fn generation(self) -> u64 {
        self.generation
    }
}
/// Opaque generation-checked subscription; each reader has its own queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReaderId {
    slot: usize,
    generation: u64,
}
/// Input payload class, enforced at publication.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// HID-usage key transitions.
    Keyboard,
    /// Relative pointer motion and buttons.
    Pointer,
}
/// Producer location; USB controller is packed segment<<16 | bus<<8 | device<<3 | function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Location {
    /// Fixed i8042 keyboard port.
    Ps2Keyboard,
    /// Fixed i8042 auxiliary port.
    Ps2Mouse,
    /// Direct-root USB port; USB addresses/slots may be reused independently.
    Usb {
        /// Encoded segment-qualified PCI controller.
        controller: u32,
        /// One-based root port.
        port: u8,
    },
}
/// Current device state used to repair a reader after lost records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// HID usages currently held, in four little bit-numbered groups of 64.
    pub keys: [u64; 4],
    /// Current left/right/middle buttons; zero for keyboards.
    pub buttons: u8,
}
impl Snapshot {
    const EMPTY: Self = Self {
        keys: [0; 4],
        buttons: 0,
    };
    /// Whether this HID usage is held on this device, not on another keyboard.
    pub fn held(self, usage: u8) -> bool {
        self.keys[usize::from(usage) / 64] & (1u64 << (usage % 64)) != 0
    }
}
/// Enumeration/query result, retained after disconnect while a reader still owns a reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Info {
    /// Device incarnation.
    pub id: DeviceId,
    /// Producer class/source.
    pub source: Source,
    /// Accepted payload class.
    pub class: Class,
    /// Physical producer location.
    pub location: Location,
    /// False after removal; new subscriptions and writes are then refused.
    pub connected: bool,
    /// Latest state, including events lost by slow readers.
    pub snapshot: Snapshot,
    /// Saturating per-device publication counter; not an identity or authentication token.
    pub sequence: u64,
}
/// One immutable publication delivered independently to every existing reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Record {
    /// Exact device incarnation.
    pub device: DeviceId,
    /// Source-tagged payload.
    pub event: Event,
    /// Per-device sequence, shared by readers of this publication.
    pub sequence: u64,
}
/// Nonblocking read result. Overflow is explicit and precedes any subsequent event/EOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Read {
    /// Next record in this subscription's FIFO.
    Event(Record),
    /// Records were discarded; replace held state with the snapshot before continuing.
    Resync {
        /// Exact device incarnation.
        device: DeviceId,
        /// Discarded records since the last resync, saturating.
        dropped: u64,
        /// Latest state, not an obsolete queued snapshot.
        snapshot: Snapshot,
        /// Snapshot publication counter.
        sequence: u64,
    },
    /// Connected object with no records available.
    Empty,
    /// Disconnected object, after all queued neutral transitions/resync have drained.
    Disconnected,
}
/// Failed operation; failures never mutate another incarnation's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Unknown, closed or reused handle.
    Stale,
    /// Existing object is no longer accepting producers/new readers.
    Disconnected,
    /// Bounded device/reader capacity or non-wrapping generations exhausted.
    Capacity,
    /// A connected producer already owns this physical location.
    Duplicate,
    /// Source, location, class or payload does not agree.
    Invalid,
}
struct DeviceSlot {
    generation: u64,
    info: Option<Info>,
}
impl DeviceSlot {
    const fn new() -> Self {
        Self {
            generation: 0,
            info: None,
        }
    }
}
struct Reader<const Q: usize> {
    device: DeviceId,
    queue: EventQueue<Q>,
    sequences: [u64; Q],
    head: usize,
    len: usize,
    lost: u64,
}
struct ReaderSlot<const Q: usize> {
    generation: u64,
    reader: Option<Reader<Q>>,
}
impl<const Q: usize> ReaderSlot<Q> {
    const fn new() -> Self {
        Self {
            generation: 0,
            reader: None,
        }
    }
}
/// Device objects, independent subscriptions and fixed-capacity per-reader event queues.
/// Disconnected objects stay alive until their last reader closes. No generation ever wraps;
/// exhausted slots are retired. Readers must close explicitly to return bounded capacity.
pub struct Registry<const D: usize, const R: usize, const Q: usize> {
    devices: [DeviceSlot; D],
    readers: [ReaderSlot<Q>; R],
}
impl<const D: usize, const R: usize, const Q: usize> Registry<D, R, Q> {
    /// Empty registry, suitable for a synchronized static.
    pub const fn new() -> Self {
        Self {
            devices: [const { DeviceSlot::new() }; D],
            readers: [const { ReaderSlot::new() }; R],
        }
    }
    fn device(&self, id: DeviceId) -> Result<&Info, Error> {
        self.devices
            .get(id.slot)
            .filter(|s| s.generation == id.generation)
            .and_then(|s| s.info.as_ref())
            .ok_or(Error::Stale)
    }
    fn reader(&mut self, id: ReaderId) -> Result<&mut Reader<Q>, Error> {
        self.readers
            .get_mut(id.slot)
            .filter(|s| s.generation == id.generation)
            .and_then(|s| s.reader.as_mut())
            .ok_or(Error::Stale)
    }
    /// Enumerate live objects, including disconnected objects still referenced by readers.
    pub fn devices(&self) -> impl Iterator<Item = Info> + '_ {
        self.devices.iter().filter_map(|s| s.info)
    }
    /// Query one exact device incarnation.
    pub fn info(&self, id: DeviceId) -> Result<Info, Error> {
        self.device(id).copied()
    }
    /// Register a connected producer; source/location/class are validated before allocation.
    pub fn attach(
        &mut self,
        source: Source,
        class: Class,
        location: Location,
    ) -> Result<DeviceId, Error> {
        let valid = match (source, class, location) {
            (Source::Ps2Keyboard, Class::Keyboard, Location::Ps2Keyboard)
            | (Source::Ps2Mouse, Class::Pointer, Location::Ps2Mouse) => true,
            (Source::UsbHid, _, Location::Usb { port, .. }) => port != 0,
            _ => false,
        };
        if !valid {
            return Err(Error::Invalid);
        }
        if self
            .devices()
            .any(|i| i.connected && i.location == location)
        {
            return Err(Error::Duplicate);
        }
        let (slot, entry) = self
            .devices
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.info.is_none() && s.generation < u64::MAX)
            .ok_or(Error::Capacity)?;
        entry.generation += 1;
        let id = DeviceId {
            slot,
            generation: entry.generation,
        };
        entry.info = Some(Info {
            id,
            source,
            class,
            location,
            connected: true,
            snapshot: Snapshot::EMPTY,
            sequence: 0,
        });
        Ok(id)
    }
    /// Open an independent reader starting with future publications; query `info` for initial state.
    pub fn open(&mut self, device: DeviceId) -> Result<ReaderId, Error> {
        if !self.device(device)?.connected {
            return Err(Error::Disconnected);
        }
        let (slot, entry) = self
            .readers
            .iter_mut()
            .enumerate()
            .find(|(_, s)| s.reader.is_none() && s.generation < u64::MAX)
            .ok_or(Error::Capacity)?;
        entry.generation += 1;
        entry.reader = Some(Reader {
            device,
            queue: EventQueue::new(),
            sequences: [0; Q],
            head: 0,
            len: 0,
            lost: 0,
        });
        Ok(ReaderId {
            slot,
            generation: entry.generation,
        })
    }
    /// Atomically open a future-event subscription and capture its initial state. The caller
    /// must use the registry's same external lock for this entire operation.
    pub fn subscribe(&mut self, device: DeviceId) -> Result<(ReaderId, Info), Error> {
        let initial = self.info(device)?;
        let reader = self.open(device)?;
        Ok((reader, initial))
    }
    /// Publish to this device only; slow readers cannot consume or block another reader's records.
    /// When a reader overflows, discard its old queue and suppress records until it reads Resync.
    pub fn publish(&mut self, id: DeviceId, kind: Kind) -> Result<(), Error> {
        let old = *self.device(id)?;
        if !old.connected {
            return Err(Error::Disconnected);
        }
        if !matches!(
            (old.class, kind),
            (Class::Keyboard, Kind::Key(_))
                | (
                    Class::Pointer,
                    Kind::Pointer(PointerEvent { buttons: 0..=7, .. })
                )
        ) {
            return Err(Error::Invalid);
        }
        let info = self.devices[id.slot].info.as_mut().unwrap();
        match kind {
            Kind::Key(key) => {
                let word = &mut info.snapshot.keys[usize::from(key.usage) / 64];
                let bit = 1u64 << (key.usage % 64);
                if key.pressed {
                    *word |= bit
                } else {
                    *word &= !bit
                }
            }
            Kind::Pointer(p) => info.snapshot.buttons = p.buttons,
        }
        info.sequence = info.sequence.saturating_add(1);
        let sequence = info.sequence;
        let event = Event {
            source: info.source,
            kind,
        };
        for reader in self
            .readers
            .iter_mut()
            .filter_map(|s| s.reader.as_mut())
            .filter(|r| r.device == id)
        {
            if reader.lost != 0 {
                reader.lost = reader.lost.saturating_add(1);
                continue;
            }
            if !reader.queue.push(event) {
                reader.lost = (reader.len as u64).saturating_add(1);
                reader.queue = EventQueue::new();
                reader.head = 0;
                reader.len = 0;
                continue;
            }
            reader.sequences[(reader.head + reader.len) % Q] = sequence;
            reader.len += 1;
        }
        Ok(())
    }
    /// Read only this subscription. Resync uses current device state even after disconnect.
    pub fn read(&mut self, id: ReaderId) -> Result<Read, Error> {
        let reader = self.reader(id)?;
        let device = reader.device;
        if reader.lost != 0 {
            let dropped = reader.lost;
            reader.lost = 0;
            let info = self.device(device)?;
            return Ok(Read::Resync {
                device,
                dropped,
                snapshot: info.snapshot,
                sequence: info.sequence,
            });
        }
        if let Some(event) = reader.queue.pop() {
            let sequence = reader.sequences[reader.head];
            reader.head = (reader.head + 1) % Q;
            reader.len -= 1;
            return Ok(Read::Event(Record {
                device,
                event,
                sequence,
            }));
        }
        Ok(if self.device(device)?.connected {
            Read::Empty
        } else {
            Read::Disconnected
        })
    }
    /// Close one subscription; closed handles cannot read/close a later reader in the same slot.
    pub fn close(&mut self, id: ReaderId) -> Result<(), Error> {
        let device = self.reader(id)?.device;
        self.readers[id.slot].reader = None;
        self.reclaim(device);
        Ok(())
    }
    fn reclaim(&mut self, id: DeviceId) {
        if self.device(id).is_ok_and(|i| !i.connected)
            && !self
                .readers
                .iter()
                .filter_map(|s| s.reader.as_ref())
                .any(|r| r.device == id)
        {
            self.devices[id.slot].info = None;
        }
    }
    /// Retire a producer, synthesizing per-device neutral transitions before terminal reads.
    /// Existing readers keep the object alive; new opens and producer submissions are refused.
    pub fn disconnect(&mut self, id: DeviceId) -> Result<(), Error> {
        let info = *self.device(id)?;
        if !info.connected {
            return Err(Error::Disconnected);
        }
        match info.class {
            Class::Keyboard => {
                for usage in 0..=255u8 {
                    if info.snapshot.held(usage) {
                        self.publish(
                            id,
                            Kind::Key(KeyEvent {
                                usage,
                                pressed: false,
                            }),
                        )?;
                    }
                }
            }
            Class::Pointer => self.publish(
                id,
                Kind::Pointer(PointerEvent {
                    dx: 0,
                    dy: 0,
                    buttons: 0,
                }),
            )?,
        }
        self.devices[id.slot].info.as_mut().unwrap().connected = false;
        self.reclaim(id);
        Ok(())
    }
}
impl<const D: usize, const R: usize, const Q: usize> Default for Registry<D, R, Q> {
    fn default() -> Self {
        Self::new()
    }
}
#[cfg(test)]
mod tests;
