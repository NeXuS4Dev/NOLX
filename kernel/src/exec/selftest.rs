//! The Executive's first process self test.
//!
//! Build two process contexts over two stable address spaces, then prove that identity,
//! access token, and handle table belong to the process rather than global state. Teardown
//! must close each process's handles and give both address spaces back.

use core::fmt;
use core::mem::MaybeUninit;

use nolx_exec_core::thread::StackDescriptor;
use nolx_hal_core::console::HalConsole;
use nolx_obj_core::handle::HandleError;
use nolx_obj_core::security::{
    rights, Ace, AceKind, Acl, IntegrityLevel, SecurityDescriptor, Sid, Token,
};

use crate::mm::space::{self, Space};
use crate::obj::boot::{self, ObjError};
use crate::sync::SyncCell;

use super::process::{Process, ProcessCreateError, ProcessExitError};
use super::thread::{Thread, ThreadCreateError};
use super::{JobId, Policy, ProcessId, SchedState, ThreadId};

static PROCESS_SPACE_A: SyncCell<MaybeUninit<Space>> = SyncCell::new(MaybeUninit::uninit());
static PROCESS_SPACE_B: SyncCell<MaybeUninit<Space>> = SyncCell::new(MaybeUninit::uninit());

#[derive(Debug)]
pub(super) enum SelftestError {
    /// The frame allocator could not create a test address space.
    AddressSpaceAllocation,
    /// A process body could not be initialized.
    ProcessCreate(ProcessCreateError),
    /// A thread body, stack, or scheduling state could not be initialized.
    ThreadCreate(ThreadCreateError),
    /// A process could not terminate cleanly.
    ProcessExit(ProcessExitError),
    /// An Object Manager operation failed.
    ObjectManager(ObjError),
    /// The test ACL exceeded its fixed capacity.
    SecurityAclFull,
    /// A process-context invariant did not hold.
    Check(&'static str),
}

impl fmt::Display for SelftestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AddressSpaceAllocation => formatter.write_str("address-space allocation failed"),
            Self::ProcessCreate(error) => write!(formatter, "process creation failed: {error:?}"),
            Self::ThreadCreate(error) => write!(formatter, "thread creation failed: {error:?}"),
            Self::ProcessExit(error) => write!(formatter, "process exit failed: {error:?}"),
            Self::ObjectManager(error) => {
                write!(formatter, "Object Manager operation failed: {error:?}")
            }
            Self::SecurityAclFull => formatter.write_str("the self-test ACL is full"),
            Self::Check(message) => write!(formatter, "invariant check failed: {message}"),
        }
    }
}

fn check(condition: bool, message: &'static str) -> Result<(), SelftestError> {
    if condition {
        Ok(())
    } else {
        Err(SelftestError::Check(message))
    }
}

fn create_space(storage: &mut MaybeUninit<Space>) -> Result<&mut Space, SelftestError> {
    space::AddressSpace::create_in_place(storage).ok_or(SelftestError::AddressSpaceAllocation)
}

/// Exercise the first M5 process-owned resource slice.
pub(super) fn run(console: &mut dyn HalConsole) -> Result<(), SelftestError> {
    let live_before = boot::live_objects();
    // SAFETY: the Executive self test runs once on the boot processor with interrupts
    // disabled. Slot A is private to this function and is initialized exactly once.
    let place_a = unsafe { &mut *PROCESS_SPACE_A.get() };
    let address_space_a = create_space(place_a)?;
    // SAFETY: as for slot A; slot B is distinct static storage, initialized once here.
    let place_b = unsafe { &mut *PROCESS_SPACE_B.get() };
    let address_space_b = create_space(place_b)?;
    let user_a = Token::new(Sid::user(1000), IntegrityLevel::Medium);
    let user_b = Token::new(Sid::user(1001), IntegrityLevel::Medium);
    let id_a = ProcessId::new(100);
    let id_b = ProcessId::new(101);

    let mut process_a =
        Process::new(id_a, None, address_space_a, user_a).map_err(SelftestError::ProcessCreate)?;
    let mut process_b = Process::new(id_b, Some(id_a), address_space_b, user_b)
        .map_err(SelftestError::ProcessCreate)?;

    check(process_a.id() == id_a, "process A lost its identity")?;
    check(
        process_b.parent_id() == Some(id_a),
        "process B lost its parent identity",
    )?;
    check(
        process_a.address_space_root() != process_b.address_space_root(),
        "two processes were given the same address space",
    )?;
    check(
        process_a.token().user() == Sid::user(1000) && process_b.token().user() == Sid::user(1001),
        "a process did not retain its own security token",
    )?;
    check(
        process_a.handle_count() == 0 && process_b.handle_count() == 0,
        "a new process did not start with an empty handle table",
    )?;
    let job = JobId::new(77);
    process_a.set_job(Some(job));
    process_a
        .set_command_line("/bin/nolxsh --login")
        .map_err(|_| SelftestError::Check("process command line exceeded the PEB storage"))?;
    process_a.peb_mut().image_base = 0x0040_0000;
    check(
        process_a.job() == Some(job)
            && process_a.peb().process_id == id_a
            && process_a.peb().parent_process_id.is_none()
            && process_a.peb().command_line.as_str() == "/bin/nolxsh --login"
            && process_a.peb().image_base == 0x0040_0000,
        "process job membership or PEB-equivalent metadata was not retained",
    )?;

    let stack = StackDescriptor::new(0x10_0000, 0x4000, Some(0x0F_F000))
        .map_err(|_| SelftestError::Check("the synthetic kernel stack descriptor was invalid"))?;
    let mut thread = Thread::new(
        ThreadId::new(200),
        id_a,
        0x0040_1000,
        0xCAFE,
        stack,
        SchedState::default(),
    )
    .map_err(SelftestError::ThreadCreate)?;
    check(
        thread.process == id_a
            && thread.context.cpu.instruction_pointer == 0x0040_1000
            && thread.context.cpu.stack_pointer == stack.top - 8
            && thread.context.cpu.general[0] == 0xCAFE
            && thread.context.kernel_stack == stack
            && thread.context.user_apcs.is_empty()
            && thread.context.kernel_apcs.is_empty(),
        "thread context did not retain its owning process, initial registers, stack, or APC lists",
    )?;
    thread
        .set_policy(Policy::RoundRobin, 16, 4)
        .map_err(SelftestError::ThreadCreate)?;
    check(
        thread.sched.policy == Policy::RoundRobin
            && thread.sched.base_priority == 16
            && thread.sched.quantum == 4,
        "thread scheduling policy was not applied",
    )?;
    thread.terminate(0).map_err(SelftestError::ThreadCreate)?;
    check(
        thread.terminated() && thread.exit_status == Some(0),
        "thread termination did not publish its waitable exit status",
    )?;
    check(
        thread.terminate(1) == Err(ThreadCreateError::AlreadyTerminated),
        "thread termination was not idempotence-checked",
    )?;
    check(
        thread.header.release(),
        "the self-test Thread object header did not release",
    )?;

    let mut acl = Acl::empty();
    acl.push(Ace {
        kind: AceKind::Deny,
        sid: Sid::user(1001),
        mask: rights::READ,
    })
    .map_err(|_| SelftestError::SecurityAclFull)?;
    acl.push(Ace {
        kind: AceKind::Allow,
        sid: Sid::user(1000),
        mask: rights::READ,
    })
    .map_err(|_| SelftestError::SecurityAclFull)?;
    let descriptor = SecurityDescriptor {
        owner: Sid::SYSTEM,
        dacl: Some(acl),
        label: None,
    };
    let object = boot::create_object("\\Device", "ProcessSelftest", "Device", descriptor)
        .map_err(SelftestError::ObjectManager)?;

    let handle_a = process_a
        .open_handle("\\Device\\ProcessSelftest", rights::READ)
        .map_err(SelftestError::ObjectManager)?;
    check(
        process_a.object_for_handle(handle_a) == Ok(object),
        "process A's handle did not name the opened object",
    )?;
    check(
        boot::reference_counts(object) == Ok((2, 1)),
        "process A's open did not add one pointer/handle reference",
    )?;
    let inherited_source = process_a
        .open_inheritable_handle("\\Device\\ProcessSelftest", rights::READ)
        .map_err(SelftestError::ObjectManager)?;
    check(
        process_a.object_for_handle(inherited_source) == Ok(object)
            && boot::reference_counts(object) == Ok((3, 2)),
        "an inheritable handle was not opened with its own object reference",
    )?;
    check(
        process_b.object_for_handle(handle_a) == Err(HandleError::Invalid),
        "process B resolved a handle that only exists in process A",
    )?;

    let handle_b =
        Process::duplicate_handle_into(&process_a, handle_a, &mut process_b, rights::READ)
            .map_err(SelftestError::ObjectManager)?;
    check(
        handle_a == handle_b,
        "process-local handle values were not independently reusable",
    )?;
    check(
        process_a.object_for_handle(handle_a) == Ok(object)
            && process_b.object_for_handle(handle_b) == Ok(object),
        "a duplicated handle did not resolve in its owning process",
    )?;
    check(
        boot::reference_counts(object) == Ok((4, 3)),
        "cross-process duplication did not add one pointer/handle reference",
    )?;
    let inherited = Process::inherit_handles_from(&process_a, &mut process_b)
        .map_err(SelftestError::ObjectManager)?;
    check(
        inherited == 1
            && process_b.object_for_handle(inherited_source) == Ok(object)
            && boot::reference_counts(object) == Ok((5, 4)),
        "child-process creation did not copy only inheritable handles with retained references",
    )?;

    let denied = process_b.open_handle("\\Device\\ProcessSelftest", rights::READ);
    check(
        matches!(denied, Err(ObjError::AccessDenied { .. })),
        "the process token did not control an object open",
    )?;
    check(
        process_a.handle_count() == 2 && process_b.handle_count() == 2,
        "a denied open changed a process handle table",
    )?;

    process_a.terminate(0).map_err(SelftestError::ProcessExit)?;
    process_b.terminate(1).map_err(SelftestError::ProcessExit)?;
    check(
        boot::reference_counts(object) == Ok((1, 0)),
        "process termination did not release every handle reference",
    )?;
    check(
        process_a.zombie
            && process_b.zombie
            && process_a.exit_status == 0
            && process_b.exit_status == 1,
        "process termination did not retain the exit statuses",
    )?;
    check(
        process_a.handle_count() == 0 && process_b.handle_count() == 0,
        "process termination left handles open",
    )?;
    check(
        process_a.address_space_root() == 0 && process_b.address_space_root() == 0,
        "process termination did not tear down both address spaces",
    )?;
    check(
        process_a.header.release() && process_b.header.release(),
        "a process object header was not released after its final reference",
    )?;

    boot::unbind("\\Device\\ProcessSelftest").map_err(SelftestError::ObjectManager)?;
    check(
        boot::live_objects() == live_before,
        "the process self test leaked an Object Manager object",
    )?;

    let _ = writeln!(
        console,
        "NOLX: process selftest: 2 processes, 2 address spaces, PEB/job metadata, thread context, \
         isolated/inherited handles, token-checked open/deny, teardown returned resources"
    );
    Ok(())
}
