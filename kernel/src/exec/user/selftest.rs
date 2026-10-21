//! The M6 ring-3 boot test: `init` spawns `hello`, observes its exit status, and only
//! then reports success.
//!
//! The evidence chain the marker asserts is entirely kernel-observed:
//!
//! 1. `hello` printed its greeting through `write` -- the kernel counted those bytes;
//! 2. `hello` exited with status 42 -- the kernel recorded it;
//! 3. `init` observed 42 through `waitid` -- proven by `init` printing the success line
//!    *and* exiting 42 itself (its assembly checks the `siginfo` status before doing so);
//! 4. `init` exited with status 42.
//!
//! The test is honest about what it cannot see: it cannot watch `init` read the siginfo.
//! What it can see is that `init`'s exit status is 42 only on the path where its own
//! `waitid` result was exactly 42, which is the contract the milestone states.

use core::fmt::Write;

use crate::exec::user::{self, process_status};

/// The marker CI greps for. Wording is part of the milestone's test contract.
pub const PASS_MARKER: &str =
    "NOLX: user mode T2: hello ran in ring 3, hello exited with status 42, \
     init observed 42 and exited with status 42";

/// The M7 ring-3 `tmpfs` T2 marker CI greps for.
pub const TMPFS_PASS_MARKER: &str =
    "NOLX: tmpfs T2: ring-3 open/write/lseek/read/close/unlink on /tmp/m7-check \
     (28 bytes round-tripped, verified=true)";

/// `hello`'s exit status the test demands.
const HELLO_STATUS: i32 = 42;
/// `init`'s exit status the test demands.
const INIT_STATUS: i32 = 42;
/// `init`'s pid: the driver spawns it first.
const INIT_PID: u64 = 1;
/// `hello`'s pid: `init`'s spawn allocates it second.
const HELLO_PID: u64 = 2;
/// Bytes in `hello`'s greeting line.
const HELLO_WROTE: u64 = 25;
/// Bytes in `init`'s output (`tmpfs: ring-3 read/write ok\n` + `init: child exited with status 42\n`).
const INIT_WROTE: u64 = 62;
/// Bytes round-tripped through `/tmp/m7-check` in `tmpfs`.
const TMPFS_BYTES: u64 = 28;

/// Start the user phase: install the idle thread and spawn `init` at ring 3.
pub fn begin(console: &mut crate::hal::console::Console) {
    user::begin().unwrap_or_else(|error| {
        let _ = writeln!(console, "NOLX: user mode T2: setup FAILED: {error:?}");
        panic!("user mode T2 setup: {error:?}");
    });
    match user::spawn(
        INIT_PID,
        "init",
        nolx_exec_core::programs_gen::INIT_ELF,
        &["init"],
    ) {
        Ok(pid) if pid == INIT_PID => {
            // The Executive handles the process was given are part of the spawn: report them
            // before ring 3 runs so the evidence does not depend on what the program does.
            if let Some(handles) = user::input_handles(pid) {
                let _ = writeln!(
                    console,
                    "NOLX: input executive: init holds {handles} input handle(s) from spawn"
                );
            }
            let _ = writeln!(
                console,
                "NOLX: user mode T2: init spawned (pid {pid}), entering ring 3"
            );
        }
        Ok(pid) => {
            let _ = writeln!(console, "NOLX: user mode T2: setup FAILED: init pid {pid}");
            panic!("user mode T2 setup: unexpected init pid");
        }
        Err(error) => {
            let _ = writeln!(console, "NOLX: user mode T2: setup FAILED: {error:?}");
            panic!("user mode T2 setup: {error:?}");
        }
    }
}

/// Whether the user phase is still in flight: any thread runs or waits.
#[must_use]
pub fn running() -> bool {
    user::active()
}

/// Whether every user thread has exited and the result is ready to read.
#[must_use]
pub fn complete() -> bool {
    !user::active()
}

/// Read the evidence chain, print the marker (or the failure), and report success.
pub fn report(console: &mut crate::hal::console::Console) -> bool {
    let init = process_status(INIT_PID);
    let hello = process_status(HELLO_PID);
    let tmpfs = user::tmpfs_evidence();
    user::cleanup_vfs();

    let hello_ok = hello.is_some_and(|(zombie, status, written)| {
        zombie && status == HELLO_STATUS && written >= HELLO_WROTE
    });
    let init_ok = init.is_some_and(|(zombie, status, written)| {
        zombie && status == INIT_STATUS && written >= INIT_WROTE
    });
    let tmpfs_ok = tmpfs.opens == 1
        && tmpfs.bytes_written == TMPFS_BYTES
        && tmpfs.seeks == 1
        && tmpfs.bytes_read == TMPFS_BYTES
        && tmpfs.closes == 1
        && tmpfs.unlinks == 1
        && tmpfs.verified;

    if hello_ok && init_ok && tmpfs_ok {
        let _ = writeln!(console, "{PASS_MARKER}");
        let _ = writeln!(console, "{TMPFS_PASS_MARKER}");
        return true;
    }

    let _ = writeln!(console, "NOLX: user mode T2: FAIL");
    match hello {
        Some((zombie, status, written)) => {
            let _ = writeln!(
                console,
                "NOLX: user mode T2: hello zombie={zombie} status={status} wrote={written} \
                 (wanted zombie=true status={HELLO_STATUS} wrote>={HELLO_WROTE})"
            );
        }
        None => {
            let _ = writeln!(console, "NOLX: user mode T2: hello has no process record");
        }
    }
    match init {
        Some((zombie, status, written)) => {
            let _ = writeln!(
                console,
                "NOLX: user mode T2: init zombie={zombie} status={status} wrote={written} \
                 (wanted zombie=true status={INIT_STATUS} wrote>={INIT_WROTE})"
            );
        }
        None => {
            let _ = writeln!(console, "NOLX: user mode T2: init has no process record");
        }
    }
    false
}
