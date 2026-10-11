//! The object manager at boot: the header pool, the root namespace, and the self test.
//!
//! This is where the host-tested rules in `nolx-obj-core` meet real object headers. Each
//! object the executive creates gets one [`ObjectHeader`] from a fixed pool; the namespace binds
//! names to object ids; handle tables hold ids and rights. The reference rules are the ones
//! `obj/mod.rs` documents, applied in one place:
//!
//! * A new object starts with one pointer reference, the creator's. Binding it into the
//!   namespace hands that reference to the namespace.
//! * Opening a handle adds a pointer reference and a handle reference together, so
//!   `handle_count <= pointer_count` holds.
//! * Closing a handle drops both. Removing a binding drops the namespace's pointer reference.
//! * Whichever release takes the count to zero deletes the object and frees its header slot.
//!
//! Every access to the namespace goes through [`with_namespace`], so the boot-time single
//! processor assumption is stated once and the rules above cannot be bypassed by a caller
//! that reaches for the namespace directly.

use core::sync::atomic::{AtomicBool, Ordering};

use nolx_hal_core::console::HalConsole;
use nolx_obj_core::handle::{Handle, HandleError, HandleTable};
use nolx_obj_core::namespace::{NsError, Namespace};

use super::{type_by_name, ObjectHeader};
use crate::sync::SyncCell;

/// Objects the boot-time pool holds at once.
pub const MAX_OBJECTS: usize = 64;

/// Nodes the root namespace can hold, including the root.
const NAMESPACE_NODES: usize = 32;

/// Entries in a handle table. Small on purpose: a process's table is quota-charged, and the
/// self test only needs a few.
pub const HANDLE_TABLE_ENTRIES: usize = 16;

/// What the namespace binds names to: an index into the header pool.
pub type ObjectId = u16;

/// The header pool. A slot is in use while its `LIVE` flag is set.
static HEADERS: [ObjectHeader; MAX_OBJECTS] = [const { ObjectHeader::new(0, 0) }; MAX_OBJECTS];
static LIVE: [AtomicBool; MAX_OBJECTS] = [const { AtomicBool::new(false) }; MAX_OBJECTS];

/// The root namespace. `None` until [`init`] builds it.
static NAMESPACE: SyncCell<Option<Namespace<ObjectId, NAMESPACE_NODES>>> = SyncCell::new(None);

/// Why an object-manager operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjError {
    /// [`init`] has not run.
    NotInitialised,
    /// No object type of that name is registered.
    UnknownType,
    /// Every header slot is in use.
    NoObjectSlot,
    /// A namespace operation failed.
    Namespace(NsError),
    /// A handle operation failed.
    Handle(HandleError),
    /// A self-test check did not hold; the string says which.
    Check(&'static str),
}

/// Run `f` with the root namespace.
///
/// The namespace is reached only from the boot processor, with no interrupt handler that
/// touches it, and `f` must not call back into this function: the `&mut` below would then
/// alias. Every caller in this module passes a closure that makes one namespace call.
fn with_namespace<R>(
    f: impl FnOnce(&mut Namespace<ObjectId, NAMESPACE_NODES>) -> R,
) -> Result<R, ObjError> {
    // SAFETY: single boot processor, no interrupt handler reaches the namespace, and `f`
    // cannot re-enter this function (see above), so this is the only live reference.
    let namespace = unsafe { &mut *NAMESPACE.get() };
    let namespace = namespace.as_mut().ok_or(ObjError::NotInitialised)?;
    Ok(f(namespace))
}

/// The header for a live object.
fn header_of(id: ObjectId) -> Result<&'static ObjectHeader, ObjError> {
    let index = usize::from(id);
    if index < MAX_OBJECTS && LIVE[index].load(Ordering::Acquire) {
        Ok(&HEADERS[index])
    } else {
        Err(ObjError::Check("reference to an object that is not live"))
    }
}

/// Free a header slot. The object body, if any, has already been torn down by the caller.
fn free_slot(id: ObjectId) {
    LIVE[usize::from(id)].store(false, Ordering::Release);
}

/// Drop one pointer reference, deleting the object if it was the last.
fn release_pointer(id: ObjectId) -> Result<(), ObjError> {
    if header_of(id)?.release() {
        free_slot(id);
    }
    Ok(())
}

/// Number of live objects in the pool.
#[must_use]
pub fn live_objects() -> usize {
    LIVE.iter()
        .filter(|live| live.load(Ordering::Acquire))
        .count()
}

/// The current `(pointer_count, handle_count)` of an object.
pub fn reference_counts(id: ObjectId) -> Result<(i32, i32), ObjError> {
    let header = header_of(id)?;
    Ok((
        header.pointer_count.load(Ordering::Acquire),
        header.handle_count.load(Ordering::Acquire),
    ))
}

/// Build the root namespace with the executive's standard directories.
pub fn init(console: &mut dyn HalConsole) -> Result<(), ObjError> {
    // SAFETY: called once, from the boot processor, before anything reads the namespace, so
    // there is no other reference to the cell.
    unsafe { *NAMESPACE.get() = Some(Namespace::new()) };
    for directory in ["Device", "FileSystem", "BaseNamedObjects", "NOLX"] {
        with_namespace(|ns| ns.create_directory("\\", directory))?.map_err(ObjError::Namespace)?;
    }
    let _ = writeln!(
        console,
        "obj: namespace: \\Device \\FileSystem \\BaseNamedObjects \\NOLX"
    );
    Ok(())
}

/// Create an object of type `type_name` and bind it to `parent\name`.
///
/// The creator's pointer reference is handed to the namespace, so the object lives until the
/// binding is removed and every handle to it is closed.
pub fn create_object(parent: &str, name: &str, type_name: &str) -> Result<ObjectId, ObjError> {
    let (type_index, _) = type_by_name(type_name).ok_or(ObjError::UnknownType)?;
    let slot = LIVE
        .iter()
        .position(|live| {
            live.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
        .ok_or(ObjError::NoObjectSlot)?;
    let id = ObjectId::try_from(slot).map_err(|_| ObjError::NoObjectSlot)?;
    let header = &HEADERS[slot];
    header.type_index.store(type_index, Ordering::Relaxed);
    header.handle_count.store(0, Ordering::Relaxed);
    header.pointer_count.store(1, Ordering::Relaxed);

    match with_namespace(|ns| ns.insert_object(parent, name, id)) {
        Ok(Ok(())) => Ok(id),
        Ok(Err(error)) => {
            free_slot(id);
            Err(ObjError::Namespace(error))
        }
        Err(error) => {
            free_slot(id);
            Err(error)
        }
    }
}

/// Open a handle to the object at `path` in `table`, with `access`.
pub fn open(
    path: &str,
    table: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    access: u32,
) -> Result<Handle, ObjError> {
    let id = with_namespace(|ns| ns.lookup(path).copied())?.map_err(ObjError::Namespace)?;
    let header = header_of(id)?;
    header.add_handle();
    match table.insert(id, access, false) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            header.drop_handle();
            release_pointer(id)?;
            Err(ObjError::Handle(error))
        }
    }
}

/// Duplicate `source_handle` from `source` into `target`, per `DuplicateHandle` rules.
pub fn duplicate(
    source: &HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    source_handle: Handle,
    target: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    access: u32,
) -> Result<Handle, ObjError> {
    let id = source
        .get(source_handle)
        .map_err(ObjError::Handle)?
        .object;
    let header = header_of(id)?;
    header.add_handle();
    match source.duplicate_into(source_handle, target, id, access, false) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            header.drop_handle();
            release_pointer(id)?;
            Err(ObjError::Handle(error))
        }
    }
}

/// Close `handle` in `table`, dropping the handle reference and its pointer reference.
pub fn close(
    table: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    handle: Handle,
) -> Result<(), ObjError> {
    let entry = table.close(handle).map_err(ObjError::Handle)?;
    header_of(entry.object)?.drop_handle();
    release_pointer(entry.object)
}

/// Remove the binding at `path`, dropping the namespace's pointer reference.
///
/// Returns `true` if a binding was removed (directories and links are removed too, and
/// carry no reference).
pub fn unbind(path: &str) -> Result<bool, ObjError> {
    match with_namespace(|ns| ns.remove(path))?.map_err(ObjError::Namespace)? {
        Some(id) => {
            release_pointer(id)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Look up the id bound at `path`, following links.
pub fn lookup(path: &str) -> Result<ObjectId, ObjError> {
    with_namespace(|ns| ns.lookup(path).copied())?.map_err(ObjError::Namespace)
}

/// Create a symbolic link `parent\name` to `target`.
pub fn link(parent: &str, name: &str, target: &str) -> Result<(), ObjError> {
    with_namespace(|ns| ns.create_symlink(parent, name, target))?.map_err(ObjError::Namespace)
}

fn check(condition: bool, message: &'static str) -> Result<(), ObjError> {
    if condition {
        Ok(())
    } else {
        Err(ObjError::Check(message))
    }
}

/// The object manager's boot self test.
///
/// It drives one object through every reference transition the rules allow and checks the
/// counts after each step: created, opened, duplicated into a second table, a duplicate that
/// widens rights refused, closed in the first table, its name removed while the second table
/// still holds it, and finally closed. The object must be freed exactly when the last
/// reference goes, and the live count must return to where it started.
pub fn selftest(console: &mut dyn HalConsole) -> Result<(), ObjError> {
    const READ: u32 = 0x1;
    const READ_WRITE: u32 = 0x3;

    let live_before = live_objects();
    let mut first: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();
    let mut second: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();

    let id = create_object("\\Device", "Null", "Device")?;
    link("\\", "NullLink", "\\Device\\Null")?;
    check(
        lookup("\\NullLink")? == id,
        "a symbolic link does not resolve to its object",
    )?;
    check(
        reference_counts(id)? == (1, 0),
        "a new object should hold only the namespace's reference",
    )?;

    let a = open("\\Device\\Null", &mut first, READ)?;
    check(reference_counts(id)? == (2, 1), "open did not add a reference pair")?;

    // Widening rights must be refused, and a refused duplicate must leave no trace.
    let refused = duplicate(&first, a, &mut second, READ_WRITE);
    check(
        refused == Err(ObjError::Handle(HandleError::AccessDenied)),
        "a duplicate that widened access was not refused",
    )?;
    check(
        reference_counts(id)? == (2, 1),
        "a refused duplicate changed the reference counts",
    )?;

    let b = duplicate(&first, a, &mut second, nolx_obj_core::handle::ACCESS_ALL)?;
    check(
        reference_counts(id)? == (3, 2),
        "a duplicate did not add a reference pair",
    )?;
    check(
        second.get(b).map(|entry| entry.access) == Ok(READ),
        "a same-access duplicate did not keep the source's rights",
    )?;

    close(&mut first, a)?;
    check(reference_counts(id)? == (2, 1), "closing a handle did not drop its pair")?;
    check(
        first.get(a) == Err(HandleError::Invalid),
        "a closed handle still resolves in its table",
    )?;

    // Remove the name while the second table still holds the object: the object must
    // survive, the name must be gone, and the link to it must dangle.
    check(unbind("\\Device\\Null")?, "the binding was not found to remove")?;
    check(
        reference_counts(id)? == (1, 1),
        "removing the binding did not drop the namespace's reference",
    )?;
    check(
        lookup("\\Device\\Null") == Err(ObjError::Namespace(NsError::NotFound)),
        "a removed binding still resolves",
    )?;
    check(
        lookup("\\NullLink") == Err(ObjError::Namespace(NsError::NotFound)),
        "a link to a removed binding still resolves",
    )?;
    check(!unbind("\\NullLink")?, "a link was reported as an object")?;

    close(&mut second, b)?;
    check(
        header_of(id).is_err(),
        "the object was not freed when its last reference went",
    )?;
    check(
        live_objects() == live_before,
        "live object count did not return to its starting value",
    )?;

    let _ = writeln!(
        console,
        "NOLX: obj selftest: 1 object, 2 handles (1 duplicated, 1 widening refused), \
         1 symlink lookup, refcount (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, \
         live objects {live_before} -> {live_before}; audit ok"
    );
    Ok(())
}
