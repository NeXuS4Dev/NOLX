//! Ring-3 processes: the table that owns them, the scheduler that runs them, and the
//! `syscall`/timer bridges that switch between their frames.
//!
//! The model is deliberately small and complete: one thread per process, a private address
//! space each, and a fixed table. POSIX *semantics* (what `waitid` means, what `write`
//! accepts) live in [`crate::psx`]; this module is the mechanism those handlers drive:
//! process identity, frame ownership, address-space activation, and the decisions the
//! shared `syscall` assembly stub needs -- return to the caller through `sysret`, or
//! resume a different thread's frame.
//!
//! # The frame contract
//!
//! Every user thread's saved context is an [`InterruptFrame`] living on its kernel stack
//! (the syscall/IRQ entry stubs build it there), and the idle thread's is saved the same
//! way when a tick preempts the kernel loop. [`on_syscall`] and [`on_timer`] are both of
//! the shape "the interrupted thread keeps its own stack; return the frame of the thread
//! to run next" -- the contract M5's round-robin switch established.

pub mod loader;
pub mod selftest;

use core::mem::MaybeUninit;

use nolx_exec_core::scheduler::{Policy, Scheduler, SchedulerError, ThreadState};
use nolx_exec_core::ThreadId;
use nolx_hal_core::interrupt::InterruptFrame;

use crate::hal::context::initialize_user_stack;
use crate::mm::space::{self, Space};
use crate::sync::{IrqMutex, SyncCell};

/// The fixed number of user processes the table holds.
pub const MAX_USER_PROCESSES: usize = 4;

/// Kernel stack size per user thread: the entry stubs build a full frame here, and the
/// loader runs on it with its load plan -- 32 KiB so a spawn can never borrow against the
/// statics that sit below the stack.
pub const KERNEL_STACK_SIZE: usize = 32 * 1024;

/// User stack size, committed eagerly for the initial image and growable below.
pub const USER_STACK_SIZE: u64 = 64 * 1024;

/// The user stack's top: the canonical hole's edge, the highest user address.
pub const STACK_TOP: u64 = 0x0000_8000_0000_0000;

/// The floor the grow-down stack may not cross (five more stack sizes below the reserve).
pub const STACK_FLOOR: u64 = STACK_TOP - 5 * USER_STACK_SIZE;

/// Scheduler slot of the idle thread the kernel loop runs as.
pub const IDLE_THREAD: ThreadId = ThreadId::new(1);

/// First scheduler slot a user process occupies.
pub const FIRST_USER_THREAD: u64 = 2;

/// A ring-3 process slot.
#[derive(Clone, Copy)]
pub struct UserProcess {
    /// Process id: `1` is `init`.
    pub pid: u64,
    /// Parent process id, or zero for the kernel test driver.
    pub parent: u64,
    /// Whether the slot holds a process.
    pub live: bool,
    /// Whether the process exited and has not been observed yet.
    pub zombie: bool,
    /// Whether the thread suspended itself in a wait.
    pub blocked: bool,
    /// Exit status once [`UserProcess::zombie`].
    pub exit_status: i32,
    /// The scheduler thread running this process.
    pub thread_id: ThreadId,
    /// User address of the `waitid` result slot to fill at wake, or zero.
    pub wait_info: u64,
    /// The child being waited for, or zero for any.
    pub wait_pid: u64,
    /// Bytes this process pushed through `write`, for the boot test's evidence chain.
    pub written: u64,
    /// The kernel stack top published to `TSS.RSP0` while this thread runs.
    pub kernel_stack_top: u64,
}

impl UserProcess {
    const fn empty() -> Self {
        Self {
            pid: 0,
            parent: 0,
            live: false,
            zombie: false,
            blocked: false,
            exit_status: 0,
            thread_id: ThreadId::new(0),
            wait_info: 0,
            wait_pid: 0,
            written: 0,
            kernel_stack_top: 0,
        }
    }
}

/// The process table, its frame store, and its scheduler.
pub struct UserTable {
    processes: [UserProcess; MAX_USER_PROCESSES],
    frames: [usize; MAX_USER_PROCESSES],
    /// The idle thread's interrupted frame, saved whenever a tick preempts the kernel loop.
    idle_frame: usize,
    scheduler: Scheduler<{ MAX_USER_PROCESSES + 2 }, 1>,
    /// The running process's slot, or `None` while the kernel idle thread runs.
    pub current: Option<usize>,
    next_pid: u64,
}

impl UserTable {
    const fn new() -> Self {
        Self {
            processes: [UserProcess::empty(); MAX_USER_PROCESSES],
            frames: [0; MAX_USER_PROCESSES],
            idle_frame: 0,
            scheduler: Scheduler::new(),
            current: None,
            next_pid: 1,
        }
    }

    fn free_slot(&self) -> Option<usize> {
        (0..MAX_USER_PROCESSES).find(|&slot| !self.processes[slot].live)
    }

    fn slot_by_pid(&self, pid: u64) -> Option<usize> {
        // A reaped slot keeps its record until reused: the boot test reads the evidence
        // chain (statuses, bytes written) after `waitid` has already released the child.
        (0..MAX_USER_PROCESSES).find(|&slot| self.processes[slot].pid == pid)
    }

    fn slot_by_thread(&self, thread: ThreadId) -> Option<usize> {
        (0..MAX_USER_PROCESSES)
            .find(|&slot| self.processes[slot].live && self.processes[slot].thread_id == thread)
    }

    fn frame_pointer(&self, slot: usize) -> *mut InterruptFrame {
        self.frames[slot] as *mut InterruptFrame
    }

    fn idle_frame_pointer(&self) -> *mut InterruptFrame {
        self.idle_frame as *mut InterruptFrame
    }
}

static TABLE: IrqMutex<UserTable> = IrqMutex::new(UserTable::new());

/// Stable address-space storage for each process slot.
static SPACES: [SyncCell<MaybeUninit<Space>>; MAX_USER_PROCESSES] =
    [const { SyncCell::new(MaybeUninit::uninit()) }; MAX_USER_PROCESSES];

/// A ring-0 stack per process slot, holding the entry frame the stubs edit.
#[repr(C, align(16))]
pub struct KernelStack(pub [u8; KERNEL_STACK_SIZE]);

/// Stable kernel-stack storage for each process slot.
static KERNEL_STACKS: [SyncCell<KernelStack>; MAX_USER_PROCESSES] =
    [const { SyncCell::new(KernelStack([0; KERNEL_STACK_SIZE])) }; MAX_USER_PROCESSES];

/// The address space of `slot`, as a mutable borrow of stable storage.
///
/// # Safety
///
/// The slot must be live and initialised by [`spawn`], and no other caller may hold the
/// same slot's space.
pub unsafe fn space_of(slot: usize) -> &'static mut Space {
    // SAFETY: the caller guarantees the slot is live; `create_in_place` initialised the
    // storage before the slot was marked live, and the table serialises access on the
    // single boot processor.
    unsafe {
        let place = &mut *SPACES[slot].get();
        place.assume_init_mut()
    }
}

/// The kernel stack storage of `slot`.
///
/// # Safety
///
/// The slot must be live and no other caller may be building a frame on that stack.
pub unsafe fn kernel_stack_of(slot: usize) -> &'static mut KernelStack {
    // SAFETY: as `space_of`; the stack is owned by the slot's single thread.
    unsafe { &mut *KERNEL_STACKS[slot].get() }
}

/// The `syscall` entry stub's Rust side: run the call, then decide where to return.
///
/// Runs with maskable interrupts disabled (`SFMASK` cleared IF). The result goes into the
/// frame's `rax`; a thread that exited or blocked -- as the handlers recorded in the
/// process table -- yields to whichever thread the scheduler dispatches next, and the
/// next thread's frame pointer is what the stub restores.
///
/// # Safety
///
/// `frame` must be the frame the entry stub built on this thread's kernel stack.
pub(crate) unsafe fn on_syscall(frame: *mut InterruptFrame) -> *mut InterruptFrame {
    // SAFETY: the stub built the frame on the current thread's kernel stack and passed it
    // in; it stays alive because the stack does.
    let request = unsafe {
        let frame_ref = &*frame;
        crate::psx::SyscallArgs {
            a0: frame_ref.rdi,
            a1: frame_ref.rsi,
            a2: frame_ref.rdx,
            a3: frame_ref.r10,
            a4: frame_ref.r8,
            a5: frame_ref.r9,
        }
    };
    // SAFETY: as above; the same frame, still alive on this thread's kernel stack.
    let number = unsafe { (*frame).rax };
    dbg_line(format_args!(
        "NOLX: t2-dbg syscall rax={} a0={:#x} a1={:#x} frame={:?} top={:#x}",
        number,
        request.a0,
        request.a1,
        frame,
        crate::hal::cpu::kernel_stack_top()
    ));
    // The register is 64 bits and the table is 16: an unguarded cast would let a caller
    // reach any handler by adding a multiple of 2^16 to the number.
    let result = match u16::try_from(number) {
        Ok(number) => crate::psx::dispatch(number, request),
        Err(_) => -(nolx_types::errno::ENOSYS as i64),
    };
    // SAFETY: as above; the frame is exclusive to this thread between entry and restore.
    unsafe {
        (*frame).rax = result as u64;
    }

    let mut table = TABLE.lock();
    let Some(slot) = table.current else {
        // A syscall can only come from a user thread the table is running.
        return frame;
    };
    let suspended = table.processes[slot].zombie || table.processes[slot].blocked;
    if !suspended {
        return frame;
    }
    table.frames[slot] = frame as usize;
    let thread = table.processes[slot].thread_id;
    let _ = table.scheduler.block(thread);
    let next = table.scheduler.current(0);
    match next {
        Some(next) => resume_thread(&mut table, Some(next)),
        None => resume_thread(&mut table, None),
    }
}

/// The timer tick's user-thread half: rotate to the next user thread's frame.
///
/// # Safety
///
/// `frame` must be the frame the interrupt stub built on this thread's kernel stack.
pub(crate) unsafe fn on_timer(frame: *mut InterruptFrame) -> *mut InterruptFrame {
    let mut table = TABLE.lock();
    let interrupted = table.current;
    match interrupted {
        Some(slot) => table.frames[slot] = frame as usize,
        None => table.idle_frame = frame as usize,
    }
    let Some(previous) = (match interrupted {
        Some(slot) => Some(table.processes[slot].thread_id),
        None => Some(IDLE_THREAD),
    }) else {
        return frame;
    };
    if table.scheduler.on_tick(0).is_err() {
        return frame;
    }
    let next = table.scheduler.current(0);
    match next {
        Some(next) if next == previous => frame,
        next => resume_thread(&mut table, next),
    }
}

/// Make `next` the running thread (or the idle loop): activate its space, publish its
/// kernel stack top, and return the frame it resumes on. The resume target may be at a
/// different privilege level than the thread that was interrupted; the shared restore
/// path handles the `swapgs` decision from the frame itself.
fn resume_thread(table: &mut UserTable, next: Option<ThreadId>) -> *mut InterruptFrame {
    let next_slot = next.and_then(|thread| table.slot_by_thread(thread));
    let previous = table.current;
    if previous != next_slot {
        if let Some(old) = previous {
            // SAFETY: the old slot is live and its space is the active one.
            unsafe { space_of(old).deactivate() };
        }
        if let Some(slot) = next_slot {
            // SAFETY: the new slot is live and no space is active after the deactivation.
            unsafe { space_of(slot).activate() };
            let top = table.processes[slot].kernel_stack_top;
            crate::hal::cpu::set_kernel_stack_top(top);
            crate::exec::user::dbg_line(format_args!(
                "NOLX: t2-dbg switch prev={:?} next={:?} top={:#x} frame={:?}",
                previous,
                next_slot,
                top,
                table.frame_pointer(slot)
            ));
        }
    }
    table.current = next_slot;
    match next_slot {
        Some(slot) => table.frame_pointer(slot),
        None => table.idle_frame_pointer(),
    }
}

/// One debug line on the serial console (temporary T2 forensics).
pub fn dbg_line(args: core::fmt::Arguments<'_>) {
    struct W;
    impl core::fmt::Write for W {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for byte in s.bytes() {
                crate::hal::console::raw_put_byte(byte);
            }
            Ok(())
        }
    }
    let _ = core::fmt::write(&mut W, args);
    for byte in b"\n".iter().copied() {
        crate::hal::console::raw_put_byte(byte);
    }
}

/// The calling process's id.
#[must_use]
pub fn current_pid() -> u64 {
    let table = TABLE.lock();
    table.current.map_or(0, |slot| table.processes[slot].pid)
}

/// The evidence chain of one process: (zombie, exit status, bytes written).
#[must_use]
pub fn process_status(pid: u64) -> Option<(bool, i32, u64)> {
    let table = TABLE.lock();
    table.slot_by_pid(pid).map(|slot| {
        (
            table.processes[slot].zombie,
            table.processes[slot].exit_status,
            table.processes[slot].written,
        )
    })
}

/// Terminate the calling process: POSIX `exit`, minus the library's flushing.
///
/// The frame switch itself happens in [`on_syscall`], which sees the zombie and parks the
/// thread; what this function does is the bookkeeping a waiter observes: the status is
/// kept, and a parent blocked in `waitid` is woken with its result already written into
/// its own frame and address space.
pub fn exit_current(status: i32) {
    let mut table = TABLE.lock();
    let Some(slot) = table.current else {
        return;
    };
    table.processes[slot].zombie = true;
    table.processes[slot].exit_status = status;
    let pid = table.processes[slot].pid;
    dbg_line(format_args!(
        "NOLX: t2-dbg exit pid={} slot={} status={}",
        pid, slot, status
    ));

    let parent = table.processes[slot].parent;
    let Some(parent_slot) = (if parent == 0 {
        None
    } else {
        table.slot_by_pid(parent)
    }) else {
        return;
    };
    if table.processes[parent_slot].zombie || !table.processes[parent_slot].blocked {
        return;
    }
    let parent_wait = table.processes[parent_slot].wait_pid;
    if parent_wait != 0 && parent_wait != pid {
        return;
    }

    // The siginfo goes into the *parent's* address space through the direct map: the
    // current space is the dying child's, and the parent is not running.
    // SAFETY: the parent slot is live; the loader's helpers own writes into a space that
    // is not the active one.
    let info_user = table.processes[parent_slot].wait_info;
    let filled = unsafe { loader::fill_siginfo(space_of(parent_slot), info_user, pid, status) };
    if filled.is_err() {
        return;
    }
    // SAFETY: the parent's saved frame is live on its kernel stack.
    unsafe {
        let parent_frame = &mut *(table.frames[parent_slot] as *mut InterruptFrame);
        parent_frame.rax = 0;
    }
    table.processes[parent_slot].blocked = false;
    table.processes[parent_slot].wait_info = 0;
    table.processes[parent_slot].wait_pid = 0;
    let parent_thread = table.processes[parent_slot].thread_id;
    let _ = table.scheduler.wake(parent_thread);
}

/// The frame switch and space activation result, for the setup path to report.
#[derive(Debug, PartialEq, Eq)]
pub enum SetupError {
    /// No free process slot.
    TableFull,
    /// The address space could not be created in its stable storage.
    AddressSpace,
    /// The scheduler refused the thread.
    Scheduler(SchedulerError),
    /// The loader refused the image.
    Load(loader::LoadError),
}

/// Create a process from an embedded image, with `parent` and a path for `AT_EXECFN`.
pub fn spawn(parent: u64, path: &str, image: &[u8], argv: &[&str]) -> Result<u64, SetupError> {
    spawn_impl(&mut TABLE.lock(), parent, path, image, argv)
}

/// Prepare the scheduler for the user test: install the idle thread.
pub fn begin() -> Result<(), SetupError> {
    let mut table = TABLE.lock();
    table
        .scheduler
        .add_idle_thread(IDLE_THREAD, 0)
        .map_err(SetupError::Scheduler)?;
    Ok(())
}

/// The `syscall` stub's C entry point.
///
/// # Safety
///
/// `frame` must be the frame the entry stub built on this thread's kernel stack; the
/// assembly stub is the only caller.
#[no_mangle]
pub unsafe extern "C" fn nolx_syscall_dispatch(frame: *mut InterruptFrame) -> *mut InterruptFrame {
    // SAFETY: the `syscall` entry stub built `frame` on this thread's kernel stack and the
    // stack outlives every pointer derived from it.
    unsafe { on_syscall(frame) }
}

fn load_error_code(error: loader::LoadError) -> i32 {
    use loader::LoadError;
    match error {
        LoadError::UserPointer => nolx_types::errno::EFAULT,
        LoadError::Arguments => nolx_types::errno::EINVAL,
        LoadError::Reserve | LoadError::Page | LoadError::Elf(_) | LoadError::Image(_) => {
            nolx_types::errno::ENOMEM
        }
    }
}

/// The `write` mechanism: bytes from the calling process reach the console.
pub fn write_console(user_buffer: u64, length: u64) -> Result<u64, i32> {
    if length == 0 {
        return Ok(0);
    }
    let slot = TABLE.lock().current.ok_or(nolx_types::errno::ESRCH)?;
    // SAFETY: the calling thread's slot is live and its space is the active one for the
    // whole syscall (maskable interrupts are off and this path never blocks).
    let space = unsafe { space_of(slot) };
    let mut done = 0u64;
    let mut chunk = [0u8; 256];
    while done < length {
        let take = (length - done).min(chunk.len() as u64) as usize;
        loader::read_user(space, user_buffer + done, &mut chunk[..take])
            .map_err(load_error_code)?;
        for byte in &chunk[..take] {
            crate::hal::console::raw_put_byte(*byte);
        }
        done += take as u64;
    }
    let mut table = TABLE.lock();
    table.processes[slot].written = table.processes[slot].written.saturating_add(done);
    Ok(done)
}

/// The `waitid` mechanism: reap a zombie child, or suspend until the child exits.
pub fn waitid(child_pid: u64, info_user: u64) -> i64 {
    let mut table = TABLE.lock();
    let Some(slot) = table.current else {
        return -i64::from(nolx_types::errno::ESRCH);
    };
    let Some(child_slot) = table.slot_by_pid(child_pid) else {
        return -i64::from(nolx_types::errno::ECHILD);
    };
    if table.processes[child_slot].parent != table.processes[slot].pid {
        return -i64::from(nolx_types::errno::ECHILD);
    }
    if !table.processes[child_slot].live {
        // Already reaped: the second `waitid` must not observe it twice.
        return -i64::from(nolx_types::errno::ECHILD);
    }
    // Validate the result slot now, at call time: the wake path writes into a space that
    // is not running, and a bad pointer there would leave this thread blocked forever.
    // SAFETY: the calling thread's slot is live and its space is active.
    let validated = unsafe {
        loader::validate_user_range(
            space_of(slot),
            info_user,
            nolx_types::abi::Siginfo::SIZE as u64,
            true,
        )
    };
    if let Err(error) = validated {
        return -i64::from(load_error_code(error));
    }
    if !table.processes[child_slot].zombie {
        // Suspend: the exit path writes the siginfo and this frame's rax at wake.
        table.processes[slot].blocked = true;
        table.processes[slot].wait_pid = child_pid;
        table.processes[slot].wait_info = info_user;
        dbg_line(format_args!(
            "NOLX: t2-dbg waitid block pid={} child={} child_zombie={} child_live={}",
            table.processes[slot].pid,
            child_pid,
            table.processes[child_slot].zombie,
            table.processes[child_slot].live
        ));
        return 0;
    }
    let status = table.processes[child_slot].exit_status;
    // SAFETY: the calling thread's slot is live and its space is active; the pointer was
    // validated above.
    let filled = unsafe { loader::fill_siginfo(space_of(slot), info_user, child_pid, status) };
    if let Err(error) = filled {
        return -i64::from(load_error_code(error));
    }
    table.processes[child_slot].live = false;
    0
}

/// The `posix_spawn` mechanism: a user-space path and argv name an embedded image.
pub fn spawn_from_user(pid_out: u64, path_user: u64, argv_user: u64) -> Result<u64, i32> {
    let mut table = TABLE.lock();
    let Some(slot) = table.current else {
        return Err(nolx_types::errno::ESRCH);
    };
    let parent = table.processes[slot].pid;
    // SAFETY: the calling thread's slot is live and its space is the active one.
    let space = unsafe { space_of(slot) };

    let path = loader::read_user_string(space, path_user).map_err(load_error_code)?;
    let Some((name, image)) = nolx_exec_core::programs_gen::PROGRAMS
        .iter()
        .find(|(candidate, _)| candidate.as_bytes() == path.as_bytes())
    else {
        return Err(nolx_types::errno::ENOENT);
    };

    // The argv vector: a user-space NULL-terminated pointer table, bounded in M6.
    let argv_strings = loader::read_user_argv(space, argv_user).map_err(load_error_code)?;
    let argv_bytes = argv_strings.as_slices();
    let mut argv_refs: [&str; loader::SPAWN_MAX_ARGS] = [""; loader::SPAWN_MAX_ARGS];
    for (slot, bytes) in argv_refs.iter_mut().zip(argv_bytes.iter()) {
        *slot = core::str::from_utf8(bytes).map_err(|_| nolx_types::errno::EINVAL)?;
    }

    let child = spawn_impl(
        &mut table,
        parent,
        name,
        image,
        &argv_refs[..argv_strings.len()],
    )
    .map_err(|error| match error {
        SetupError::TableFull => nolx_types::errno::EAGAIN,
        SetupError::AddressSpace | SetupError::Load(_) => nolx_types::errno::ENOMEM,
        SetupError::Scheduler(_) => nolx_types::errno::EAGAIN,
    })?;

    if pid_out != 0 {
        let bytes = child.to_le_bytes();
        loader::write_user(space, pid_out, &bytes).map_err(load_error_code)?;
    }
    Ok(child)
}

fn spawn_impl(
    table: &mut UserTable,
    parent: u64,
    path: &str,
    image: &[u8],
    argv: &[&str],
) -> Result<u64, SetupError> {
    let slot = table.free_slot().ok_or(SetupError::TableFull)?;
    let pid = table.next_pid;
    table.next_pid += 1;

    // SAFETY: the slot is free, so nobody holds its storage; it becomes live only after
    // `create_in_place` succeeds below.
    let place = unsafe { &mut *SPACES[slot].get() };
    let space = space::AddressSpace::create_in_place(place).ok_or(SetupError::AddressSpace)?;

    let entry = loader::load(space, image, path, argv).map_err(SetupError::Load)?;

    // SAFETY: the slot is free; its kernel stack is exclusively this thread's until the
    // slot is released.
    let stack = unsafe { &mut *KERNEL_STACKS[slot].get() };
    let stack_top = core::ptr::from_mut(stack).cast::<u8>() as u64 + KERNEL_STACK_SIZE as u64;
    let frame = initialize_user_stack(stack_top, entry.rip, entry.rsp);

    let thread_id = ThreadId::new(FIRST_USER_THREAD + slot as u64);
    table
        .scheduler
        .add_thread(thread_id, Policy::Other, 8, 1, 1)
        .map_err(SetupError::Scheduler)?;
    table.processes[slot] = UserProcess {
        pid,
        parent,
        live: true,
        zombie: false,
        blocked: false,
        exit_status: 0,
        thread_id,
        wait_info: 0,
        wait_pid: 0,
        written: 0,
        kernel_stack_top: stack_top,
    };
    table.frames[slot] = frame as usize;
    dbg_line(format_args!(
        "NOLX: t2-dbg spawned pid={} slot={} thread={:?} stack_top={:#x} frame={:?} entry={:#x} user_rsp={:#x}",
        pid, slot, thread_id, stack_top, frame, entry.rip, entry.rsp
    ));
    Ok(pid)
}

/// Whether any user thread is still runnable or running.
#[must_use]
pub fn active() -> bool {
    let table = TABLE.lock();
    table.current.is_some()
        || table.processes.iter().any(|process| {
            process.live
                && !process.zombie
                && table
                    .scheduler
                    .snapshot(process.thread_id)
                    .is_some_and(|snapshot| snapshot.state != ThreadState::Terminated)
        })
}
