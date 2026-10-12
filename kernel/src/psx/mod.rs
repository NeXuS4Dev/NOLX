//! The POSIX subsystem.
//!
//! This is where the two halves of the project meet. Everything above this line in the
//! kernel is an object-based executive; everything a POSIX program can observe is produced
//! here. If a behaviour is specified in *The Open Group Base Specifications, Issue 8*, its
//! code lives in this module -- not in the I/O manager, not in a filesystem driver.
//!
//! In M3 this was the dispatch skeleton; M6 fills in the ring-3 fast path's five calls.
//! The table contract remains the point: libc, the tests, and the conformance dashboard
//! are all written against it.

use nolx_hal_core::console::HalConsole;
use nolx_types::errno;
use nolx_types::syscall;

/// The numeric type of a syscall number, as the ABI defines it.
pub type Number = syscall::Number;

/// The result of a syscall: a non-negative value, or a negative `errno`.
pub type SysResult = i64;

/// Arguments as the `syscall` instruction delivers them.
///
/// `rcx` and `r11` are absent on purpose: the instruction itself consumes them (return
/// address and flags), so a caller that tried to pass an argument there would have it
/// silently destroyed. Six arguments is the most the register convention allows without
/// spilling to the stack, and POSIX needs no more.
#[derive(Clone, Copy, Debug)]
pub struct SyscallArgs {
    /// First argument.
    pub a0: u64,
    /// Second argument.
    pub a1: u64,
    /// Third argument.
    pub a2: u64,
    /// Fourth argument.
    pub a3: u64,
    /// Fifth argument.
    pub a4: u64,
    /// Sixth argument.
    pub a5: u64,
}

impl SyscallArgs {
    /// The arguments as a slice, for handlers that walk them generically.
    #[must_use]
    pub const fn as_slice(&self) -> [u64; 6] {
        [self.a0, self.a1, self.a2, self.a3, self.a4, self.a5]
    }
}

/// A syscall handler.
///
/// Takes the arguments and returns either a value or a negative `errno`. Handlers run at
/// `PASSIVE` level and may block; that is the whole point of them being here rather than in
/// the interrupt path.
pub type Handler = fn(SyscallArgs) -> SysResult;

/// One row of the dispatch table.
#[derive(Clone, Copy)]
pub struct Entry {
    /// The syscall number this row answers.
    pub number: syscall::Number,
    /// The handler.
    pub handler: Handler,
}

/// The implemented handlers.
///
/// A `&'static` slice, not a table built at boot. That choice buys four things at once: no
/// `static mut` and therefore no `Sync` assumption to justify, no initialisation order to
/// get wrong, no capacity limit, and no way for a number that is absent from
/// [`syscall::TABLE`] to acquire a handler.
///
/// M6 fills in the ring-3 fast path's five calls. Each one is the POSIX behaviour in this
/// module; the process-table mechanics it drives live in [`crate::exec::user`], which is
/// the same split the milestone's design fixes: psx decides *what* a call means, the
/// executive performs it.
static HANDLERS: &[Entry] = &[
    Entry {
        number: syscall::SYS_EXIT,
        handler: handle_exit,
    },
    Entry {
        number: syscall::SYS_WAITID,
        handler: handle_waitid,
    },
    Entry {
        number: syscall::SYS_GETPID,
        handler: handle_getpid,
    },
    Entry {
        number: syscall::SYS_POSIX_SPAWN,
        handler: handle_posix_spawn,
    },
    Entry {
        number: syscall::SYS_OPEN,
        handler: handle_open,
    },
    Entry {
        number: syscall::SYS_CLOSE,
        handler: handle_close,
    },
    Entry {
        number: syscall::SYS_READ,
        handler: handle_read,
    },
    Entry {
        number: syscall::SYS_WRITE,
        handler: handle_write,
    },
    Entry {
        number: syscall::SYS_LSEEK,
        handler: handle_lseek,
    },
    Entry {
        number: syscall::SYS_UNLINK,
        handler: handle_unlink,
    },
];

/// POSIX `exit`: the calling process terminates with the low 8 bits of `status`.
///
/// The `_Exit` semantics are deliberate: no flushing, no atexit, no signal delivery. Those
/// belong to libc and to later milestones; a kernel that invented them here would be
/// implementing a different contract than the one libc will assume.
fn handle_exit(args: SyscallArgs) -> SysResult {
    crate::exec::user::exit_current(args.a0 as i32);
    0
}

/// POSIX `waitid`: report one child's state change into a `siginfo_t`.
///
/// Only `P_PID` + `WEXITED` are implemented: the milestone's contract is "the parent
/// observes the child's exit status", and pretending to support process groups or
/// `WNOWAIT` without a job control story would be the kind of partial support that
/// programs cannot detect until production.
fn handle_waitid(args: SyscallArgs) -> SysResult {
    let (idtype, id, info_user, options) = (args.a0 as i32, args.a1, args.a2, args.a3 as i32);
    if idtype != nolx_types::abi::idtype::P_PID || options != nolx_types::abi::waitopt::WEXITED {
        return -i64::from(errno::EINVAL);
    }
    if id == 0 {
        return -i64::from(errno::ECHILD);
    }
    crate::exec::user::waitid(id, info_user)
}

/// POSIX `getpid`: the calling process's identity. No caching, no getpid-as-a-constant.
fn handle_getpid(_args: SyscallArgs) -> SysResult {
    crate::exec::user::current_pid() as i64
}

/// POSIX `posix_spawn`'s file-actions-free core: create a process from an embedded image.
///
/// The `path` names a program the kernel embeds (see [`nolx_exec_core::programs_gen`]);
/// `argv` is a user-space NULL-terminated pointer table; `envp` must be NULL for now. A
/// VFS-backed spawn replaces the lookup without touching this contract.
fn handle_posix_spawn(args: SyscallArgs) -> SysResult {
    let (pid_out, path_user, argv_user, envp_user) = (args.a0, args.a1, args.a2, args.a3);
    if envp_user != 0 {
        // No environment support yet: refusing beats silently dropping variables.
        return -i64::from(errno::ENOSYS);
    }
    match crate::exec::user::spawn_from_user(pid_out, path_user, argv_user) {
        Ok(child) => child as i64,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `write`: descriptors 1 and 2 reach the console; descriptors `>= 3` write through
/// the VFS to the underlying open file object.
fn handle_write(args: SyscallArgs) -> SysResult {
    let (fd, buffer, length) = (args.a0, args.a1, args.a2);
    let outcome = if fd == 1 || fd == 2 {
        crate::exec::user::write_console(buffer, length)
    } else if fd >= 3 {
        match usize::try_from(fd) {
            Ok(fd_idx) => crate::exec::user::write_file(fd_idx, buffer, length),
            Err(_) => Err(errno::EBADF),
        }
    } else {
        Err(errno::EBADF)
    };
    match outcome {
        Ok(written) => written as i64,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `open`: resolve `path`, optionally create or truncate in `tmpfs`, and allocate
/// the lowest free descriptor in the process descriptor table.
fn handle_open(args: SyscallArgs) -> SysResult {
    let (path_user, flags, mode) = (args.a0, args.a1 as u32, args.a2 as u32);
    match crate::exec::user::open_file(path_user, flags, mode) {
        Ok(fd) => fd as i64,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `close`: release a descriptor slot and decrement its `FileObject` reference count.
fn handle_close(args: SyscallArgs) -> SysResult {
    let Ok(fd) = usize::try_from(args.a0) else {
        return -i64::from(errno::EBADF);
    };
    if fd < 3 {
        return -i64::from(errno::EBADF);
    }
    match crate::exec::user::close_file(fd) {
        Ok(_) => 0,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `read`: read from an open VFS file descriptor into user space.
fn handle_read(args: SyscallArgs) -> SysResult {
    let Ok(fd) = usize::try_from(args.a0) else {
        return -i64::from(errno::EBADF);
    };
    if fd < 3 {
        return -i64::from(errno::EBADF);
    }
    let (buffer, length) = (args.a1, args.a2);
    match crate::exec::user::read_file(fd, buffer, length) {
        Ok(read) => read as i64,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `lseek`: reposition an open VFS file object's byte offset.
fn handle_lseek(args: SyscallArgs) -> SysResult {
    let Ok(fd) = usize::try_from(args.a0) else {
        return -i64::from(errno::EBADF);
    };
    if fd < 3 {
        return -i64::from(errno::ESPIPE);
    }
    let (offset, whence) = (args.a1 as i64, args.a2 as i32);
    match crate::exec::user::lseek_file(fd, offset, whence) {
        Ok(pos) => pos as i64,
        Err(code) => -i64::from(code),
    }
}

/// POSIX `unlink`: remove a directory entry from `tmpfs`.
fn handle_unlink(args: SyscallArgs) -> SysResult {
    match crate::exec::user::unlink_file(args.a0) {
        Ok(_) => 0,
        Err(code) => -i64::from(code),
    }
}

/// The answer for every syscall that has no handler yet.
///
/// `ENOSYS` is the correct POSIX response to an unimplemented function, and it is the
/// response `configure` scripts and `libc` probes expect, so returning anything else would
/// actively mislead a program that is testing for support.
fn unimplemented(_args: SyscallArgs) -> SysResult {
    -i64::from(errno::ENOSYS)
}

/// Report what the subsystem can and cannot do.
pub fn init(console: &mut dyn HalConsole) {
    let _ = writeln!(
        console,
        "psx: {} syscall numbers assigned, {} implemented, the rest return ENOSYS",
        syscall::TABLE.len(),
        HANDLERS.len()
    );
}

/// Dispatch a syscall.
///
/// The single entry point from the `syscall` trap handler. Anything with no handler gets
/// `ENOSYS`, never a guess and never a panic: a bad number is a user-space bug, and the
/// kernel's job is to report it.
#[must_use]
pub fn dispatch(number: syscall::Number, args: SyscallArgs) -> SysResult {
    match HANDLERS.iter().find(|entry| entry.number == number) {
        Some(entry) => (entry.handler)(args),
        None => unimplemented(args),
    }
}

/// `true` if `number` names a syscall POSIX assigns, whether or not NOLX implements it.
///
/// This distinction matters: `ENOSYS` says "not implemented", which is a legitimate answer
/// a program can act on, while an unassigned number means the caller's ABI assumptions are
/// wrong. Both get `ENOSYS` from `dispatch`, because POSIX defines no other error for it,
/// but only one of them is a missing feature.
#[must_use]
pub fn is_assigned(number: syscall::Number) -> bool {
    syscall::name_of(number).is_some()
}

/// `true` if `number` has a handler in this kernel.
#[must_use]
pub fn is_implemented(number: syscall::Number) -> bool {
    HANDLERS.iter().any(|entry| entry.number == number)
}

/// Whether a blocking call that was interrupted should return `EINTR` or be restarted.
///
/// This is the `SA_RESTART` matrix, in one place, because getting it wrong per-call is how
/// shells end up dropping output on a busy system.
#[must_use]
pub const fn restart_on_signal(call: syscall::Number, sa_restart: bool) -> bool {
    if !sa_restart {
        return false;
    }
    // The restartable set. `read`, `write`, and friends restart; anything with a timeout or
    // a partial-progress contract does not, because restarting it would lose the progress
    // the caller is entitled to observe.
    matches!(
        call,
        syscall::SYS_READ
            | syscall::SYS_WRITE
            | syscall::SYS_READV
            | syscall::SYS_WRITEV
            | syscall::SYS_IOCTL
            | syscall::SYS_WAITID
            | syscall::SYS_OPEN
            | syscall::SYS_OPENAT
            | syscall::SYS_NANOSLEEP
    )
}
