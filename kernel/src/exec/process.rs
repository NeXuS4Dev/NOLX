//! Process objects and their process-owned resources.
//!
//! The process context keeps the address space, Object Manager handle table, and security
//! token together. An address space is large, so the process owns an exclusive reference to
//! stable storage rather than moving the page-table/VAD structure into a temporary. The
//! caller must keep that storage alive for the process's lifetime.

use core::mem::size_of;

use nolx_exec_core::{ProcessContext, ProcessId};
use nolx_obj_core::handle::{Handle, HandleError};
use nolx_obj_core::security::Token;

use crate::mm::space::{Space, SpaceError};
use crate::obj::boot::{self, ObjError, ObjectId};
use crate::obj::{type_by_name, ObjectHeader};

/// The fixed number of handle slots owned by each process for this M5 slice.
pub const PROCESS_HANDLE_TABLE_ENTRIES: usize = 64;

/// A process object.
///
/// The address-space storage is separately allocated or static, then lent exclusively to this
/// object. This preserves the address space's stable address; moving its large inline VAD arena
/// through a kernel stack frame previously caused a boot-stack overflow.
#[repr(C)]
pub struct Process<'space> {
    /// The Object Manager header. The body follows it in memory.
    pub header: ObjectHeader,
    context: ProcessContext<&'space mut Space, ObjectId, PROCESS_HANDLE_TABLE_ENTRIES>,
    /// POSIX process group id, set by `setpgid`.
    pub pgid: i32,
    /// POSIX session id, set by `setsid`.
    pub sid: i32,
    /// Exit status once terminated, as `waitid` reports it.
    pub exit_status: i32,
    /// `true` once the process has exited but has not been reaped.
    pub zombie: bool,
}

impl<'space> Process<'space> {
    /// Construct a process that owns `address_space`, an empty handle table, and `token`.
    ///
    /// The built-in `Process` object type must have been registered by `obj::init` first.
    /// `address_space` must point to stable storage and is held as an exclusive borrow for the
    /// process lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ProcessCreateError::TypeNotRegistered`] before Object Manager initialization,
    /// or [`ProcessCreateError::BodyTooLarge`] if the process body cannot fit the header's size
    /// field.
    pub fn new(
        id: ProcessId,
        parent: Option<ProcessId>,
        address_space: &'space mut Space,
        token: Token,
    ) -> Result<Self, ProcessCreateError> {
        let (type_index, _) =
            type_by_name("Process").ok_or(ProcessCreateError::TypeNotRegistered)?;
        let body_size = u32::try_from(size_of::<Self>() - size_of::<ObjectHeader>())
            .map_err(|_| ProcessCreateError::BodyTooLarge)?;

        Ok(Self {
            header: ObjectHeader::new(type_index, body_size),
            context: ProcessContext::new(id, parent, address_space, token),
            pgid: 0,
            sid: 0,
            exit_status: 0,
            zombie: false,
        })
    }

    /// This process's identity.
    #[must_use]
    pub const fn id(&self) -> ProcessId {
        self.context.id()
    }

    /// The parent process, if any.
    #[must_use]
    pub const fn parent_id(&self) -> Option<ProcessId> {
        self.context.parent()
    }

    /// The security token all object opens from this process use.
    #[must_use]
    pub const fn token(&self) -> Token {
        self.context.token()
    }

    /// The root page-table frame of this process's address space.
    #[must_use]
    pub fn address_space_root(&self) -> u64 {
        (**self.context.address_space()).root()
    }

    /// Number of live handles owned by this process.
    #[must_use]
    pub const fn handle_count(&self) -> usize {
        self.context.handles().len()
    }

    /// The object id held by `handle`, if it is live in this process's table.
    ///
    /// This is an inspection operation for the kernel and tests; callers do not receive an
    /// object pointer or bypass the handle's granted access mask.
    pub fn object_for_handle(&self, handle: Handle) -> Result<ObjectId, HandleError> {
        self.context.handles().get(handle).map(|entry| entry.object)
    }

    /// Open `path` using this process's token and handle table.
    pub fn open_handle(&mut self, path: &str, desired: u32) -> Result<Handle, ObjError> {
        let token = self.context.token();
        boot::open(path, &token, self.context.handles_mut(), desired)
    }

    /// Close `handle` in this process, dropping its object references.
    pub fn close_handle(&mut self, handle: Handle) -> Result<(), ObjError> {
        boot::close(self.context.handles_mut(), handle)
    }

    /// Duplicate a handle from one process into another, narrowing access if requested.
    pub fn duplicate_handle_into(
        source: &Self,
        handle: Handle,
        target: &mut Self,
        access: u32,
    ) -> Result<Handle, ObjError> {
        boot::duplicate(
            source.context.handles(),
            handle,
            target.context.handles_mut(),
            access,
        )
    }

    /// Close all remaining handles, as process teardown must do before releasing its body.
    ///
    /// Returns the number of handles closed. The list is captured first so the handle table
    /// is not mutably borrowed while the Object Manager closes entries one by one.
    pub fn close_all_handles(&mut self) -> Result<usize, ObjError> {
        let mut handles = [None; PROCESS_HANDLE_TABLE_ENTRIES];
        for (index, (handle, _)) in self.context.handles().iter().enumerate() {
            handles[index] = Some(handle);
        }

        let mut closed = 0;
        for handle in handles.into_iter().flatten() {
            self.close_handle(handle)?;
            closed += 1;
        }
        Ok(closed)
    }

    /// Exit this process: close every handle and tear down its address space.
    ///
    /// The body remains a zombie for its parent to reap; its Object Manager header is not
    /// released here. Reaping and parent wait queues arrive with the process manager.
    pub fn terminate(&mut self, exit_status: i32) -> Result<(), ProcessExitError> {
        if self.zombie {
            return Err(ProcessExitError::AlreadyExited);
        }
        self.close_all_handles()
            .map_err(ProcessExitError::ObjectManager)?;
        self.with_address_space(|space| space.teardown())
            .map_err(ProcessExitError::AddressSpace)?;
        self.exit_status = exit_status;
        self.zombie = true;
        Ok(())
    }

    fn with_address_space<R>(&mut self, f: impl FnOnce(&mut Space) -> R) -> R {
        f(self.context.address_space_mut())
    }
}

/// Why a process body could not be constructed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCreateError {
    /// The Object Manager has not registered its built-in `Process` type.
    TypeNotRegistered,
    /// The body size does not fit in [`ObjectHeader::body_size`].
    BodyTooLarge,
}

/// Why process termination could not finish.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessExitError {
    /// The process has already exited.
    AlreadyExited,
    /// Closing a process-owned object handle failed.
    ObjectManager(ObjError),
    /// The process address space could not be torn down.
    AddressSpace(SpaceError),
}
