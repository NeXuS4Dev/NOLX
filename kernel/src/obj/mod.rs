//! The Object Manager.
//!
//! Every kernel resource is an object with a type, a reference count, an optional name, and
//! a security descriptor. Files, sockets, processes, threads, timers, ports, and device
//! nodes all get their lifetime management, security checking, and auditing from here
//! rather than each implementing its own.
//!
//! This module is a skeleton in M3: the object header layout and the type table, which are
//! the parts everything else depends on. Handle tables and the namespace land in M4.

use core::fmt::Write;
use core::sync::atomic::{AtomicI32, AtomicU16, Ordering};

use crate::hal::console::Console;
use crate::sync::SyncCell;

/// The object header, prefixed to every object body.
///
/// The layout matches `docs/architecture/01-kernel-model.md` section 3. It is `repr(C)`
/// because the POSIX subsystem and the debugger both walk it by offset.
#[repr(C)]
pub struct ObjectHeader {
    /// Index into the global object type table.
    pub type_index: AtomicU16,
    /// [`HeaderFlags`].
    pub flags: AtomicU16,
    /// Pointer references held by kernel code.
    pub pointer_count: AtomicI32,
    /// Handle references held by processes.
    ///
    /// Invariant: `handle_count <= pointer_count`, because every handle implies a pointer
    /// reference. Every release path asserts it.
    pub handle_count: AtomicI32,
    /// Size of the body that follows this header, in bytes.
    pub body_size: u32,
}

/// Object header flags.
pub mod header_flags {
    /// The object is not deleted when its last handle closes.
    pub const PERMANENT: u16 = 1 << 0;
    /// Only one handle may exist; a second open fails.
    pub const EXCLUSIVE: u16 = 1 << 1;
    /// Name lookups ignore case. Off by default: NOLX is case-sensitive.
    pub const CASE_INSENSITIVE: u16 = 1 << 2;
    /// Generate an audit record when the last handle closes.
    pub const AUDIT_ON_CLOSE: u16 = 1 << 3;
    /// The object has a security descriptor worth checking. Objects without one skip
    /// `SeAccessCheck` entirely, which is the common case for anonymous objects.
    pub const SECURITY_REQUIRED: u16 = 1 << 4;
}

/// A registered object type.
pub struct ObjectType {
    /// The type's name, for diagnostics and for `\ObjectTypes` in the namespace.
    pub name: &'static str,
    /// Called when the last reference goes away.
    pub delete: Option<fn(*mut u8)>,
    /// Called to validate a requested access mask before a handle is granted.
    pub validate_access: Option<fn(requested: u32) -> bool>,
}

/// The number of object types the table holds. Generous, because adding a type is cheap and
/// growing a static array later is not.
pub const MAX_OBJECT_TYPES: usize = 64;

/// The global object type table.
///
/// Written once during `init`, read afterwards, so it needs no lock. That assumption is
/// recorded here because it is the kind of thing that breaks silently when someone adds
/// runtime type registration.
static TYPE_TABLE: SyncCell<[Option<&'static ObjectType>; MAX_OBJECT_TYPES]> =
    SyncCell::new([None; MAX_OBJECT_TYPES]);
static TYPE_COUNT: SyncCell<usize> = SyncCell::new(0);

/// Built-in object types, registered in this order so their indices are stable. A stable
/// index matters because crash dumps print the index, and a dump from one build should be
/// readable with another's symbol table.
static BUILTIN_TYPES: &[ObjectType] = &[
    ObjectType { name: "Directory", delete: None, validate_access: None },
    ObjectType { name: "SymbolicLink", delete: None, validate_access: None },
    ObjectType { name: "File", delete: None, validate_access: None },
    ObjectType { name: "Device", delete: None, validate_access: None },
    ObjectType { name: "Driver", delete: None, validate_access: None },
    ObjectType { name: "Process", delete: None, validate_access: None },
    ObjectType { name: "Thread", delete: None, validate_access: None },
    ObjectType { name: "Event", delete: None, validate_access: None },
    ObjectType { name: "Mutex", delete: None, validate_access: None },
    ObjectType { name: "Semaphore", delete: None, validate_access: None },
    ObjectType { name: "Timer", delete: None, validate_access: None },
    ObjectType { name: "Port", delete: None, validate_access: None },
    ObjectType { name: "Section", delete: None, validate_access: None },
    ObjectType { name: "Key", delete: None, validate_access: None },
    ObjectType { name: "Token", delete: None, validate_access: None },
];

/// Register the built-in object types.
pub fn init(console: &mut Console) {
    // SAFETY: `init` runs once on the boot processor before any other code reads the
    // table. There is no concurrent reader to race with.
    unsafe {
        let table = &mut *TYPE_TABLE.get();
        let count_ref = &mut *TYPE_COUNT.get();
        for ty in BUILTIN_TYPES {
            if *count_ref < MAX_OBJECT_TYPES {
                table[*count_ref] = Some(ty);
                *count_ref += 1;
            }
        }
        let count = *count_ref;
        let _ = writeln!(console, "obj: registered {count} object types");
    }
}

/// The number of registered object types.
#[must_use]
pub fn type_count() -> usize {
    // SAFETY: written once in `init`, read-only afterwards.
    unsafe { *TYPE_COUNT.get() }
}

/// Look up an object type by index.
#[must_use]
pub fn type_by_index(index: u16) -> Option<&'static ObjectType> {
    // SAFETY: as [`type_count`]; the index is bounds-checked.
    unsafe {
        (*TYPE_TABLE.get())
            .get(usize::from(index))
            .and_then(|slot| *slot)
    }
}

/// Look up an object type by name.
///
/// Linear, because the table is small and this is not a hot path -- it runs at object
/// creation, which is already doing far more work.
#[must_use]
pub fn type_by_name(name: &str) -> Option<(u16, &'static ObjectType)> {
    // SAFETY: as [`type_count`].
    unsafe {
        (*TYPE_TABLE.get())
            .iter()
            .take(*TYPE_COUNT.get())
            .enumerate()
            .find_map(|(i, slot)| slot.map(|ty| (i as u16, ty)).filter(|(_, ty)| ty.name == name))
    }
}

impl ObjectHeader {
    /// A header for a body of `body_size` bytes, of type `type_index`.
    #[must_use]
    pub const fn new(type_index: u16, body_size: u32) -> Self {
        Self {
            type_index: AtomicU16::new(type_index),
            flags: AtomicU16::new(0),
            // A newly created object starts with one pointer reference, held by the
            // creator until it either stores the pointer or drops it. Starting at zero
            // would let the object be freed before the creator ever used it.
            pointer_count: AtomicI32::new(1),
            handle_count: AtomicI32::new(0),
            body_size,
        }
    }

    /// Add a pointer reference.
    pub fn retain(&self) {
        let previous = self.pointer_count.fetch_add(1, Ordering::Relaxed);
        debug_assert!(previous > 0, "retained an object that was already dead");
    }

    /// Drop a pointer reference, returning `true` if this was the last one.
    ///
    /// `Release` ordering so that every write made before the release is visible to the
    /// thread that observes the count reach zero and runs the delete procedure.
    pub fn release(&self) -> bool {
        let previous = self.pointer_count.fetch_sub(1, Ordering::Release);
        debug_assert!(previous > 0, "released an object more times than it was retained");
        if previous == 1 {
            // Make sure we see every write that preceded the final release elsewhere.
            core::sync::atomic::fence(Ordering::Acquire);
            true
        } else {
            false
        }
    }

    /// Note that a handle was granted.
    ///
    /// A handle implies a pointer reference, so this bumps both, which is what maintains
    /// `handle_count <= pointer_count`.
    pub fn add_handle(&self) {
        self.pointer_count.fetch_add(1, Ordering::Relaxed);
        self.handle_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Note that a handle was closed.
    pub fn drop_handle(&self) {
        self.handle_count.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(
            self.handle_count.load(Ordering::Relaxed) <= self.pointer_count.load(Ordering::Relaxed),
            "handle_count exceeded pointer_count"
        );
    }

    /// `true` if the object should survive its last handle closing.
    #[must_use]
    pub fn is_permanent(&self) -> bool {
        self.flags.load(Ordering::Relaxed) & header_flags::PERMANENT != 0
    }
}
