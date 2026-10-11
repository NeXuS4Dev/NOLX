//! Host-testable Executive process state.
//!
//! A process is the owner of three resources that must not drift apart: its identity, the
//! address space it runs in, and the handle table/security token used to access executive
//! objects. The address-space type is generic because the kernel owns its page tables while
//! host tests use small stand-ins. In both cases, the process context owns the supplied value
//! (or an exclusive reference to stable kernel storage) and one independent handle table.
//!
//! This crate deliberately stops before scheduling. Threads, dispatcher queues, and process
//! creation policy are layered on top of this context in M5.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

use nolx_obj_core::handle::HandleTable;
use nolx_obj_core::security::Token;

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

/// The process-owned part of an Executive process object.
///
/// `AddressSpace` is normally a stable mutable reference to a large kernel address-space
/// object. Keeping the reference here gives the process exclusive ownership without moving
/// the page-table/VAD structure. `Object` is the payload a handle refers to, usually an
/// Object Manager object identifier.
#[derive(Debug)]
pub struct ProcessContext<AddressSpace, Object, const HANDLE_CAPACITY: usize> {
    id: ProcessId,
    parent: Option<ProcessId>,
    address_space: AddressSpace,
    token: Token,
    handles: HandleTable<Object, HANDLE_CAPACITY>,
}

impl<AddressSpace, Object, const HANDLE_CAPACITY: usize>
    ProcessContext<AddressSpace, Object, HANDLE_CAPACITY>
{
    /// Start a process context with an empty handle table.
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
    fn process_context_keeps_identity_address_space_parent_and_token() {
        let parent = ProcessId::new(10);
        let process = ProcessContext::<u32, u16, 4>::new(
            ProcessId::new(11),
            Some(parent),
            0x1234,
            token(1000),
        );

        assert_eq!(process.id(), ProcessId::new(11));
        assert_eq!(process.parent(), Some(parent));
        assert_eq!(*process.address_space(), 0x1234);
        assert_eq!(process.token().user(), Sid::user(1000));
        assert!(process.handles().is_empty());
        assert_eq!(process.handles().capacity(), 4);
    }

    #[test]
    fn every_process_has_an_independent_handle_table() {
        let mut first = ProcessContext::<u32, u16, 2>::new(
            ProcessId::new(1),
            None,
            0x1000,
            token(1000),
        );
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

        let unrelated = ProcessContext::<u32, u16, 2>::new(
            ProcessId::new(3),
            None,
            0x3000,
            token(1002),
        );
        assert_eq!(
            unrelated.handles().get(first_handle),
            Err(HandleError::Invalid)
        );
    }
}
