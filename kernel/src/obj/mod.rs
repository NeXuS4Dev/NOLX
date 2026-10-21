//! The Object Manager.
//!
//! The Object Manager owns object types, reference counts, security checks, auditing, and
//! namespace bindings. In the boot-stage implementation, descriptors live in a parallel
//! table and names live on namespace bindings rather than in [`ObjectHeader`]; per-body
//! quota charging is not wired yet. Files, sockets, processes, threads, timers, ports, and
//! device nodes share the same lifetime and security rules rather than implementing their own.
//!
//! M3 landed the object header layout, the type table, and the pool accounting the executive
//! reports through [`pool`]. M4 adds the handle tables and the namespace rules in
//! `crates/nolx-obj-core`, and [`boot`] wires them to real headers: the header pool, the root
//! namespace, and the boot self test.

pub mod boot;
pub mod pool;

use core::sync::atomic::{AtomicI32, AtomicU16, Ordering};

use crate::sync::SyncCell;
use nolx_hal_core::console::HalConsole;
use nolx_obj_core::handle::Handle;
use nolx_obj_core::security::{SecurityDescriptor, Token};

/// The object header, prefixed to every object body.
///
/// The layout matches `docs/architecture/01-kernel-model.md` section 3. It is `repr(C)`
/// because the POSIX subsystem and the debugger both walk it by offset.
#[repr(C)]
pub struct ObjectHeader {
    /// Index into the global object type table.
    pub type_index: AtomicU16,
    /// The bitmask described by [`header_flags`].
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

/// The object type's create callback. It runs after the header and descriptor are initialized,
/// but before the object is published in a namespace.
pub type ObjectCreateCallback =
    fn(boot::ObjectId, &str, &SecurityDescriptor) -> Result<(), boot::ObjError>;
/// The object type's open callback. It runs after the security monitor grants access and after
/// a provisional handle/reference exists; a failure rolls that provisional state back.
pub type ObjectOpenCallback =
    fn(boot::ObjectId, &Token, requested: u32, granted: u32) -> Result<(), boot::ObjError>;
/// The object type's parse callback. It validates a resolved object/path pair after generic
/// namespace traversal and symbolic-link handling.
pub type ObjectParseCallback = fn(boot::ObjectId, path: &str) -> Result<(), boot::ObjError>;
/// The object type's close callback, called for each successfully closed handle.
pub type ObjectCloseCallback = fn(boot::ObjectId, Handle);
/// The object type's delete callback, called once when the final reference is released.
pub type ObjectDeleteCallback = fn(boot::ObjectId);

/// A registered object type and its operation callbacks.
pub struct ObjectType {
    /// The type's name, for diagnostics and for `\ObjectTypes` in the namespace.
    pub name: &'static str,
    /// Called before a newly created object is published in a namespace.
    pub create: Option<ObjectCreateCallback>,
    /// Called after SRM validation when a provisional handle is installed; errors roll it back.
    pub open: Option<ObjectOpenCallback>,
    /// Called after a namespace path resolves to an object of this type.
    pub parse: Option<ObjectParseCallback>,
    /// Called when one of the object's handles is closed.
    pub close: Option<ObjectCloseCallback>,
    /// Called when the last reference goes away.
    pub delete: Option<ObjectDeleteCallback>,
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
    ObjectType {
        name: "Directory",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "SymbolicLink",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "File",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Device",
        create: Some(boot::device_create),
        open: Some(boot::device_open),
        parse: Some(boot::device_parse),
        close: Some(boot::device_close),
        delete: Some(boot::device_delete),
        validate_access: Some(boot::device_validate_access),
    },
    ObjectType {
        name: "Driver",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Process",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Thread",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Event",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Mutex",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Semaphore",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Timer",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Port",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Section",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Key",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    ObjectType {
        name: "Token",
        create: None,
        open: None,
        parse: None,
        close: None,
        delete: None,
        validate_access: None,
    },
    // Added after the original fifteen so their indices stay stable: a crash dump from an
    // older build still names the same types.
    ObjectType {
        name: crate::hal::input::executive::TYPE_NAME,
        create: Some(crate::hal::input::executive::create),
        open: Some(crate::hal::input::executive::open),
        parse: None,
        close: Some(crate::hal::input::executive::close),
        delete: Some(crate::hal::input::executive::delete),
        validate_access: Some(crate::hal::input::executive::validate_access),
    },
];

/// Register the built-in object types.
pub fn init(console: &mut dyn HalConsole) {
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
            .find_map(|(i, slot)| {
                slot.map(|ty| (i as u16, ty))
                    .filter(|(_, ty)| ty.name == name)
            })
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
        debug_assert!(
            previous > 0,
            "released an object more times than it was retained"
        );
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
