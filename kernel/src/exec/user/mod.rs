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
use nolx_obj_core::handle::HandleTable;
use nolx_vfs::fd::DescriptorTable;
use nolx_vfs::tmpfs::Tmpfs;
use nolx_vfs::vfs::{FsInstance, ProcessVfsContext, VirtualFileSystem};

use crate::hal::context::initialize_user_stack;
use crate::mm::space::{self, Space};
use crate::sync::{IrqMutex, SyncCell};

/// The fixed number of user processes the table holds.
pub const MAX_USER_PROCESSES: usize = 4;

/// Kernel stack size per user thread.
///
/// A spawn is not a shallow call: it takes the process table lock, builds a PTE store, copies
/// the kernel half of a PML4 and writes a fresh `VadTree` -- and that tree alone is
/// `SPACE_NODES` descriptors, about 26 KiB of temporary when it is materialised by value. Four
/// slots at 32 KiB left a nested spawn (a ring-3 parent creating its child) with under 6 KiB of
/// headroom, which is how a 26 KiB frame once ran off the top of slot 0 and overwrote the block
/// layer's binding table. 128 KiB keeps a whole tree temporary at a fifth of the stack; the
/// sentinels below make the next such mistake loud instead of silent.
pub const KERNEL_STACK_SIZE: usize = 128 * 1024;

/// The sentinel word repeated in the guard bands on both ends of a [`KernelStack`].
pub const STACK_GUARD: u64 = 0x4E4F_4C58_5354_4143; // "NOLXSTAC"

/// How many sentinel words each guard band holds: 512 words, 4 KiB.
///
/// Both ends are banded. The processor's stack grows down, but a *materialised* temporary can
/// also be written past the top when a large frame is built before the stack pointer is
/// lowered -- exactly what the hardware watchpoint caught, where the overwriting `memcpy`
/// started 0x30 bytes above the top of the stack. A band narrower than that offset would be
/// stepped straight over, so the band is a whole page's worth: a run-off has to be shifted by
/// more than 4 KiB to escape, and by then it has already destroyed the band it skipped.
pub const STACK_GUARD_WORDS: usize = 512;

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
    /// Input-device handles this process was given at spawn.
    ///
    /// Kept after exit as the spawn's record; the handles themselves are closed when the
    /// process exits, and a slot reused by a new process starts from zero again.
    pub input_handles: u8,
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
            input_handles: 0,
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
///
/// The body is banded by [`STACK_GUARD_WORDS`] sentinels on both sides, so a frame that runs
/// off either end hits a sentinel instead of an unrelated static in .bss. [`KernelStack::top`]
/// returns the address the stack pointer starts at, and [`KernelStack::guard_intact`] reads the
/// bands back.
#[repr(C, align(16))]
pub struct KernelStack {
    low_guard: [u64; STACK_GUARD_WORDS],
    body: [u8; KERNEL_STACK_SIZE],
    high_guard: [u64; STACK_GUARD_WORDS],
}

impl KernelStack {
    const fn empty() -> Self {
        Self {
            low_guard: [STACK_GUARD; STACK_GUARD_WORDS],
            body: [0; KERNEL_STACK_SIZE],
            high_guard: [STACK_GUARD; STACK_GUARD_WORDS],
        }
    }

    /// The address the stack pointer starts at: the first byte after the stack body.
    fn top(&mut self) -> u64 {
        // SAFETY: the body is exactly `KERNEL_STACK_SIZE` bytes of this object, and the pointer
        // is formed from the field itself rather than from a guess about layout.
        let base = core::ptr::addr_of_mut!(self.body).cast::<u8>() as u64;
        base + KERNEL_STACK_SIZE as u64
    }

    /// Whether every sentinel word in both bands still holds [`STACK_GUARD`].
    fn guard_intact(&self) -> bool {
        self.low_guard
            .iter()
            .chain(self.high_guard.iter())
            .all(|word| *word == STACK_GUARD)
    }

    /// Re-arm both bands, for a slot being reused by a new process.
    fn rearm(&mut self) {
        self.low_guard = [STACK_GUARD; STACK_GUARD_WORDS];
        self.high_guard = [STACK_GUARD; STACK_GUARD_WORDS];
    }
}

/// Executive handles each process slot holds, created at spawn and closed at exit.
///
/// The table is per slot rather than per thread because handles belong to the process, and it
/// is created lazily: a `HandleTable` asserts its capacity in `new`, which cannot run in a
/// static initialiser. A slot that never spawned anything holds `None`.
static PROCESS_HANDLES: [SyncCell<Option<HandleTable<crate::obj::boot::ObjectId, 4>>>;
    MAX_USER_PROCESSES] = [const { SyncCell::new(None) }; MAX_USER_PROCESSES];

/// Stable kernel-stack storage for each process slot.
static KERNEL_STACKS: [SyncCell<KernelStack>; MAX_USER_PROCESSES] =
    [const { SyncCell::new(KernelStack::empty()) }; MAX_USER_PROCESSES];

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
            crate::hal::cpu::set_kernel_stack_top(table.processes[slot].kernel_stack_top);
        }
    }
    table.current = next_slot;
    match next_slot {
        Some(slot) => table.frame_pointer(slot),
        None => table.idle_frame_pointer(),
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

/// Panic if the running process's kernel stack ran past its sentinels.
///
/// The kernel stack is the one place a deep call tree can silently destroy unrelated kernel
/// state: .bss follows every stack, so an overflow lands on whatever the linker put next.
/// Checking the sentinels costs four comparisons on the exit path and turns that into an
/// immediate, named failure.
fn check_stack_guard() {
    let table = TABLE.lock();
    let Some(slot) = table.current else {
        return;
    };
    // SAFETY: the slot is the running one, so its stack is exclusively owned by this thread
    // and the shared borrow lasts no longer than this check.
    let stack = unsafe { &*KERNEL_STACKS[slot].get() };
    assert!(
        stack.guard_intact(),
        "the kernel stack of process slot {slot} overflowed into .bss"
    );
}

/// Open the Executive's input objects for the process in `slot`.
///
/// A refusal is not a spawn failure: a machine with no input hardware simply gives the process
/// no handles, and a table shortage is reported by the count the caller records.
fn bind_input_handles(slot: usize) -> u8 {
    // SAFETY: `slot` is the slot this single-threaded spawn is populating, and no other code
    // reaches its table until the process starts running.
    let table = unsafe { &mut *PROCESS_HANDLES[slot].get() };
    let table = table.get_or_insert_with(HandleTable::new);
    // A refusal is not a spawn failure: the process simply starts without input handles, and
    // the count the caller records is what the boot report shows.
    crate::hal::input::bind_process(table, &crate::hal::input::process_token())
        .map_or(0, |opened| opened as u8)
}

/// Close the process's Executive handles. Called once, when the slot is retired.
fn release_input_handles(slot: usize) {
    // SAFETY: the slot's process has exited, so nothing else can reach this table.
    let table = unsafe { &mut *PROCESS_HANDLES[slot].get() };
    if let Some(table) = table.as_mut() {
        crate::hal::input::release_process(table);
    }
    *table = None;
}

/// Terminate the calling process: POSIX `exit`, minus the library's flushing.
///
/// The frame switch itself happens in [`on_syscall`], which sees the zombie and parks the
/// thread; what this function does is the bookkeeping a waiter observes: the status is
/// kept, and a parent blocked in `waitid` is woken with its result already written into
/// its own frame and address space.
pub fn exit_current(status: i32) {
    // A process that exits is the last moment its kernel stack is still the one it ran on, so
    // this is where an overflow is caught before the slot is reused.
    check_stack_guard();
    let mut table = TABLE.lock();
    let Some(slot) = table.current else {
        return;
    };
    table.processes[slot].zombie = true;
    table.processes[slot].exit_status = status;
    // The handles die with the process, exactly as `ExitProcess` closes them. The count stays
    // in the slot as the record of what the spawn gave it, until a new process reuses the slot.
    release_input_handles(slot);
    let pid = table.processes[slot].pid;

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

/// Prepare the scheduler and VFS for the user test: install the idle thread and mount
/// the ring-3 `tmpfs` instance.
pub fn begin() -> Result<(), SetupError> {
    {
        let mut vfs = VirtualFileSystem::new();
        let root = vfs
            .mount_root(
                FsInstance::Tmpfs(Tmpfs::with_limits(64, 16 * 1024)),
                "tmpfs",
            )
            .expect("ring-3 root tmpfs mount must succeed");
        let mut ctx = ProcessVfsContext::new(root);
        ctx.fds = DescriptorTable::with_stdio(0, 1, 2);
        let _ = vfs.mkdir(&ctx, b"/tmp", 0o777);
        *USER_VFS.lock() = Some((vfs, ctx));
        *TMPFS_EVIDENCE.lock() = TmpfsT2Evidence::new();
    }
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

    // Creating a space writes a `VadTree` descriptor array of tens of kilobytes, and the
    // caller's own kernel stack is what it is written through. Check the caller's sentinels
    // *here*, while the damage is still attributable, instead of discovering it later as an
    // unrelated subsystem that stopped answering.
    if let Some(caller) = table.current {
        // SAFETY: the calling thread owns its slot's stack for as long as it runs.
        let caller_stack = unsafe { &*KERNEL_STACKS[caller].get() };
        assert!(
            caller_stack.guard_intact(),
            "spawn from process slot {caller} overflowed its kernel stack"
        );
    }

    let entry = loader::load(space, image, path, argv).map_err(SetupError::Load)?;

    // SAFETY: the slot is free; its kernel stack is exclusively this thread's until the
    // slot is released.
    let stack = unsafe { &mut *KERNEL_STACKS[slot].get() };
    stack.rearm();
    let stack_top = stack.top();
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
        input_handles: bind_input_handles(slot),
    };
    table.frames[slot] = frame as usize;
    Ok(pid)
}

/// Input handles the process with `pid` was given at spawn, for the boot report.
#[must_use]
pub fn input_handles(pid: u64) -> Option<u8> {
    let table = TABLE.lock();
    table
        .slot_by_pid(pid)
        .map(|slot| table.processes[slot].input_handles)
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

/// Kernel-observed evidence chain for the ring-3 `tmpfs` T2 round-trip test.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TmpfsT2Evidence {
    /// Successful `SYS_OPEN` calls on `tmpfs`.
    pub opens: u64,
    /// Bytes written via `SYS_WRITE` to `tmpfs` descriptors (`fd >= 3`).
    pub bytes_written: u64,
    /// Successful `SYS_LSEEK` calls on `tmpfs` descriptors.
    pub seeks: u64,
    /// Bytes read back via `SYS_READ` from `tmpfs` descriptors.
    pub bytes_read: u64,
    /// Successful `SYS_CLOSE` calls on `tmpfs` descriptors.
    pub closes: u64,
    /// Successful `SYS_UNLINK` calls on `tmpfs`.
    pub unlinks: u64,
    /// `true` if the bytes read back via `SYS_READ` matched `"tmpfs: ring-3 read/write ok\n"`.
    pub verified: bool,
}

impl TmpfsT2Evidence {
    const fn new() -> Self {
        Self {
            opens: 0,
            bytes_written: 0,
            seeks: 0,
            bytes_read: 0,
            closes: 0,
            unlinks: 0,
            verified: false,
        }
    }
}

static USER_VFS: IrqMutex<Option<(VirtualFileSystem, ProcessVfsContext)>> = IrqMutex::new(None);
static TMPFS_EVIDENCE: IrqMutex<TmpfsT2Evidence> = IrqMutex::new(TmpfsT2Evidence::new());

/// Snapshot the ring-3 `tmpfs` T2 evidence chain.
#[must_use]
pub fn tmpfs_evidence() -> TmpfsT2Evidence {
    *TMPFS_EVIDENCE.lock()
}

/// Release the ring-3 `VirtualFileSystem` heap allocations once all user processes have
/// exited, returning the kernel heap to zero bytes in use before `mm::heap::selftest`.
pub fn cleanup_vfs() {
    *USER_VFS.lock() = None;
}

/// Ring-3 `open(path_user, flags, mode)` backed by `USER_VFS` (`tmpfs`).
pub fn open_file(path_user: u64, flags: u32, mode: u32) -> Result<u64, i32> {
    let slot = TABLE.lock().current.ok_or(nolx_types::errno::ESRCH)?;
    // SAFETY: the calling thread's slot is live and its address space is active.
    let space = unsafe { space_of(slot) };
    let path = loader::read_user_string(space, path_user).map_err(load_error_code)?;

    let mut guard = USER_VFS.lock();
    let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::ENOENT)?;
    let fd = vfs.open(ctx, path.as_bytes(), flags, mode)?;
    TMPFS_EVIDENCE.lock().opens += 1;
    Ok(fd as u64)
}

/// Ring-3 `close(fd)` backed by `USER_VFS`.
pub fn close_file(fd: usize) -> Result<u64, i32> {
    let mut guard = USER_VFS.lock();
    let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::EBADF)?;
    vfs.close(ctx, fd)?;
    TMPFS_EVIDENCE.lock().closes += 1;
    Ok(0)
}

/// Ring-3 `write(fd, user_buffer, length)` to an open `tmpfs` file (`fd >= 3`).
pub fn write_file(fd: usize, user_buffer: u64, length: u64) -> Result<u64, i32> {
    if length == 0 {
        return Ok(0);
    }
    let slot = TABLE.lock().current.ok_or(nolx_types::errno::ESRCH)?;
    // SAFETY: the calling thread's slot is live and its address space is active.
    let space = unsafe { space_of(slot) };

    let mut done = 0u64;
    let mut chunk = [0u8; 256];
    while done < length {
        let take = (length - done).min(chunk.len() as u64) as usize;
        loader::read_user(space, user_buffer + done, &mut chunk[..take])
            .map_err(load_error_code)?;
        let written = {
            let mut guard = USER_VFS.lock();
            let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::EBADF)?;
            vfs.write(ctx, fd, &chunk[..take])?
        };
        done += written as u64;
        if written < take {
            break;
        }
    }
    TMPFS_EVIDENCE.lock().bytes_written += done;
    Ok(done)
}

/// Ring-3 `read(fd, user_buffer, length)` from an open `tmpfs` file.
pub fn read_file(fd: usize, user_buffer: u64, length: u64) -> Result<u64, i32> {
    if length == 0 {
        return Ok(0);
    }
    let slot = TABLE.lock().current.ok_or(nolx_types::errno::ESRCH)?;
    // SAFETY: the calling thread's slot is live and its address space is active.
    let space = unsafe { space_of(slot) };

    let mut done = 0u64;
    let mut chunk = [0u8; 256];
    while done < length {
        let take = (length - done).min(chunk.len() as u64) as usize;
        let n = {
            let mut guard = USER_VFS.lock();
            let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::EBADF)?;
            vfs.read(ctx, fd, &mut chunk[..take])?
        };
        if n == 0 {
            break;
        }
        loader::write_user(space, user_buffer + done, &chunk[..n]).map_err(load_error_code)?;
        if done == 0 && n == 28 && &chunk[..28] == b"tmpfs: ring-3 read/write ok\n" {
            TMPFS_EVIDENCE.lock().verified = true;
        }
        done += n as u64;
        if n < take {
            break;
        }
    }
    TMPFS_EVIDENCE.lock().bytes_read += done;
    Ok(done)
}

/// Ring-3 `lseek(fd, offset, whence)` on an open `tmpfs` file.
pub fn lseek_file(fd: usize, offset: i64, whence: i32) -> Result<u64, i32> {
    let mut guard = USER_VFS.lock();
    let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::EBADF)?;
    let pos = vfs.lseek(ctx, fd, offset, whence)?;
    TMPFS_EVIDENCE.lock().seeks += 1;
    Ok(pos)
}

/// Ring-3 `unlink(path_user)` on `USER_VFS`.
pub fn unlink_file(path_user: u64) -> Result<u64, i32> {
    let slot = TABLE.lock().current.ok_or(nolx_types::errno::ESRCH)?;
    // SAFETY: the calling thread's slot is live and its address space is active.
    let space = unsafe { space_of(slot) };
    let path = loader::read_user_string(space, path_user).map_err(load_error_code)?;

    let mut guard = USER_VFS.lock();
    let (vfs, ctx) = guard.as_mut().ok_or(nolx_types::errno::ENOENT)?;
    vfs.unlink(ctx, path.as_bytes())?;
    TMPFS_EVIDENCE.lock().unlinks += 1;
    Ok(0)
}
