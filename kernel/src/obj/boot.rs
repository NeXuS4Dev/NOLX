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

use core::sync::atomic::{AtomicBool, Ordering};

use nolx_hal_core::console::HalConsole;
use nolx_obj_core::handle::{Handle, HandleError, HandleTable, ACCESS_ALL};
use nolx_obj_core::namespace::{Namespace, NsError};
use nolx_obj_core::security::{
    access_check, privilege, rights, Ace, AceKind, Acl, AuditLog, AuditRecord, Decision,
    IntegrityLevel, SecurityDescriptor, Sid, Token,
};

use super::{type_by_name, ObjectHeader};
use crate::sync::SyncCell;

/// Objects the boot-time pool holds at once.
pub const MAX_OBJECTS: usize = 64;

/// Nodes the root namespace can hold, including the root.
const NAMESPACE_NODES: usize = 32;

/// Entries in a handle table. Small on purpose: a process's table is quota-charged, and the
/// self test only needs a few.
pub const HANDLE_TABLE_ENTRIES: usize = 16;

/// Access decisions the audit log keeps in full. Totals are kept exactly beyond this.
const AUDIT_RECORDS: usize = 16;

/// What the namespace binds names to: an index into the header pool.
pub type ObjectId = u16;

/// The header pool. A slot is in use while its `LIVE` flag is set.
static HEADERS: [ObjectHeader; MAX_OBJECTS] = [const { ObjectHeader::new(0, 0) }; MAX_OBJECTS];
static LIVE: [AtomicBool; MAX_OBJECTS] = [const { AtomicBool::new(false) }; MAX_OBJECTS];

/// The root namespace. Built empty at compile time and given its root by [`init`], so it lives
/// in `.data` rather than being assembled on the boot stack.
static NAMESPACE: SyncCell<Namespace<ObjectId, NAMESPACE_NODES>> =
    SyncCell::new(Namespace::empty());

/// The security descriptor of each live object, by slot.
static DESCRIPTORS: SyncCell<[Option<SecurityDescriptor>; MAX_OBJECTS]> =
    SyncCell::new([None; MAX_OBJECTS]);

/// Every access decision made through [`open`].
static AUDIT: SyncCell<AuditLog<AUDIT_RECORDS>> = SyncCell::new(AuditLog::empty());

/// Why an object-manager operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObjError {
    /// No object type of that name is registered.
    UnknownType,
    /// Every header slot is in use.
    NoObjectSlot,
    /// A namespace operation failed.
    Namespace(NsError),
    /// A handle operation failed.
    Handle(HandleError),
    /// The security monitor refused the rights asked for.
    AccessDenied {
        /// The rights that were asked for and refused.
        missing: u32,
    },
    /// A self-test check did not hold, or an object was used after it was freed. The string
    /// says which.
    Check(&'static str),
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

/// Run `f` with the descriptor table. The same single-processor, non-reentrant rule as
/// [`with_namespace`] applies.
fn with_descriptors<R>(
    f: impl FnOnce(&mut [Option<SecurityDescriptor>; MAX_OBJECTS]) -> R,
) -> R {
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

/// The header for a live object.
fn header_of(id: ObjectId) -> Result<&'static ObjectHeader, ObjError> {
    let index = usize::from(id);
    if index < MAX_OBJECTS && LIVE[index].load(Ordering::Acquire) {
        Ok(&HEADERS[index])
    } else {
        Err(ObjError::Check("reference to an object that is not live"))
    }
}

/// Free a header slot and its descriptor. The object body, if any, was torn down by the caller.
fn free_slot(id: ObjectId) {
    let index = usize::from(id);
    with_descriptors(|descriptors| descriptors[index] = None);
    LIVE[index].store(false, Ordering::Release);
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

/// Build the root namespace with the executive's standard directories.
pub fn init(console: &mut dyn HalConsole) -> Result<(), ObjError> {
    with_namespace(|ns| ns.ensure_root());
    for directory in ["Device", "FileSystem", "BaseNamedObjects", "NOLX"] {
        with_namespace(|ns| ns.create_directory("\\", directory)).map_err(ObjError::Namespace)?;
    }
    let _ = writeln!(
        console,
        "obj: namespace: \\Device \\FileSystem \\BaseNamedObjects \\NOLX"
    );
    Ok(())
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
    with_descriptors(|descriptors| descriptors[slot] = Some(descriptor));

    match with_namespace(|ns| ns.insert_object(parent, name, id)) {
        Ok(()) => Ok(id),
        Err(error) => {
            free_slot(id);
            Err(ObjError::Namespace(error))
        }
    }
}

/// Run `SeAccessCheck` for `token` and `desired` against object `id`, recording the decision.
/// Returns the rights granted, which equal `desired` on success.
fn check_access(id: ObjectId, token: &Token, desired: u32) -> Result<u32, ObjError> {
    header_of(id)?;
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

/// Open a handle to the object at `path` for `token`, asking for `desired` rights.
///
/// The handle carries the rights the monitor granted. A request that is refused in part is
/// refused in whole, and then no handle is created and no reference is taken.
pub fn open(
    path: &str,
    token: &Token,
    table: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    desired: u32,
) -> Result<Handle, ObjError> {
    let id = lookup(path)?;
    let granted = check_access(id, token, desired)?;
    let header = header_of(id)?;
    header.add_handle();
    match table.insert(id, granted, false) {
        Ok(handle) => Ok(handle),
        Err(error) => {
            header.drop_handle();
            release_pointer(id)?;
            Err(ObjError::Handle(error))
        }
    }
}

/// Duplicate `source_handle` from `source` into `target`, per `DuplicateHandle` rules.
///
/// Duplication does not consult the monitor again: a duplicate can only narrow rights the
/// caller already holds, and the handle table enforces that.
pub fn duplicate(
    source: &HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
    source_handle: Handle,
    target: &mut HandleTable<ObjectId, HANDLE_TABLE_ENTRIES>,
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

/// Look up the id bound at `path`, following links.
pub fn lookup(path: &str) -> Result<ObjectId, ObjError> {
    with_namespace(|ns| ns.lookup(path).copied()).map_err(ObjError::Namespace)
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
        Token::new(Sid::user(1002), IntegrityLevel::Medium)
            .with_privileges(privilege::BACKUP);
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
    let mut first: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();
    let mut second: HandleTable<ObjectId, HANDLE_TABLE_ENTRIES> = HandleTable::new();

    // Part one: the reference lifecycle, as the SYSTEM token.
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
    // One decision for the lifecycle's open, and one for each case.
    check(
        decisions == cases.len() as u64 + 1,
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

    let _ = writeln!(
        console,
        "NOLX: obj selftest: 1 object, 2 handles (1 duplicated, 1 widening refused), \
         1 symlink lookup, refcount (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, \
         live objects {live_before} -> {live_before}; audit ok"
    );
    let _ = writeln!(
        console,
        "NOLX: srm selftest: {} access checks ({} granted, {} denied), 1 backup bypass, \
         1 integrity refusal, 1 deny entry, audit total {} ({} refused); audit ok",
        cases.len(),
        cases.len() as u64 - expected_refusals,
        expected_refusals,
        audit_after,
        failures_after,
    );
    Ok(())
}
