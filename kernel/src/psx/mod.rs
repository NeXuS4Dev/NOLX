//! The POSIX subsystem.
//!
//! This is where the two halves of the project meet. Everything above this line in the
//! kernel is an object-based executive; everything a POSIX program can observe is produced
//! here. If a behaviour is specified in *The Open Group Base Specifications, Issue 8*, its
//! code lives in this module -- not in the I/O manager, not in a filesystem driver.
//!
//! In M3 this is the dispatch skeleton. The table exists first, deliberately, because it is
//! the contract: libc, the tests, and the conformance dashboard are all written against it.

use nolx_hal_core::console::HalConsole;
use nolx_types::errno;
use nolx_types::syscall;

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
/// It is empty in M3 because nothing is implemented yet. Rows are added as milestones land,
/// which is what makes the growth of this slice a usable progress measure.
static HANDLERS: &[Entry] = &[];

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
