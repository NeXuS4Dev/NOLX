//! Host-testable Executive core for process, thread, dispatcher, and scheduling state.
//!
//! The kernel supplies object headers, page tables, interrupt entry, and saved stacks. This
//! crate supplies the bounded data structures and transition rules that those hardware-facing
//! components consume, so failure cases are ordinary host tests instead of boot-time corruption.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use nolx_obj_core::handle::HandleTable;
use nolx_obj_core::security::Token;

/// Bounded validation of x86-64 ELF executable load plans.
pub mod elf;
/// Bounded deferred procedure-call and system-worker queues.
pub mod deferred;
/// Waitable dispatcher objects and the common multi-object wait operation.
pub mod dispatcher;
/// Integrated scheduler/dispatcher wait and wake transitions.
pub mod executive;
/// Fair queued locks and reader/writer push-lock state.
pub mod lock;
/// Per-processor priority run queues and scheduler policy.
pub mod scheduler;
/// Thread CPU context, stack descriptors, TEB metadata, and APC queues.
pub mod thread;

/// The numeric identity of an Executive process.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ProcessId(u64);

impl ProcessId {
    /// Construct an identity from the process manager's allocated value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric value used by diagnostics and POSIX PID projection.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The numeric identity of an Executive thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ThreadId(u64);

impl ThreadId {
    /// Construct an identity from the thread manager's allocated value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric value used by diagnostics and object handles.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// The numeric identity of an Executive job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct JobId(u64);

impl JobId {
    /// Construct an identity from the job manager's allocated value.
    #[must_use]
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// The numeric value used by diagnostics and scheduler membership.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }
}

/// Why a fixed-size process parameter string could not be stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessInfoError {
    /// The string exceeds its fixed storage capacity.
    TooLong,
    /// The value contains an embedded NUL byte.
    EmbeddedNul,
}

/// An inline UTF-8 string used by the PEB-equivalent process metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineString<const CAPACITY: usize> {
    bytes: [u8; CAPACITY],
    length: usize,
}

impl<const CAPACITY: usize> InlineString<CAPACITY> {
    /// Construct an empty fixed-capacity string.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; CAPACITY],
            length: 0,
        }
    }

    /// Replace the contents, rejecting truncation and embedded NUL characters.
    pub fn set(&mut self, value: &str) -> Result<(), ProcessInfoError> {
        if value.len() > CAPACITY {
            return Err(ProcessInfoError::TooLong);
        }
        if value.as_bytes().contains(&0) {
            return Err(ProcessInfoError::EmbeddedNul);
        }
        self.bytes[..value.len()].copy_from_slice(value.as_bytes());
        self.bytes[value.len()..].fill(0);
        self.length = value.len();
        Ok(())
    }

    /// The string contents as UTF-8.
    #[must_use]
    pub fn as_str(&self) -> &str {
        // Only `&str` input is copied into this array, so the live prefix is valid UTF-8.
        core::str::from_utf8(&self.bytes[..self.length]).unwrap_or("")
    }

    /// Maximum number of UTF-8 bytes the string can hold.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        CAPACITY
    }
}

impl<const CAPACITY: usize> Default for InlineString<CAPACITY> {
    fn default() -> Self {
        Self::new()
    }
}

/// Process parameters which become the user-visible PEB-equivalent when M6 maps user memory.
///
/// The metadata stays kernel-owned in M5; no user pointer is dereferenced until the user-mode
/// address-space and syscall boundary are introduced. Strings are bounded and never truncated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessEnvironmentBlock {
    /// Process identity projected into the process information page.
    pub process_id: ProcessId,
    /// Parent identity, if this process has a parent.
    pub parent_process_id: Option<ProcessId>,
    /// Image base address, zero until an executable is loaded.
    pub image_base: u64,
    /// Entry point, zero until an executable is loaded.
    pub image_entry: u64,
    /// Address of the initial environment vector, zero until user memory is populated.
    pub environment_address: u64,
    /// Process command line, held inline before a user mapping exists.
    pub command_line: InlineString<256>,
    /// Current directory used to seed the POSIX process state.
    pub current_directory: InlineString<128>,
}

impl ProcessEnvironmentBlock {
    fn new(process_id: ProcessId, parent_process_id: Option<ProcessId>) -> Self {
        let mut current_directory = InlineString::new();
        let _ = current_directory.set("/");
        Self {
            process_id,
            parent_process_id,
            image_base: 0,
            image_entry: 0,
            environment_address: 0,
            command_line: InlineString::new(),
            current_directory,
        }
    }
}

/// The process-owned part of an Executive process object.
///
/// A process owns its identity and parentage, one address-space association, security token,
/// job membership, PEB-equivalent process parameters, and one independent handle table.
/// `AddressSpace` is generic because the kernel owns page tables while host tests use small
/// stand-ins. `Object` is the payload stored in process-local handles.
#[derive(Debug)]
pub struct ProcessContext<AddressSpace, Object, const HANDLE_CAPACITY: usize> {
    id: ProcessId,
    parent: Option<ProcessId>,
    address_space: AddressSpace,
    token: Token,
    job: Option<JobId>,
    peb: ProcessEnvironmentBlock,
    handles: HandleTable<Object, HANDLE_CAPACITY>,
}

impl<AddressSpace, Object, const HANDLE_CAPACITY: usize>
    ProcessContext<AddressSpace, Object, HANDLE_CAPACITY>
{
    /// Start a process context with an empty handle table and initialized process metadata.
    ///
    /// The caller is responsible for supplying a unique process ID and a fully initialized
    /// address-space value. The handle table is private to this context and starts empty.
    #[must_use]
    pub fn new(
        id: ProcessId,
        parent: Option<ProcessId>,
        address_space: AddressSpace,
        token: Token,
    ) -> Self {
        Self {
            id,
            parent,
            address_space,
            token,
            job: None,
            peb: ProcessEnvironmentBlock::new(id, parent),
            handles: HandleTable::new(),
        }
    }

    /// This process's identity.
    #[must_use]
    pub const fn id(&self) -> ProcessId {
        self.id
    }

    /// The parent process, if this process was created by another process.
    #[must_use]
    pub const fn parent(&self) -> Option<ProcessId> {
        self.parent
    }

    /// Borrow the process's address-space owner.
    #[must_use]
    pub const fn address_space(&self) -> &AddressSpace {
        &self.address_space
    }

    /// Mutably borrow the process's address-space owner.
    #[must_use]
    pub fn address_space_mut(&mut self) -> &mut AddressSpace {
        &mut self.address_space
    }

    /// The token all object opens from this process must use.
    #[must_use]
    pub const fn token(&self) -> Token {
        self.token
    }

    /// The process's current job membership.
    #[must_use]
    pub const fn job(&self) -> Option<JobId> {
        self.job
    }

    /// Assign the process to a job object.
    pub fn set_job(&mut self, job: Option<JobId>) {
        self.job = job;
    }

    /// Borrow the process's PEB-equivalent metadata.
    #[must_use]
    pub const fn peb(&self) -> &ProcessEnvironmentBlock {
        &self.peb
    }

    /// Mutably borrow process parameters before constructing the user-visible PEB page.
    #[must_use]
    pub fn peb_mut(&mut self) -> &mut ProcessEnvironmentBlock {
        &mut self.peb
    }

    /// The process's handle table.
    #[must_use]
    pub const fn handles(&self) -> &HandleTable<Object, HANDLE_CAPACITY> {
        &self.handles
    }

    /// Mutably borrow the process's handle table for Object Manager operations.
    #[must_use]
    pub fn handles_mut(&mut self) -> &mut HandleTable<Object, HANDLE_CAPACITY> {
        &mut self.handles
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nolx_obj_core::handle::{HandleError, ACCESS_ALL};
    use nolx_obj_core::security::{IntegrityLevel, Sid};

    fn token(uid: u32) -> Token {
        Token::new(Sid::user(uid), IntegrityLevel::Medium)
    }

    #[test]
    fn process_context_keeps_identity_parent_address_space_token_and_peb() {
        let parent = ProcessId::new(10);
        let mut process = ProcessContext::<u32, u16, 4>::new(
            ProcessId::new(11),
            Some(parent),
            0x1234,
            token(1000),
        );

        assert_eq!(process.id(), ProcessId::new(11));
        assert_eq!(process.parent(), Some(parent));
        assert_eq!(*process.address_space(), 0x1234);
        assert_eq!(process.token().user(), Sid::user(1000));
        assert_eq!(process.peb().process_id, ProcessId::new(11));
        assert_eq!(process.peb().parent_process_id, Some(parent));
        assert_eq!(process.peb().current_directory.as_str(), "/");
        assert!(process.handles().is_empty());
        assert_eq!(process.handles().capacity(), 4);

        process
            .peb_mut()
            .command_line
            .set("/bin/nolxsh -l")
            .unwrap();
        process.peb_mut().current_directory.set("/tmp").unwrap();
        process.peb_mut().image_base = 0x400000;
        assert_eq!(process.peb().command_line.as_str(), "/bin/nolxsh -l");
        assert_eq!(process.peb().current_directory.as_str(), "/tmp");
        assert_eq!(process.peb().image_base, 0x400000);
        process.set_job(Some(JobId::new(7)));
        assert_eq!(process.job(), Some(JobId::new(7)));
        process.set_job(None);
        assert_eq!(process.job(), None);
    }

    #[test]
    fn fixed_process_parameter_strings_reject_truncation_and_nul() {
        let mut string = InlineString::<3>::new();
        string.set("abc").unwrap();
        assert_eq!(string.as_str(), "abc");
        assert_eq!(string.set("long"), Err(ProcessInfoError::TooLong));
        assert_eq!(string.as_str(), "abc");
        assert_eq!(string.set("a\0b"), Err(ProcessInfoError::EmbeddedNul));
        assert_eq!(string.capacity(), 3);
    }

    #[test]
    fn every_process_has_an_independent_handle_table() {
        let mut first =
            ProcessContext::<u32, u16, 2>::new(ProcessId::new(1), None, 0x1000, token(1000));
        let mut second = ProcessContext::<u32, u16, 2>::new(
            ProcessId::new(2),
            Some(ProcessId::new(1)),
            0x2000,
            token(1001),
        );

        let first_handle = first.handles_mut().insert(7, ACCESS_ALL, false).unwrap();
        let second_handle = second.handles_mut().insert(9, ACCESS_ALL, false).unwrap();

        // A raw handle is interpreted in the owning process's table, not globally.
        assert_eq!(first_handle, second_handle);
        assert_eq!(first.handles().get(first_handle).unwrap().object, 7);
        assert_eq!(second.handles().get(second_handle).unwrap().object, 9);
        assert_eq!(first.handles().len(), 1);
        assert_eq!(second.handles().len(), 1);

        let unrelated =
            ProcessContext::<u32, u16, 2>::new(ProcessId::new(3), None, 0x3000, token(1002));
        assert_eq!(
            unrelated.handles().get(first_handle),
            Err(HandleError::Invalid)
        );
    }
}
