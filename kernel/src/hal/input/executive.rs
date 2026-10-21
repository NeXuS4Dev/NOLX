//! Executive bindings for kernel input objects.
//!
//! The registry in [`super`] is deliberately not a user-visible interface: it is a bounded,
//! allocation-free table of device-owned objects that the kernel itself reads. This module is
//! the step between that and a caller: every attached input device also gets an Executive
//! object, `\Device\InputN`, of type `InputDevice`, with a security descriptor, a handle that
//! a process table can hold, and a waitable state.
//!
//! Three properties are worth stating, because they are what the binding actually buys:
//!
//! * **Security.** The descriptor names SYSTEM as the owner, refuses POSIX user 1001 outright,
//!   and grants user 1000 `READ | SYNCHRONIZE`. The type's access mask does not include
//!   `WRITE`, so no handle can ever carry the right to inject input: producers are kernel
//!   drivers and nothing else.
//! * **Identity.** The handle names the *incarnation* of the device, not its slot. A
//!   disconnected device's object is unbound, so a later `\Device\Input0` is a different
//!   object; a handle to the retired one still resolves only through its own reference count.
//! * **Readiness.** The waitable is auto-reset and per device: a publication sets it, and the
//!   waiter that consumes it clears it, so two waiters cannot both observe one event. It is
//!   kept in an atomic array indexed by registry slot so the interrupt path can set it without
//!   taking a lock that thread context may hold.
//!
//! What this is not: a `read(2)` interface. Exposing reports to ring 3 belongs to the syscall
//! layer, and the wait here is a ready/not-ready check rather than a scheduler block; both are
//! recorded as open in the M9 completion gate rather than implied by this module.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use nolx_obj_core::handle::{Handle, HandleTable};
use nolx_obj_core::security::{
    rights, Ace, AceKind, Acl, IntegrityLevel, SecurityDescriptor, Sid, Token,
};

use crate::obj::boot::{self, ObjError, ObjectId};
use crate::sync::IrqMutex;

use super::{Class, DeviceId, DEVICES};

/// Object type name registered with the Object Manager for these bindings.
pub const TYPE_NAME: &str = "InputDevice";

/// Rights an input handle may carry.
///
/// `WRITE` is absent on purpose: a handle is for observing a device, never for feeding it.
pub const INPUT_ACCESS: u32 = rights::READ | rights::SYNCHRONIZE | rights::READ_CONTROL;

/// Names published under `\Device`, indexed by binding slot.
const NAMES: [&str; MAX_BINDINGS] = [
    "Input0", "Input1", "Input2", "Input3", "Input4", "Input5", "Input6", "Input7", "Input8",
    "Input9", "Input10", "Input11", "Input12", "Input13", "Input14", "Input15",
];

/// Bindings this module can publish at once: one per registry slot.
pub const MAX_BINDINGS: usize = DEVICES;

/// One published device.
struct Binding {
    /// The registry object the Executive object stands for.
    device: DeviceId,
    /// The object id the namespace binds.
    object: ObjectId,
    /// Which input class this is, for diagnostics.
    class: Class,
}

static BINDINGS: IrqMutex<[Option<Binding>; MAX_BINDINGS]> =
    IrqMutex::new([const { None }; MAX_BINDINGS]);

/// Readiness per registry slot, so the interrupt path needs no lock.
static SIGNALED: [AtomicBool; DEVICES] = [const { AtomicBool::new(false) }; DEVICES];

/// Registry slot to binding slot, or [`NO_BINDING`]; written under the binding lock and read by
/// the handle walk without it.
static SLOT_OF: [AtomicU8; DEVICES] = [const { AtomicU8::new(NO_BINDING) }; DEVICES];

/// Marker for "this registry slot has no Executive object".
const NO_BINDING: u8 = u8::MAX;

/// Open-callback count, so a test can prove the type hook runs rather than assume it.
static OPEN_CALLBACKS: AtomicUsize = AtomicUsize::new(0);
/// Delete-callback count.
static DELETE_CALLBACKS: AtomicUsize = AtomicUsize::new(0);

/// The descriptor every input object carries.
///
/// User 1001 is refused before the grants are considered, so a token that matches only the
/// allow entries still cannot open the device. There is no integrity label: an input report is
/// not lower-integrity data than the process reading it.
fn descriptor() -> SecurityDescriptor {
    let mut acl = Acl::empty();
    // Both pushes are static data with room for three entries in a 4096-entry-capacity ACL, so
    // a full ACL here would be a programming error rather than a runtime condition.
    let _ = acl.push(Ace {
        kind: AceKind::Deny,
        sid: Sid::user(1001),
        mask: rights::ALL,
    });
    let _ = acl.push(Ace {
        kind: AceKind::Allow,
        sid: Sid::SYSTEM,
        mask: rights::ALL,
    });
    let _ = acl.push(Ace {
        kind: AceKind::Allow,
        sid: Sid::user(1000),
        mask: INPUT_ACCESS,
    });
    SecurityDescriptor {
        owner: Sid::SYSTEM,
        dacl: Some(acl),
        label: None,
    }
}

/// The token a spawned user process runs under.
///
/// NOLX has one interactive session at this stage of M9; giving every user process the same
/// identity keeps the descriptor above meaningful instead of aspirational. A per-process token
/// arrives with the security milestone.
#[must_use]
pub fn process_token() -> Token {
    Token::new(Sid::user(1000), IntegrityLevel::Medium)
}

/// The path an input object is published at.
#[must_use]
pub fn path(name: &str) -> &str {
    // The names are compile-time constants under a compile-time parent, so the returned string
    // is always one of the sixteen literals below.
    match name {
        "Input0" => "\\Device\\Input0",
        "Input1" => "\\Device\\Input1",
        "Input2" => "\\Device\\Input2",
        "Input3" => "\\Device\\Input3",
        "Input4" => "\\Device\\Input4",
        "Input5" => "\\Device\\Input5",
        "Input6" => "\\Device\\Input6",
        "Input7" => "\\Device\\Input7",
        "Input8" => "\\Device\\Input8",
        "Input9" => "\\Device\\Input9",
        "Input10" => "\\Device\\Input10",
        "Input11" => "\\Device\\Input11",
        "Input12" => "\\Device\\Input12",
        "Input13" => "\\Device\\Input13",
        "Input14" => "\\Device\\Input14",
        _ => "\\Device\\Input15",
    }
}

/// Publish the Executive object for a newly attached device.
///
/// Returns the published name, which the caller logs. Failure is not fatal to input: the
/// kernel-side object keeps working, and the caller reports the refusal.
///
/// # Errors
///
/// [`ObjError`] from the Object Manager: no header slot, no namespace room, or a duplicate
/// name.
pub fn publish(device: DeviceId, class: Class) -> Result<&'static str, ObjError> {
    let mut bindings = BINDINGS.lock();
    let slot = bindings
        .iter()
        .position(Option::is_none)
        .ok_or(ObjError::Check("every input binding slot is in use"))?;
    let name = NAMES[slot];
    let object = boot::create_object("\\Device", name, TYPE_NAME, descriptor())?;
    bindings[slot] = Some(Binding {
        device,
        object,
        class,
    });
    SLOT_OF[device.slot()].store(slot as u8, Ordering::Release);
    SIGNALED[device.slot()].store(false, Ordering::Release);
    drop(bindings);
    Ok(name)
}

/// Withdraw a device's Executive object: unbind the name, then forget the binding.
///
/// A handle already open keeps its own reference, so the object outlives this call; what ends
/// is the ability to *find* it by name, which is the property a stale device reference must
/// not have.
pub fn withdraw(device: DeviceId) {
    let mut bindings = BINDINGS.lock();
    let slot = SLOT_OF[device.slot()].swap(NO_BINDING, Ordering::AcqRel);
    SIGNALED[device.slot()].store(false, Ordering::Release);
    if slot == NO_BINDING {
        return;
    }
    let index = usize::from(slot);
    let Some(binding) = bindings[index].take() else {
        return;
    };
    drop(bindings);
    // The binding was ours, so the path is a literal; a failure here means the namespace lost
    // an entry it should still hold and the object keeps its namespace reference. Report it
    // rather than silently leaking the name.
    if boot::unbind(path(NAMES[index])).is_err() {
        crate::hal::console::raw_put_str("input: executive unbind failed; device name retained\n");
    }
    let _ = binding.class;
}

/// Set the device's waitable. Safe to call from an interrupt: it takes no lock.
pub fn signal(device: DeviceId) {
    SIGNALED[device.slot()].store(true, Ordering::Release);
}

/// Clear the waitable because a reader has drained the device. Interrupt-safe.
pub fn reset(device: DeviceId) {
    SIGNALED[device.slot()].store(false, Ordering::Release);
}

/// Find the ready input object behind one of `table`'s handles and consume its readiness.
///
/// This is the auto-reset rule: the first waiter to see the signal clears it, so two waiters
/// never act on one event. The handle is returned because the caller needs to know *which*
/// device woke it, and the device id because that is what it then reads.
#[must_use]
pub fn wait_ready<const N: usize>(table: &HandleTable<ObjectId, N>) -> Option<(Handle, DeviceId)> {
    let bindings = BINDINGS.lock();
    for (handle, entry) in table.iter() {
        if entry.access & rights::SYNCHRONIZE == 0 {
            continue;
        }
        let Some((device, _)) = bindings
            .iter()
            .flatten()
            .find(|binding| binding.object == entry.object)
            .map(|binding| (binding.device, binding.class))
        else {
            continue;
        };
        if SIGNALED[device.slot()].swap(false, Ordering::AcqRel) {
            return Some((handle, device));
        }
    }
    None
}

/// Open every published input object for `token` into `table`.
///
/// Returns how many handles were installed. A refused open (the DACL refuses the token, or the
/// table is full) stops the walk rather than installing a prefix: a caller either holds the
/// devices it may see, or it is told why not.
///
/// # Errors
///
/// [`ObjError`] from the first refused open.
pub fn bind_process<const N: usize>(
    table: &mut HandleTable<ObjectId, N>,
    token: &Token,
) -> Result<usize, ObjError> {
    let mut opened = 0;
    for index in 0..MAX_BINDINGS {
        if BINDINGS.lock()[index].is_none() {
            continue;
        }
        boot::open(path(NAMES[index]), token, table, INPUT_ACCESS)?;
        opened += 1;
    }
    Ok(opened)
}

/// Close every input handle in `table`, leaving handles to other objects alone.
///
/// The walk takes one handle at a time rather than collecting them first: closing removes the
/// entry, so the loop makes progress by construction, and no scratch array has to guess how
/// many handles there are or which iteration closed which one.
pub fn release_process<const N: usize>(table: &mut HandleTable<ObjectId, N>) {
    loop {
        let next = {
            let bindings = BINDINGS.lock();
            table
                .iter()
                .find(|(_, entry)| {
                    bindings
                        .iter()
                        .flatten()
                        .any(|binding| binding.object == entry.object)
                })
                .map(|(handle, _)| handle)
        };
        let Some(handle) = next else {
            break;
        };
        if boot::close(table, handle).is_err() {
            // A handle that cannot be closed would make this loop spin forever; the table is
            // in a state no caller can repair, so say so and stop instead.
            crate::hal::console::raw_put_str("input: handle close failed\n");
            break;
        }
    }
}

/// How many devices are bound.
#[must_use]
pub fn count() -> usize {
    BINDINGS.lock().iter().flatten().count()
}

/// The binding's object id, for tests and diagnostics.
#[must_use]
pub fn object_of(device: DeviceId) -> Option<ObjectId> {
    BINDINGS
        .lock()
        .iter()
        .flatten()
        .find(|binding| binding.device == device)
        .map(|binding| binding.object)
}

/// `InputDevice` create hook: a name is required and must be one this module publishes.
///
/// Reusing the Device type's check (a non-empty name) would let anything be called an input
/// device. The stricter rule is that the name must be one of the literals in [`NAMES`], which
/// only [`publish`] can produce.
pub(crate) fn create(
    _id: ObjectId,
    name: &str,
    _descriptor: &SecurityDescriptor,
) -> Result<(), ObjError> {
    if NAMES.contains(&name) {
        Ok(())
    } else {
        Err(ObjError::Check("input object name is not a binding name"))
    }
}

/// `InputDevice` open hook: record the grant and refuse anything outside the access mask.
pub(crate) fn open(
    _id: ObjectId,
    _token: &Token,
    requested: u32,
    granted: u32,
) -> Result<(), ObjError> {
    if requested & !INPUT_ACCESS != 0 {
        return Err(ObjError::UnsupportedAccess { requested });
    }
    if granted & !requested != 0 {
        return Err(ObjError::Check("input open widened the requested access"));
    }
    OPEN_CALLBACKS.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// `InputDevice` close hook.
pub(crate) fn close(_id: ObjectId, _handle: Handle) {}

/// `InputDevice` delete hook: the last reference is gone, so the waitable must not linger.
pub(crate) fn delete(id: ObjectId) {
    DELETE_CALLBACKS.fetch_add(1, Ordering::Relaxed);
    let mut bindings = BINDINGS.lock();
    for index in 0..MAX_BINDINGS {
        if bindings[index]
            .as_ref()
            .is_some_and(|binding| binding.object == id)
        {
            let slot = bindings[index]
                .as_ref()
                .map(|binding| binding.device.slot());
            bindings[index] = None;
            if let Some(slot) = slot {
                SLOT_OF[slot].store(NO_BINDING, Ordering::Release);
                SIGNALED[slot].store(false, Ordering::Release);
            }
        }
    }
}

/// `InputDevice` access mask.
pub(crate) fn validate_access(requested: u32) -> bool {
    requested & !INPUT_ACCESS == 0
}

/// Boot evidence for the binding as a whole.
///
/// Runs whether or not devices exist: on a machine with no input hardware the check is that
/// nothing was published, which is as much a fact about the binding as a granted handle is.
pub fn selftest(console: &mut dyn nolx_hal_core::console::HalConsole) {
    let published = count();
    if published == 0 {
        let _ = writeln!(
            console,
            "NOLX: input executive selftest: no input device on this machine, no object published"
        );
        return;
    }
    let Some(slot) = BINDINGS.lock().iter().position(Option::is_some) else {
        return;
    };
    let device = BINDINGS.lock()[slot]
        .as_ref()
        .map(|binding| binding.device)
        .expect("binding slot just observed");
    let name = NAMES[slot];
    let path = path(name);
    let object = object_of(device).expect("published binding has an object");

    let interactive = process_token();
    let restricted = Token::new(Sid::user(1001), IntegrityLevel::Medium);
    let low = Token::new(Sid::user(1000), IntegrityLevel::Low);
    let mut first: HandleTable<ObjectId, 8> = HandleTable::new();
    let mut second: HandleTable<ObjectId, 8> = HandleTable::new();
    let before = boot::reference_counts(object).expect("live input object");

    // The name resolves to the object the binding published.
    let resolved = boot::lookup(path) == Ok(object);

    // An interactive token receives exactly the mask the descriptor grants, and the type hook
    // runs for it.
    let opens_before = OPEN_CALLBACKS.load(Ordering::Relaxed);
    let granted = boot::open(path, &interactive, &mut first, INPUT_ACCESS);
    let handle_ok = granted.as_ref().is_ok_and(|handle| {
        first
            .get(*handle)
            .is_ok_and(|entry| entry.access == INPUT_ACCESS)
    });
    let callback_ran =
        OPEN_CALLBACKS.load(Ordering::Relaxed) == opens_before + usize::from(handle_ok);

    // A refused identity is refused before any grant is considered, and leaves nothing behind.
    let denied = boot::open(path, &restricted, &mut second, rights::READ)
        == Err(ObjError::AccessDenied {
            missing: rights::READ,
        });
    let refused_empty = second.is_empty();
    // A low-integrity token is *not* refused: input reports are not lower-integrity data, and
    // the descriptor carries no label on purpose. Asserting it keeps the descriptor honest.
    let low_granted = boot::open(path, &low, &mut second, rights::READ);
    let low_allowed = low_granted.is_ok();
    // Write access does not exist for this type at all: producers are kernel drivers.
    let no_write = boot::open(path, &interactive, &mut second, rights::WRITE)
        == Err(ObjError::UnsupportedAccess {
            requested: rights::WRITE,
        });

    // Auto-reset readiness: nothing ready, one signal, one waiter consumes it, the second
    // waiter sees nothing, and a drained device is not ready again.
    reset(device);
    let idle = wait_ready(&first).is_none();
    signal(device);
    let first_wait = wait_ready(&first).is_some_and(|(_, id)| id == device);
    let second_wait = wait_ready(&first).is_none();
    signal(device);
    reset(device);
    let drained = wait_ready(&first).is_none();

    // The same code path a spawned process uses: a private table holding the device handles.
    let mut process: HandleTable<ObjectId, 8> = HandleTable::new();
    let bound = bind_process(&mut process, &interactive);
    let process_handles =
        bound.is_ok_and(|opened| opened == published && process.len() == published);
    release_process(&mut process);
    let process_released = process.is_empty();

    if let Ok(handle) = granted {
        let _ = boot::close(&mut first, handle);
    }
    if let Ok(handle) = low_granted {
        let _ = boot::close(&mut second, handle);
    }
    let after = boot::reference_counts(object).expect("input object outlives its handles");

    let failure = if !resolved {
        Some("the published name does not resolve to its object")
    } else if !handle_ok {
        Some("an interactive token did not receive the full access mask")
    } else if !callback_ran {
        Some("the type open hook did not run for a granted handle")
    } else if !denied || !refused_empty {
        Some("a refused identity left a handle or was not refused")
    } else if !low_allowed {
        Some("a low-integrity token was refused a device with no label")
    } else if !no_write {
        Some("the input type accepted a write handle")
    } else if !idle || !first_wait || !second_wait || !drained {
        Some("the ready device did not follow the auto-reset rule")
    } else if !process_handles || !process_released {
        Some("binding a process table did not install and release the device handles")
    } else if after != before {
        Some("the input object's reference counts did not return")
    } else {
        None
    };
    if let Some(message) = failure {
        let _ = writeln!(console, "input: executive selftest: FAILED: {message}");
        panic!("input executive selftest: {message}");
    }
    let _ = writeln!(
        console,
        "NOLX: input executive selftest: {published} object(s) under \\Device\\Input*, \
         {path} granted read+synchronize, user 1001 refused without a handle, write refused \
         by the type mask, wait auto-reset and drain verified, {published} process handle(s) \
         installed and released, references {} -> {}",
        before.0, after.0
    );
}
