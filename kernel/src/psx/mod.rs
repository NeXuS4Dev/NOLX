//! The POSIX subsystem.
//!
//! This is where the two halves of the project meet. Everything above this line in the
//! kernel is an object-based executive; everything a POSIX program can observe is produced
//! here. If a behaviour is specified in *The Open Group Base Specifications, Issue 8*, its
//! code lives in this module -- not in the I/O manager, not in a filesystem driver.
//!
//! In M3 this is the dispatch skeleton: a table that maps syscall numbers to handlers, with
//! every handler returning `ENOSYS` until its milestone implements it. The table exists
//! first, deliberately, because it is the contract: libc, the tests, and the conformance
//! dashboard are all written against it.

use core::fmt::Write;

use crate::hal::console::Console;
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

/// A syscall handler.
///
/// Takes the arguments and returns either a value or a negative `errno`. Handlers run at
/// `PASSIVE` level and may block; that is the whole point of them being here rather than in
/// the interrupt path.
pub type Handler = fn(SyscallArgs) -> SysResult;

/// One row of the dispatch table.
pub struct Entry {
    /// The syscall number.
    pub number: syscall::Number,
    /// The handler, or `None` for a number that is reserved but not implemented.
    pub handler: Option<Handler>,
}

/// The number of rows the dispatch table holds.
///
/// One slot per possible number would be 65536 entries; the table is sparse-indexed instead
/// and searched, because syscalls are not so hot that a linear scan of ~200 entries is the
/// bottleneck, and a dense table would be mostly `None` and mostly cache misses.
pub const MAX_ENTRIES: usize = 512;

static mut DISPATCH: [Option<Entry>; MAX_ENTRIES] = [None; MAX_ENTRIES];
static mut DISPATCH_LEN: usize = 0;

/// `ENOSYS` for every number, which is the correct answer for anything unimplemented.
fn unimplemented(_args: SyscallArgs) -> SysResult {
    -i64::from(errno::ENOSYS)
}

/// Register the dispatch table.
pub fn init(console: &mut Console) {
    // SAFETY: runs once on the boot processor before user mode exists, so there is no
    // concurrent reader. After this returns the table is read-only.
    unsafe {
        let mut count = 0usize;
        let mut index = 0usize;
        while index < syscall::TABLE.len() && count < MAX_ENTRIES {
            let (number, _name) = syscall::TABLE[index];
            DISPATCH[count] = Some(Entry { number, handler: Some(unimplemented) });
            count += 1;
            index += 1;
        }
        DISPATCH_LEN = count;
        let _ = writeln!(
            console,
            "psx: dispatch table has {count} entries, {} assigned numbers, all returning \
             ENOSYS until their milestone lands",
            syscall::TABLE.len()
        );
    }
}

/// Dispatch a syscall.
///
/// The single entry point from the `syscall` trap handler. Anything that is not a known
/// number gets `ENOSYS`, never a guess and never a panic: a bad number is a user-space bug,
/// and the kernel's job is to report it.
#[must_use]
pub fn dispatch(number: syscall::Number, args: SyscallArgs) -> SysResult {
    // SAFETY: the table is written once in `init` and read-only afterwards.
    let handler = unsafe {
        DISPATCH
            .iter()
            .take(DISPATCH_LEN)
            .find_map(|slot| slot.as_ref())
            .filter(|entry| entry.number == number)
            .and_then(|entry| entry.handler)
    };

    match handler {
        Some(f) => f(args),
        None => -i64::from(errno::ENOSYS),
    }
}

/// `true` if `number` is a known syscall, whether or not it is implemented.
#[must_use]
pub fn is_known(number: syscall::Number) -> bool {
    // SAFETY: as [`dispatch`].
    unsafe {
        DISPATCH
            .iter()
            .take(DISPATCH_LEN)
            .find_map(|slot| slot.as_ref())
            .is_some_and(|entry| entry.number == number)
    }
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
