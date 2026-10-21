//! Kernel-facing input object API. Independent subscriptions, not process handles/syscalls.
//! Producers never call consumers. Console reads are one subscription, outside the state lock.
use crate::sync::IrqMutex;
use nolx_hal_core::console::HalConsole;
pub use nolx_input_core::objects::{Class, DeviceId, Error, Info, Location, Read, ReaderId};
use nolx_input_core::{
    events::{Kind, Source},
    objects::Registry,
};
const DEVICES: usize = 16;
/// The Executive binding: names, security, handles and the waitable for these objects.
pub(crate) mod executive;
pub(crate) use executive::{bind_process, process_token, release_process};
struct State {
    registry: Registry<DEVICES, 32, 64>,
    console: [Option<(DeviceId, ReaderId)>; DEVICES],
}
static STATE: IrqMutex<State> = IrqMutex::new(State {
    registry: Registry::new(),
    console: [None; DEVICES],
});
/// Copy a complete bounded enumeration; insufficient output capacity refuses without a prefix.
/// At most the first 16 output slots are touched, regardless of caller buffer length.
pub fn enumerate(out: &mut [Option<Info>]) -> Result<usize, Error> {
    let state = STATE.lock();
    let count = state.registry.devices().count();
    if out.len() < count {
        return Err(Error::Capacity);
    }
    // Bound time under the ISR-shared lock even if a trusted caller supplies a huge slice.
    let limit = out.len().min(DEVICES);
    out[..limit].fill(None);
    for (index, info) in state.registry.devices().enumerate() {
        out[index] = Some(info);
    }
    Ok(count)
}
/// Query the exact connected or retained-disconnected incarnation.
pub fn info(id: DeviceId) -> Result<Info, Error> {
    STATE.lock().registry.info(id)
}
/// Open a separate nonblocking reader; future publications never steal the console's records.
pub fn open(id: DeviceId) -> Result<ReaderId, Error> {
    STATE.lock().registry.open(id)
}
/// Open a reader and capture its initial state under the same lock, avoiding a query/open gap.
pub fn subscribe(id: DeviceId) -> Result<(ReaderId, Info), Error> {
    STATE.lock().registry.subscribe(id)
}
/// Read one event, explicit loss repair, Empty or terminal Disconnected.
pub fn read(reader: ReaderId) -> Result<Read, Error> {
    STATE.lock().registry.read(reader)
}
/// Close a reader. Its generation is invalid even if the slot is subsequently reused.
pub fn close(reader: ReaderId) -> Result<(), Error> {
    STATE.lock().registry.close(reader)
}
pub(super) fn register(
    source: Source,
    class: Class,
    location: Location,
    console: &mut dyn HalConsole,
) -> Result<DeviceId, Error> {
    let mut state = STATE.lock();
    let index = state
        .console
        .iter()
        .position(Option::is_none)
        .ok_or(Error::Capacity)?;
    let id = state.registry.attach(source, class, location)?;
    let reader = match state.registry.open(id) {
        Ok(r) => r,
        Err(e) => {
            let _ = state.registry.disconnect(id);
            return Err(e);
        }
    };
    state.console[index] = Some((id, reader));
    drop(state);
    let _=writeln!(console,"input: object attached device={}:{} source={source:?} class={class:?} location={location:?}",id.slot(),id.generation());
    // The Executive object is best-effort: a namespace or header shortage must not take the
    // kernel-side device with it, so a refusal is reported and the object still works.
    match executive::publish(id, class) {
        Ok(name) => {
            let _=writeln!(console,"input: executive object \\Device\\{name} published type={} access=read|synchronize",executive::TYPE_NAME);
        }
        Err(error) => {
            let _ = writeln!(console, "input: executive binding refused: {error:?}");
        }
    }
    Ok(id)
}
pub(super) fn publish(id: DeviceId, kind: Kind) -> Result<(), Error> {
    let published = STATE.lock().registry.publish(id, kind);
    if published.is_ok() {
        // Set the waitable before the record is observable, so a waiter that wakes on it can
        // always read the report that made it ready.
        executive::signal(id);
    }
    published
}
pub(super) fn disconnect(id: DeviceId) -> Result<(), Error> {
    let result = STATE.lock().registry.disconnect(id);
    // The name goes away with the device: a stale path must not find a retired incarnation.
    executive::withdraw(id);
    result
}
/// Consume console subscriptions through the public read API, without holding the registry
/// while printing. Other readers retain their own queues and disconnected object references.
pub fn report_events(console: &mut dyn HalConsole) {
    for index in 0..DEVICES {
        let Some((id, reader)) = STATE.lock().console[index] else {
            continue;
        };
        for _ in 0..65 {
            match read(reader) {
                Ok(Read::Event(record)) => {
                    assert_eq!(record.device, id);
                    let source = match record.event.source {
                        Source::Ps2Keyboard | Source::Ps2Mouse => "ps2",
                        Source::UsbHid => "usb",
                    };
                    match record.event.kind {
                        Kind::Key(k) => {
                            let _=writeln!(console,"input: {source} keyboard usage={:#04x} pressed={} device={}:{} sequence={}",k.usage,k.pressed,id.slot(),id.generation(),record.sequence);
                        }
                        Kind::Pointer(p) => {
                            let _=writeln!(console,"input: {source} mouse dx={} dy={} buttons={} device={}:{} sequence={}",p.dx,p.dy,p.buttons,id.slot(),id.generation(),record.sequence);
                        }
                    }
                }
                Ok(Read::Resync {
                    dropped,
                    snapshot,
                    sequence,
                    ..
                }) => {
                    let _=writeln!(console,"input: object resync device={}:{} dropped={dropped} keys={:x?} buttons={} sequence={sequence}",id.slot(),id.generation(),snapshot.keys,snapshot.buttons);
                }
                Ok(Read::Disconnected) => {
                    close(reader).expect("console input subscription ownership");
                    STATE.lock().console[index] = None;
                    let _ = writeln!(
                        console,
                        "input: object disconnected device={}:{}",
                        id.slot(),
                        id.generation()
                    );
                    break;
                }
                Ok(Read::Empty) => {
                    // Drained: the device is no longer ready, which is the auto-reset half of
                    // the waitable's contract.
                    executive::reset(id);
                    break;
                }
                Err(error) => panic!("input console stale subscription: {error:?}"),
            }
        }
    }
}
#[cfg(feature = "hal-input-object-test")]
mod tests;
/// Run hardware-observer checks only in the dedicated input-object test image.
#[cfg(feature = "hal-input-object-test")]
pub fn hardware_selftest(console: &mut dyn HalConsole) {
    tests::step(console);
}

/// Run the Executive binding's boot checks. Called once from the kernel idle loop.
pub fn executive_selftest(console: &mut dyn HalConsole) {
    executive::selftest(console);
}
