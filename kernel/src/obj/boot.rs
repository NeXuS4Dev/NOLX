//! The object manager at boot: the header pool, the root namespace, the security monitor's
//! access checks, and the self test.
//!
//! This is where the host-tested rules in `nolx-obj-core` meet real object headers. Each
//! object the executive creates gets one [`ObjectHeader`] from a fixed pool, a security
//! descriptor, and a name in the namespace; handle tables hold object ids and granted rights.
//! The reference rules are the ones `obj/mod.rs` documents, applied in one place:
//!
//! * A new object starts with one pointer reference, the creator's. Binding it into the
//!   namespace hands that reference to the namespace.
//! * Opening a handle runs `SeAccessCheck` first, then adds a pointer reference and a handle
//!   reference together, so `handle_count <= pointer_count` holds. The handle carries the
//!   rights granted, never more than were asked for.
//! * Closing a handle drops both. Removing a binding drops the namespace's pointer reference.
//! * Whichever release takes the count to zero deletes the object and frees its header slot.
//!
//! Every access to the namespace, the descriptor table, and the audit log goes through a
//! `with_*` helper, so the boot-time single-processor assumption is stated once and the rules
//! above cannot be bypassed by a caller that reaches for the state directly.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use nolx_hal_core::console::HalConsole;
use nolx_obj_core::handle::{Handle, HandleError, HandleTable, ACCESS_ALL};
use nolx_obj_core::namespace::{Namespace, NsError, PrivateNamespace};
use nolx_obj_core::security::{
    access_check, privilege, rights, Ace, AceKind, Acl, AuditLog, AuditRecord, Decision,
    IntegrityLevel, SecurityDescriptor, Sid, Token,
};

use super::{type_by_index, type_by_name, ObjectHeader, ObjectType};
use crate::sync::SyncCell;

/// Objects the boot-time pool holds at once.
pub const MAX_OBJECTS: usize = 64;

/// Nodes the root namespace can hold, including the root.
const NAMESPACE_NODES: usize = 32;
/// The number of simultaneously live private namespaces.
pub const MAX_PRIVATE_NAMESPACES: usize = 4;
/// Nodes each private namespace can hold, including its root.
const PRIVATE_NAMESPACE_NODES: usize = 16;

/// Entries in a handle table. Small on purpose: a process's table is quota-charged, and the
/// self test only needs a few.
pub const HANDLE_TABLE_ENTRIES: usize = 16;

/// Access decisions the audit log keeps in full. Totals are kept exactly beyond this.
const AUDIT_RECORDS: usize = 16;
const DEVICE_CALLBACK_COUNT: usize = 5;

/// What the namespace binds names to: an index into the header pool.
pub type ObjectId = u16;

/// The header pool. A slot is in use while its `LIVE` flag is set.
static HEADERS: [ObjectHeader; MAX_OBJECTS] = [const { ObjectHeader::new(0, 0) }; MAX_OBJECTS];
static LIVE: [AtomicBool; MAX_OBJECTS] = [const { AtomicBool::new(false) }; MAX_OBJECTS];
static DEVICE_CALLBACKS: [AtomicUsize; DEVICE_CALLBACK_COUNT] =
    [const { AtomicUsize::new(0) }; DEVICE_CALLBACK_COUNT];

/// The root namespace. Built empty at compile time and given its root by [`init`], so it lives
/// in `.data` rather than being assembled on the boot stack.
static NAMESPACE: SyncCell<Namespace<ObjectId, NAMESPACE_NODES>> =
    SyncCell::new(Namespace::empty());

struct PrivateNamespaceSlot {
    generation: u16,
    namespace: Option<PrivateNamespace<ObjectId, PRIVATE_NAMESPACE_NODES>>,
}

static PRIVATE_NAMESPACES: SyncCell<[PrivateNamespaceSlot; MAX_PRIVATE_NAMESPACES]> = SyncCell::new(
    [const {
        PrivateNamespaceSlot {
            generation: 0,
            namespace: None,
        }
    }; MAX_PRIVATE_NAMESPACES],
);

/// The security descriptor of each live object, by slot.
static DESCRIPTORS: SyncCell<[Option<SecurityDescriptor>; MAX_OBJECTS]> =
    SyncCell::new([None; MAX_OBJECTS]);

/// SRM checks and private namespace boundary decisions made by the boot object manager.
static AUDIT: SyncCell<AuditLog<AUDIT_RECORDS>> = SyncCell::new(AuditLog::empty());

/// A handle to one live private namespace.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PrivateNamespaceId {
    slot: u8,
    generation: u16,
}

impl PrivateNamespaceId {
    /// A stable diagnostic value combining the slot and generation.
    #[must_use]
    pub fn raw(self) -> u32 {
        (u32::from(self.generation) << 8) | u32::from(self.slot)
    }
}

/// Why an object-manager operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjError {
    /// No object type of that name is registered.
    UnknownType,
    /// Every header slot is in use.
    NoObjectSlot,
    /// No private namespace slot is available.
    NoPrivateNamespaceSlot,
    /// The private namespace ID is stale or invalid.
    PrivateNamespaceNotFound,
    /// A namespace operation failed.
    Namespace(NsError),
    /// A handle operation failed.
    Handle(HandleError),
    /// The object type does not support one or more requested access rights.
    UnsupportedAccess {
        /// The unsupported access mask.
        requested: u32,
    },
    /// The security monitor refused the rights asked for.
    AccessDenied {
        /// The rights that were asked for and refused.
        missing: u32,
    },
    /// A self-test check did not hold, or an object was used after it was freed. The string
    /// says which.
    Check(&'static str),
}

const DEVICE_CREATE: usize = 0;
const DEVICE_OPEN: usize = 1;
const DEVICE_PARSE: usize = 2;
const DEVICE_CLOSE: usize = 3;
const DEVICE_DELETE: usize = 4;

fn record_device_callback(index: usize) {
    DEVICE_CALLBACKS[index].fetch_add(1, Ordering::Relaxed);
}

/// Device type create callback. Device-specific allocation will be attached here when the
/// device manager lands; the current fixed-pool object validates and records the lifecycle hook.
pub(super) fn device_create(
    _id: ObjectId,
    name: &str,
    _descriptor: &SecurityDescriptor,
) -> Result<(), ObjError> {
    if name.is_empty() {
        return Err(ObjError::Check("device object name is empty"));
    }
    record_device_callback(DEVICE_CREATE);
    Ok(())
}

/// Device type open callback. It rejects execution and any accidental rights widening after SRM.
pub(super) fn device_open(
    _id: ObjectId,
    _token: &Token,
    requested: u32,
    granted: u32,
) -> Result<(), ObjError> {
    if requested & rights::EXECUTE != 0 {
        return Err(ObjError::UnsupportedAccess { requested });
    }
    if granted & !requested != 0 {
        return Err(ObjError::Check("device open widened the requested access"));
    }
    record_device_callback(DEVICE_OPEN);
    Ok(())
}

/// Device type parse callback. Generic traversal resolves the path; this hook enforces that
/// the resolved device name remains absolute.
pub(super) fn device_parse(_id: ObjectId, path: &str) -> Result<(), ObjError> {
    if !path.starts_with('\\') {
        return Err(ObjError::Namespace(NsError::InvalidPath));
    }
    record_device_callback(DEVICE_PARSE);
    Ok(())
}

/// Device type close callback.
pub(super) fn device_close(_id: ObjectId, _handle: Handle) {
    record_device_callback(DEVICE_CLOSE);
}

/// Device type delete callback.
pub(super) fn device_delete(_id: ObjectId) {
    record_device_callback(DEVICE_DELETE);
}

/// Validate that every requested Device right belongs to the SRM access mask.
pub(super) fn device_validate_access(requested: u32) -> bool {
    requested & !rights::ALL == 0
}

fn device_callback_counts() -> [usize; DEVICE_CALLBACK_COUNT] {
    core::array::from_fn(|index| DEVICE_CALLBACKS[index].load(Ordering::Relaxed))
}

/// Run `f` with the root namespace.
///
/// The namespace is reached only from the boot processor, with no interrupt handler that
/// touches it, and `f` must not call back into this function: the `&mut` below would then
/// alias. Every caller in this module passes a closure that makes one namespace call.
fn with_namespace<R>(f: impl FnOnce(&mut Namespace<ObjectId, NAMESPACE_NODES>) -> R) -> R {
    // SAFETY: single boot processor, no interrupt handler reaches the namespace, and `f`
    // cannot re-enter this function (see above), so this is the only live reference.
    let namespace = unsafe { &mut *NAMESPACE.get() };
    f(namespace)
}

fn with_private_namespaces<R>(
    f: impl FnOnce(&mut [PrivateNamespaceSlot; MAX_PRIVATE_NAMESPACES]) -> R,
) -> R {
    // SAFETY: as with_namespace, only the boot processor reaches this table, no interrupt
    // handler touches it, and callers finish the closure before re-entering this helper.
    let namespaces = unsafe { &mut *PRIVATE_NAMESPACES.get() };
    f(namespaces)
}

fn with_private_namespace<R>(
    id: PrivateNamespaceId,
    f: impl FnOnce(&mut PrivateNamespace<ObjectId, PRIVATE_NAMESPACE_NODES>) -> Result<R, NsError>,
) -> Result<R, ObjError> {
    with_private_namespaces(|namespaces| {
        let Some(slot) = namespaces.get_mut(usize::from(id.slot)) else {
            return Err(ObjError::PrivateNamespaceNotFound);
        };
        if slot.generation != id.generation {
            return Err(ObjError::PrivateNamespaceNotFound);
        }
        let Some(namespace) = slot.namespace.as_mut() else {
            return Err(ObjError::PrivateNamespaceNotFound);
        };
        f(namespace).map_err(ObjError::Namespace)
    })
}

/// Run `f` with the descriptor table. The same single-processor, non-reentrant rule as
/// [`with_namespace`] applies.
fn with_descriptors<R>(f: impl FnOnce(&mut [Option<SecurityDescriptor>; MAX_OBJECTS]) -> R) -> R {
    // SAFETY: as `with_namespace`: the boot processor is the only user, and `f` cannot re-enter.
    let descriptors = unsafe { &mut *DESCRIPTORS.get() };
    f(descriptors)
}

/// Run `f` with the audit log. The same rule as [`with_namespace`] applies.
fn with_audit<R>(f: impl FnOnce(&mut AuditLog<AUDIT_RECORDS>) -> R) -> R {
    // SAFETY: as `with_namespace`: the boot processor is the only user, and `f` cannot re-enter.
    let audit = unsafe { &mut *AUDIT.get() };
    f(audit)
}

fn record_namespace_access(token: &Token, desired: u32, success: bool) {
    with_audit(|audit| {
        audit.record(AuditRecord {
            subject: token.user(),
            desired,
            success,
        });
    });
}

fn record_private_namespace_access<T>(token: &Token, desired: u32, result: &Result<T, ObjError>) {
    if matches!(result, Err(ObjError::PrivateNamespaceNotFound)) {
        return;
    }
    let success = !matches!(
        result,
        Err(ObjError::Namespace(NsError::AccessDenied { .. }))
    );
    record_namespace_access(token, desired, success);
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

/// The registered type of a live object.
fn object_type_of(id: ObjectId) -> Result<&'static ObjectType, ObjError> {
    let type_index = header_of(id)?.type_index.load(Ordering::Acquire);
    type_by_index(type_index).ok_or(ObjError::UnknownType)
}

/// Clear a header slot after a create callback rejects the object before publication.
fn abandon_slot(id: ObjectId) {
    let index = usize::from(id);
    with_descriptors(|descriptors| descriptors[index] = None);
    LIVE[index].store(false, Ordering::Release);
}

/// Free a header slot and its descriptor after invoking the type's delete callback.
fn free_slot(id: ObjectId) {
    if let Ok(object_type) = object_type_of(id) {
        if let Some(delete) = object_type.delete {
            delete(id);
        }
    }
    abandon_slot(id);
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

/// `(decisions made, decisions refused)` since boot.
#[must_use]
pub fn audit_totals() -> (u64, u64) {
    with_audit(|audit| (audit.total(), audit.failures()))
}

/// Ensure the per-session object directory `\Sessions\<session_id>` exists.
pub fn create_session_namespace(session_id: u32) -> Result<(), ObjError> {
    let mut digits = [0u8; 10];
    let session_name = decimal_u32(session_id, &mut digits)?;
    let mut path = [0u8; 20];
    let prefix = b"\\Sessions\\";
    path[..prefix.len()].copy_from_slice(prefix);
    path[prefix.len()..prefix.len() + session_name.len()].copy_from_slice(session_name.as_bytes());
    let path_len = prefix.len() + session_name.len();
    let session_path = core::str::from_utf8(&path[..path_len])
        .map_err(|_| ObjError::Check("session path formatter produced invalid UTF-8"))?;

    with_namespace(|namespace| {
        ensure_directory(namespace, "\\", "Sessions", "\\Sessions")?;
        ensure_directory(namespace, "\\Sessions", session_name, session_path)
    })
    .map_err(ObjError::Namespace)
}

fn decimal_u32(value: u32, buffer: &mut [u8; 10]) -> Result<&str, ObjError> {
    let mut value = value;
    let mut start = buffer.len();
    loop {
        start -= 1;
        buffer[start] = b'0'
            + u8::try_from(value % 10)
                .map_err(|_| ObjError::Check("session id digit was out of range"))?;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    core::str::from_utf8(&buffer[start..])
        .map_err(|_| ObjError::Check("session id formatter produced invalid UTF-8"))
}

fn ensure_directory(
    namespace: &mut Namespace<ObjectId, NAMESPACE_NODES>,
    parent: &str,
    name: &str,
    path: &str,
) -> Result<(), NsError> {
    match namespace.is_directory(path) {
        Ok(true) => Ok(()),
        Ok(false) => Err(NsError::NotADirectory),
        Err(NsError::NotFound) => namespace.create_directory(parent, name),
        Err(error) => Err(error),
    }
}

/// Build the root namespace with the executive's standard directories and boot session.
pub fn init(console: &mut dyn HalConsole) -> Result<(), ObjError> {
    with_namespace(|ns| ns.ensure_root());
    for directory in [
        "Device",
        "FileSystem",
        "BaseNamedObjects",
        "NOLX",
        "Sessions",
    ] {
        with_namespace(|ns| ns.create_directory("\\", directory)).map_err(ObjError::Namespace)?;
    }
    create_session_namespace(0)?;
    let _ = writeln!(
        console,
        "obj: namespace: \\Device \\FileSystem \\BaseNamedObjects \\NOLX"
    );
    let _ = writeln!(console, "obj: session namespaces: \\Sessions\\0");
    Ok(())
}

/// Create an empty private namespace with a boundary security descriptor.
pub fn create_private_namespace(
    boundary: SecurityDescriptor,
) -> Result<PrivateNamespaceId, ObjError> {
    with_private_namespaces(|namespaces| {
        let (index, slot) = namespaces
            .iter_mut()
            .enumerate()
            .find(|(_, slot)| slot.namespace.is_none())
            .ok_or(ObjError::NoPrivateNamespaceSlot)?;
        let slot_index = u8::try_from(index).map_err(|_| ObjError::NoPrivateNamespaceSlot)?;
        let generation = slot.generation.wrapping_add(1).max(1);
        slot.generation = generation;
        slot.namespace = Some(PrivateNamespace::new(boundary));
        Ok(PrivateNamespaceId {
            slot: slot_index,
            generation,
        })
    })
}

/// Destroy an empty private namespace. Its generation changes the next time the slot is reused.
pub fn destroy_private_namespace(id: PrivateNamespaceId) -> Result<(), ObjError> {
    with_private_namespaces(|namespaces| {
        let Some(slot) = namespaces.get_mut(usize::from(id.slot)) else {
            return Err(ObjError::PrivateNamespaceNotFound);
        };
        if slot.generation != id.generation {
            return Err(ObjError::PrivateNamespaceNotFound);
        }
        let Some(namespace) = slot.namespace.as_ref() else {
            return Err(ObjError::PrivateNamespaceNotFound);
        };
        if !namespace.is_empty() {
            return Err(ObjError::Namespace(NsError::DirectoryNotEmpty));
        }
        slot.namespace = None;
        Ok(())
    })
}

/// Allocate a header and run the type-specific create callback before publication.
fn allocate_object(
    name: &str,
    type_name: &str,
    descriptor: SecurityDescriptor,
) -> Result<(ObjectId, &'static ObjectType), ObjError> {
    let (type_index, object_type) = type_by_name(type_name).ok_or(ObjError::UnknownType)?;
    let slot = LIVE
        .iter()
        .position(|live| {
            live.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        })
        .ok_or(ObjError::NoObjectSlot)?;
    let id = match ObjectId::try_from(slot) {
        Ok(id) => id,
        Err(_) => {
            LIVE[slot].store(false, Ordering::Release);
            return Err(ObjError::NoObjectSlot);
        }
    };
    let header = &HEADERS[slot];
    header.type_index.store(type_index, Ordering::Relaxed);
    header.handle_count.store(0, Ordering::Relaxed);
    header.pointer_count.store(1, Ordering::Relaxed);
    with_descriptors(|descriptors| descriptors[slot] = Some(descriptor));

    if let Some(create) = object_type.create {
        if let Err(error) = create(id, name, &descriptor) {
            abandon_slot(id);
            return Err(error);
        }
    }
    Ok((id, object_type))
}

/// Bind a previously initialized object into the root namespace.
fn bind_object(parent: &str, name: &str, id: ObjectId) -> Result<(), ObjError> {
    with_namespace(|namespace| namespace.insert_object(parent, name, id))
        .map_err(ObjError::Namespace)
}

/// Create an object of type `type_name` with `descriptor`, and bind it to `parent\name`.
///
/// The creator's pointer reference is handed to the namespace, so the object lives until the
/// binding is removed and every handle to it is closed.
pub fn create_object(
    parent: &str,
    name: &str,
    type_name: &str,
    descriptor: SecurityDescriptor,
) -> Result<ObjectId, ObjError> {
    let (id, _) = allocate_object(name, type_name, descriptor)?;
    match bind_object(parent, name, id) {
        Ok(()) => Ok(id),
        Err(error) => {
            free_slot(id);
            Err(error)
        }
    }
}

/// Create a directory in a private namespace after checking its boundary descriptor.
pub fn create_private_directory(
    namespace_id: PrivateNamespaceId,
    token: &Token,
    parent: &str,
    name: &str,
) -> Result<(), ObjError> {
    let result = with_private_namespace(namespace_id, |namespace| {
        namespace.create_directory(token, parent, name)
    });
    record_private_namespace_access(token, rights::WRITE, &result);
    result
}

/// Create a symbolic link in a private namespace after checking its boundary descriptor.
pub fn create_private_link(
    namespace_id: PrivateNamespaceId,
    token: &Token,
    parent: &str,
    name: &str,
    target: &str,
) -> Result<(), ObjError> {
    let result = with_private_namespace(namespace_id, |namespace| {
        namespace.create_symlink(token, parent, name, target)
    });
    record_private_namespace_access(token, rights::WRITE, &result);
    result
}

/// Create and bind an object inside a private namespace.
pub fn create_private_object(
    namespace_id: PrivateNamespaceId,
    token: &Token,
    parent: &str,
    name: &str,
    type_name: &str,
    descriptor: SecurityDescriptor,
) -> Result<ObjectId, ObjError> {
    let (id, _) = allocate_object(name, type_name, descriptor)?;
    let result = with_private_namespace(namespace_id, |namespace| {
        namespace.insert_object(token, parent, name, id)
    });
    record_private_namespace_access(token, rights::WRITE, &result);
    match result {
        Ok(()) => Ok(id),
        Err(error) => {
            free_slot(id);
            Err(error)
        }
    }
}

/// Run the type-specific parse callback for a resolved object/path pair.
fn parse_object(id: ObjectId, path: &str) -> Result<(), ObjError> {
    if let Some(parse) = object_type_of(id)?.parse {
        parse(id, path)?;
    }
    Ok(())
}

/// Look up an object in a private namespace after checking its boundary descriptor.
pub fn lookup_private(
    namespace_id: PrivateNamespaceId,
    token: &Token,
    path: &str,
) -> Result<ObjectId, ObjError> {
    let result = with_private_namespace(namespace_id, |namespace| {
        namespace.lookup(token, path).copied()
    });
    record_private_namespace_access(token, rights::READ, &result);
    let id = result?;
    parse_object(id, path)?;
    Ok(id)
}

/// Open a handle to an object in a private namespace.
pub fn open_private<const N: usize>(
    namespace_id: PrivateNamespaceId,
    path: &str,
    token: &Token,
    table: &mut HandleTable<ObjectId, N>,
    desired: u32,
) -> Result<Handle, ObjError> {
    open_id(
        lookup_private(namespace_id, token, path)?,
        token,
        table,
        desired,
    )
}

/// Remove an entry from a private namespace and release its object reference, if any.
///
/// Returns `true` when the removed entry was an object binding; directories and symbolic links
/// are still removed but return `false`.
pub fn unbind_private(
    namespace_id: PrivateNamespaceId,
    token: &Token,
    path: &str,
) -> Result<bool, ObjError> {
    let result = with_private_namespace(namespace_id, |namespace| namespace.remove(token, path));
    record_private_namespace_access(token, rights::DELETE, &result);
    match result? {
        Some(id) => {
            release_pointer(id)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Run type validation and `SeAccessCheck` for `token` and `desired` against `id`.
/// Returns the rights granted, which equal `desired` on success.
fn check_access(id: ObjectId, token: &Token, desired: u32) -> Result<u32, ObjError> {
    let object_type = object_type_of(id)?;
    if object_type
        .validate_access
        .is_some_and(|validate| !validate(desired))
    {
        return Err(ObjError::UnsupportedAccess { requested: desired });
    }
    let descriptor = with_descriptors(|descriptors| descriptors[usize::from(id)])
        .ok_or(ObjError::Check("a live object has no security descriptor"))?;
    let decision = access_check(token, &descriptor, desired);
    let success = matches!(decision, Decision::Granted(_));
    with_audit(|audit| {
        audit.record(AuditRecord {
            subject: token.user(),
            desired,
            success,
        });
    });
    match decision {
        Decision::Granted(granted) => Ok(granted),
        Decision::Denied { missing } => Err(ObjError::AccessDenied { missing }),
    }
}

/// Open a handle to an already resolved object id.
fn open_id<const N: usize>(
    id: ObjectId,
    token: &Token,
    table: &mut HandleTable<ObjectId, N>,
    desired: u32,
) -> Result<Handle, ObjError> {
    let granted = check_access(id, token, desired)?;
    let object_type = object_type_of(id)?;
    let header = header_of(id)?;
    header.add_handle();
    let handle = match table.insert(id, granted, false) {
        Ok(handle) => handle,
        Err(error) => {
            header.drop_handle();
            release_pointer(id)?;
            return Err(ObjError::Handle(error));
        }
    };
    if let Some(open) = object_type.open {
        if let Err(error) = open(id, token, desired, granted) {
            let _ = table.close(handle);
            header.drop_handle();
            release_pointer(id)?;
            return Err(error);
        }
    }
    Ok(handle)
}

/// Open a handle to the object at `path` for `token`, asking for `desired` rights.
///
/// The handle carries the rights the monitor granted. A request that is refused in part is
/// refused in whole, and then no handle is created and no reference is taken.
pub fn open<const N: usize>(
    path: &str,
    token: &Token,
    table: &mut HandleTable<ObjectId, N>,
    desired: u32,
) -> Result<Handle, ObjError> {
    open_id(lookup(path)?, token, table, desired)
}

/// Duplicate `source_handle` from `source` into `target`, per `DuplicateHandle` rules.
///
/// Duplication does not consult the monitor again: a duplicate can only narrow rights the
/// caller already holds, and the handle table enforces that.
pub fn duplicate<const N: usize, const M: usize>(
    source: &HandleTable<ObjectId, N>,
    source_handle: Handle,
    target: &mut HandleTable<ObjectId, M>,
    access: u32,
) -> Result<Handle, ObjError> {
    let id = source.get(source_handle).map_err(ObjError::Handle)?.object;
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
pub fn close<const N: usize>(
    table: &mut HandleTable<ObjectId, N>,
    handle: Handle,
) -> Result<(), ObjError> {
    let entry = table.close(handle).map_err(ObjError::Handle)?;
    let object_type = object_type_of(entry.object)?;
    if let Some(close) = object_type.close {
        close(entry.object, handle);
    }
    header_of(entry.object)?.drop_handle();
    release_pointer(entry.object)
}

/// Remove the binding at `path`, dropping the namespace's pointer reference.
///
/// Returns `true` if an object binding was removed. Directories and links are removed too,
/// but carry no reference, so they return `false`.
pub fn unbind(path: &str) -> Result<bool, ObjError> {
    match with_namespace(|ns| ns.remove(path)).map_err(ObjError::Namespace)? {
        Some(id) => {
            release_pointer(id)?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Look up the id bound at `path`, following links and running the type's parse callback.
pub fn lookup(path: &str) -> Result<ObjectId, ObjError> {
    let id =
        with_namespace(|namespace| namespace.lookup(path).copied()).map_err(ObjError::Namespace)?;
    parse_object(id, path)?;
    Ok(id)
}

/// Create a symbolic link `parent\name` to `target`.
pub fn link(parent: &str, name: &str, target: &str) -> Result<(), ObjError> {
    with_namespace(|ns| ns.create_symlink(parent, name, target)).map_err(ObjError::Namespace)
}

fn check(condition: bool, message: &'static str) -> Result<(), ObjError> {
    if condition {
        Ok(())
    } else {
        Err(ObjError::Check(message))
    }
}

fn acl_of(entries: &[Ace]) -> Result<Acl, ObjError> {
    let mut acl = Acl::empty();
    for entry in entries {
        acl.push(*entry)
            .map_err(|_| ObjError::Check("a self-test ACL overflowed its capacity"))?;
    }
    Ok(acl)
}

/// Attempt to open `path` for `token` and `desired`, closing the handle again on success.
/// Returns whether the monitor allowed it; any other failure is an error.
fn probe_access(
    path: &str,
    token: &Token,
    desired: u32,
    table: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
) -> Result<bool, ObjError> {
    match open(path, token, table, desired) {
        Ok(handle) => {
            close(table, handle)?;
            Ok(true)
        }
        Err(ObjError::AccessDenied { .. }) => Ok(false),
        Err(other) => Err(other),
    }
}

/// The object manager's boot self test.
///
/// Two parts. The first drives one object through every reference transition the rules allow
/// and checks the counts after each step: created, opened, duplicated into a second table, a
/// duplicate that widens rights refused, closed in the first table, its name removed while the
/// second table still holds it, and finally closed. The object must be freed exactly when the
/// last reference goes, and the live count must return to where it started.
///
/// The second part runs a fixed set of access checks through [`open`] and requires each to come
/// out as expected: a grant, a deny entry, a backup privilege, a null-free label refusal, and
/// the owner's rights. The audit log must have recorded exactly those decisions.
pub fn selftest(console: &mut dyn HalConsole) -> Result<(), ObjError> {
    const READ: u32 = rights::READ;
    const WRITE: u32 = rights::WRITE;
    const READ_WRITE: u32 = rights::READ | rights::WRITE;

    let system = Token::new(Sid::SYSTEM, IntegrityLevel::System);
    let reader = Token::new(Sid::user(1000), IntegrityLevel::Medium);
    let refused_user = Token::new(Sid::user(1001), IntegrityLevel::Medium);
    let backup_operator =
        Token::new(Sid::user(1002), IntegrityLevel::Medium).with_privileges(privilege::BACKUP);
    let low_reader = Token::new(Sid::user(1000), IntegrityLevel::Low);

    // The device: the owner is SYSTEM; user 1001 is refused everything before anything else
    // can grant it; user 1000 may read.
    let device_acl = acl_of(&[
        Ace {
            kind: AceKind::Deny,
            sid: Sid::user(1001),
            mask: rights::ALL,
        },
        Ace {
            kind: AceKind::Allow,
            sid: Sid::SYSTEM,
            mask: rights::ALL,
        },
        Ace {
            kind: AceKind::Allow,
            sid: Sid::user(1000),
            mask: READ,
        },
    ])?;
    let device = SecurityDescriptor {
        owner: Sid::SYSTEM,
        dacl: Some(device_acl),
        label: None,
    };
    // The log: anyone may read and write as far as the DACL goes, but the Medium label refuses
    // writes from any token below Medium.
    let log_acl = acl_of(&[Ace {
        kind: AceKind::Allow,
        sid: Sid::WORLD,
        mask: rights::ALL,
    }])?;
    let log = SecurityDescriptor {
        owner: Sid::SYSTEM,
        dacl: Some(log_acl),
        label: Some(IntegrityLevel::Medium),
    };

    let live_before = live_objects();
    let (audit_before, failures_before) = audit_totals();
    let callback_counts_before = device_callback_counts();
    create_session_namespace(1)?;
    check(
        with_namespace(|ns| {
            ns.is_directory("\\Sessions\\0") == Ok(true)
                && ns.is_directory("\\Sessions\\1") == Ok(true)
        }),
        "the object manager did not create both session directories",
    )?;
    let mut first: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();
    let mut second: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();

    // Part one: the reference lifecycle, as the SYSTEM token.
    check(
        create_object("\\Device", "", "Device", device)
            == Err(ObjError::Check("device object name is empty"))
            && live_objects() == live_before,
        "a rejected type create callback leaked its header slot",
    )?;
    let id = create_object("\\Device", "Null", "Device", device)?;
    link("\\", "NullLink", "\\Device\\Null")?;
    check(
        lookup("\\NullLink")? == id,
        "a symbolic link does not resolve to its object",
    )?;
    check(
        reference_counts(id)? == (1, 0),
        "a new object should hold only the namespace's reference",
    )?;

    check(
        open("\\Device\\Null", &system, &mut first, u32::MAX)
            == Err(ObjError::UnsupportedAccess {
                requested: u32::MAX,
            })
            && first.is_empty()
            && reference_counts(id)? == (1, 0),
        "the Device type accepted rights outside its access mask",
    )?;
    check(
        open("\\Device\\Null", &system, &mut first, rights::EXECUTE)
            == Err(ObjError::UnsupportedAccess {
                requested: rights::EXECUTE,
            })
            && first.is_empty()
            && reference_counts(id)? == (1, 0),
        "a rejected type open callback leaked a handle or reference",
    )?;

    let a = open("\\Device\\Null", &system, &mut first, READ)?;
    check(
        reference_counts(id)? == (2, 1),
        "open did not add a reference pair",
    )?;

    // Widening rights must be refused by the handle table, and a refused duplicate must leave
    // no trace.
    let refused = duplicate(&first, a, &mut second, READ_WRITE);
    check(
        refused == Err(ObjError::Handle(HandleError::AccessDenied)),
        "a duplicate that widened access was not refused",
    )?;
    check(
        reference_counts(id)? == (2, 1),
        "a refused duplicate changed the reference counts",
    )?;

    let b = duplicate(&first, a, &mut second, ACCESS_ALL)?;
    check(
        reference_counts(id)? == (3, 2),
        "a duplicate did not add a reference pair",
    )?;
    check(
        second.get(b).map(|entry| entry.access) == Ok(READ),
        "a same-access duplicate did not keep the source's rights",
    )?;

    close(&mut first, a)?;
    check(
        reference_counts(id)? == (2, 1),
        "closing a handle did not drop its pair",
    )?;
    check(
        first.get(a) == Err(HandleError::Invalid),
        "a closed handle still resolves in its table",
    )?;

    // Remove the name while the second table still holds the object: the object must survive,
    // the name must be gone, and the link to it must dangle.
    check(
        unbind("\\Device\\Null")?,
        "the binding was not found to remove",
    )?;
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

    // Part two: access checks. Each case states what the monitor must decide. The device is
    // created again, because part one freed it.
    create_object("\\Device", "Null", "Device", device)?;
    let log_id = create_object("\\Device", "Log", "Device", log)?;
    check(
        lookup("\\Device\\Log")? == log_id,
        "the log object did not bind",
    )?;

    let cases: [(&str, Token, u32, bool); 10] = [
        ("\\Device\\Null", reader, READ, true),
        ("\\Device\\Null", reader, WRITE, false),
        ("\\Device\\Null", refused_user, READ, false),
        ("\\Device\\Null", backup_operator, READ, true),
        ("\\Device\\Null", backup_operator, WRITE, false),
        ("\\Device\\Null", system, READ_WRITE | rights::DELETE, true),
        ("\\Device\\Log", low_reader, READ, true),
        ("\\Device\\Log", low_reader, WRITE, false),
        ("\\Device\\Log", reader, WRITE, true),
        ("\\Device\\Log", system, WRITE, true),
    ];
    let mut probe: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();
    for (path, token, desired, allowed) in cases {
        let granted = probe_access(path, &token, desired, &mut probe)?;
        check(
            granted == allowed,
            "the monitor's decision differed from the expected one",
        )?;
    }
    check(probe.is_empty(), "a probe left a handle open")?;

    let (audit_after, failures_after) = audit_totals();
    let decisions = audit_after - audit_before;
    let refusals = failures_after - failures_before;
    // The lifecycle opens that reach the SRM (one later rejected by the type callback), plus
    // one decision for each access-check case.
    check(
        decisions == cases.len() as u64 + 2,
        "the audit log did not record every access decision",
    )?;
    let expected_refusals = cases.iter().filter(|case| !case.3).count() as u64;
    check(
        refusals == expected_refusals,
        "the audit log's refusal count disagrees with the cases",
    )?;

    // Tear the boot objects down so the pool is back where it started.
    unbind("\\Device\\Log")?;
    unbind("\\Device\\Null")?;
    check(
        live_objects() == live_before,
        "the boot self test's objects were not all freed",
    )?;

    // Two sessions may bind the same name without sharing an object namespace.
    let session_zero = create_object("\\Sessions\\0", "SharedName", "Device", device)?;
    let session_one = create_object("\\Sessions\\1", "SharedName", "Device", device)?;
    check(
        session_zero != session_one
            && lookup("\\Sessions\\0\\SharedName")? == session_zero
            && lookup("\\Sessions\\1\\SharedName")? == session_one,
        "per-session object namespaces resolved the same name to shared state",
    )?;
    unbind("\\Sessions\\0\\SharedName")?;
    unbind("\\Sessions\\1\\SharedName")?;
    check(
        live_objects() == live_before,
        "session namespace selftest leaked object headers",
    )?;

    // Private namespaces are independently rooted and require their boundary token on every
    // lookup, insertion, and removal.
    let boundary_acl = acl_of(&[Ace {
        kind: AceKind::Allow,
        sid: reader.user(),
        mask: rights::ALL,
    }])?;
    let boundary = SecurityDescriptor {
        owner: Sid::SYSTEM,
        dacl: Some(boundary_acl),
        label: None,
    };
    let private_one = create_private_namespace(boundary)?;
    let private_two = create_private_namespace(boundary)?;
    let private_audit_before = audit_totals();
    create_private_directory(private_one, &reader, "\\", "Private")?;
    create_private_directory(private_two, &reader, "\\", "Private")?;
    let private_object_one = create_private_object(
        private_one,
        &reader,
        "\\Private",
        "Shared",
        "Device",
        device,
    )?;
    let private_object_two = create_private_object(
        private_two,
        &reader,
        "\\Private",
        "Shared",
        "Device",
        device,
    )?;
    create_private_link(private_one, &reader, "\\", "Alias", "\\Private\\Shared")?;
    let private_lookup_one = lookup_private(private_one, &reader, "\\Private\\Shared")?;
    let private_lookup_two = lookup_private(private_two, &reader, "\\Private\\Shared")?;
    check(
        private_object_one != private_object_two
            && private_lookup_one == private_object_one
            && private_lookup_two == private_object_two,
        "private namespaces did not isolate identical object paths",
    )?;
    let mut private_handles: HandleTable<ObjectId, 2> = HandleTable::new();
    let private_handle = open_private(
        private_one,
        "\\Alias",
        &reader,
        &mut private_handles,
        rights::READ,
    )?;
    let private_handle_object = private_handles
        .get(private_handle)
        .map(|entry| entry.object);
    check(
        private_handle_object == Ok(private_object_one)
            && reference_counts(private_object_one)? == (2, 1),
        "a private namespace open did not own the expected object handle",
    )?;
    let private_object_one_unbound = unbind_private(private_one, &reader, "\\Private\\Shared")?;
    check(
        private_object_one_unbound && reference_counts(private_object_one)? == (1, 1),
        "removing a private binding did not leave its open handle alive",
    )?;
    close(&mut private_handles, private_handle)?;
    check(
        header_of(private_object_one).is_err(),
        "closing the last private handle did not free its unbound object",
    )?;
    let boundary_denial = lookup_private(private_one, &refused_user, "\\Private\\Shared");
    check(
        boundary_denial
            == Err(ObjError::Namespace(NsError::AccessDenied {
                missing: rights::READ,
            })),
        "a token outside the private namespace boundary was allowed to look up an object",
    )?;
    check(
        destroy_private_namespace(private_one)
            == Err(ObjError::Namespace(NsError::DirectoryNotEmpty)),
        "a nonempty private namespace was destroyed",
    )?;
    let object_two_unbound = unbind_private(private_two, &reader, "\\Private\\Shared")?;
    check(
        object_two_unbound,
        "a private object binding was not removed",
    )?;
    let alias_unbound = unbind_private(private_one, &reader, "\\Alias")?;
    let directory_one_unbound = unbind_private(private_one, &reader, "\\Private")?;
    let directory_two_unbound = unbind_private(private_two, &reader, "\\Private")?;
    check(
        !alias_unbound && !directory_one_unbound && !directory_two_unbound,
        "private namespace links or directories were reported as object bindings",
    )?;
    destroy_private_namespace(private_one)?;
    destroy_private_namespace(private_two)?;
    let reused_private = create_private_namespace(boundary)?;
    let stale_lookup = lookup_private(private_one, &reader, "\\Private\\Shared");
    check(
        reused_private != private_one && stale_lookup == Err(ObjError::PrivateNamespaceNotFound),
        "a stale private namespace id referred to a reused slot",
    )?;
    destroy_private_namespace(reused_private)?;
    let private_audit_after = audit_totals();
    check(
        private_audit_after.0 - private_audit_before.0 == 15
            && private_audit_after.1 - private_audit_before.1 == 1,
        "private namespace boundary decisions were not audited exactly",
    )?;
    check(
        live_objects() == live_before,
        "private namespace selftest leaked object headers",
    )?;

    let callback_counts_after = device_callback_counts();
    check(
        callback_counts_after
            .iter()
            .zip(callback_counts_before)
            .all(|(after, before)| *after > before),
        "not every Device object-type callback ran",
    )?;

    let _ = writeln!(
        console,
        "NOLX: obj session selftest: sessions 0 and 1 are isolated"
    );
    let _ = writeln!(
        console,
        "NOLX: obj private namespace selftest: boundaries enforced, isolated roots, \
         stale ids rejected, audit delta {} ({} refused)",
        private_audit_after.0 - private_audit_before.0,
        private_audit_after.1 - private_audit_before.1,
    );
    let _ = writeln!(
        console,
        "NOLX: obj type callback selftest: create/open/parse/close/delete invoked"
    );
    let _ = writeln!(
        console,
        "NOLX: obj selftest: 1 object, 2 handles (1 duplicated, 1 widening refused), \
         1 symlink lookup, refcount (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, \
         live objects {live_before} -> {live_before}; audit ok"
    );
    let _ = writeln!(
        console,
        "NOLX: srm selftest: {} access checks ({} granted, {} denied), 1 backup bypass, \
         1 integrity refusal, 1 deny entry, SRM audit snapshot {} ({} refused); audit ok",
        cases.len(),
        cases.len() as u64 - expected_refusals,
        expected_refusals,
        audit_after,
        failures_after,
    );
    Ok(())
}
