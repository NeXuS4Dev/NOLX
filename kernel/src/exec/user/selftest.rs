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
            // Both lines are evidence a fixture greps for, and ring 3 starts as soon as the
            // second one is written, so they are printed with maskable interrupts off: a timer
            // switch inside a `writeln!` would let the new thread write into the middle of it
            // and leave the log with two fragments instead of the line.
            crate::hal::interrupts::without_interrupts(|| {
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
            });
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
    // With the input `read(2)` phase compiled in, the ring-3 descriptor table has to outlive
    // this report: that phase opens a device descriptor through the same context, and it
    // releases the VFS itself when it reports.
    #[cfg(not(feature = "hal-input-read-test"))]
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

/// The ring-3 input `read(2)` test: a user program reads real key records from the device.
///
/// The evidence this phase produces is deliberately not self-serving:
///
/// 1. the record comes from the PS/2 IRQ path — the harness injects a key through QMP, the
///    i8042 raises IRQ1, the decoder turns the scancode into a HID usage, and the producer
///    publishes it into the same registry a physical keyboard feeds. Nothing here fabricates
///    an event to hand the reader;
/// 2. the reader is a ring-3 program using only `open`/`read`/`close`, so what the kernel
///    reports is what a program could observe;
/// 3. the program demands *both* edges — press then release with the same usage — and checks
///    the class and source bytes, so a record that only loosely resembled the format would
///    fail it;
/// 4. the boot image stays honest when the hardware is not there: a machine without a PS/2
///    keyboard prints that the phase was skipped instead of claiming a pass.
///
/// The phase is a separate ring-3 run because a device descriptor needs a live process and a
/// mounted descriptor table, and the first phase's slots are retired when its evidence is read.
/// It runs after the M6/M7 evidence has been reported and before the memory selftests, which
/// take over every address space.
#[cfg(feature = "hal-input-read-test")]
pub mod read_test {
    use core::fmt::Write;

    use nolx_input_core::events::Source;
    use nolx_input_core::objects::Class;

    use crate::exec::user::{self, process_status};
    use crate::hal::console::Console;
    use crate::hal::input;

    /// The marker CI greps for.
    pub const PASS_MARKER: &str = "NOLX: input read(2) T2: ring-3 open/read/close on the PS/2 \
        keyboard device returned the injected press and release records";

    /// The status `inputread` exits with when every check held.
    const STATUS: i32 = 42;
    /// Bytes `inputread` writes when it succeeds.
    const WROTE: u64 = 48;
    /// Phase state: zero not started, one running, two reported.
    static PHASE: crate::sync::IrqMutex<u8> = crate::sync::IrqMutex::new(0);
    /// The pid `inputread` was given, or zero before the spawn.
    static PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

    /// Whether the machine had no PS/2 keyboard object to read from.
    static SKIPPED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

    /// Whether the phase is running: started, and the reader has not exited yet.
    #[must_use]
    pub fn running() -> bool {
        *PHASE.lock() == 1 && user::active()
    }

    /// Find the PS/2 keyboard's published Executive path.
    ///
    /// The name is allocated by publication order, so the test asks the binding which name this
    /// incarnation got instead of assuming `\\Device\\Input0` is the keyboard.
    fn keyboard_path() -> Option<&'static str> {
        let mut devices = [None; 16];
        let count = input::enumerate(&mut devices).ok()?;
        devices
            .into_iter()
            .take(count)
            .flatten()
            .filter(|info| info.connected)
            .find(|info| info.source == Source::Ps2Keyboard && info.class == Class::Keyboard)
            .and_then(|info| crate::hal::input::executive::name_of(info.id))
            .map(crate::hal::input::executive::path)
    }

    /// Start the phase once: spawn `inputread` and tell the harness to send a key.
    pub fn begin(console: &mut Console) {
        let mut state = PHASE.lock();
        if *state != 0 {
            return;
        }
        let Some(path) = keyboard_path() else {
            let _ = writeln!(
                console,
                "NOLX: input read(2) T2: no PS/2 keyboard object on this machine; phase skipped"
            );
            SKIPPED.store(true, core::sync::atomic::Ordering::Release);
            *state = 2;
            return;
        };
        match user::spawn(
            0,
            "inputread",
            nolx_exec_core::programs_gen::INPUTREAD_ELF,
            &["inputread", path],
        ) {
            Ok(pid) => {
                PID.store(pid, core::sync::atomic::Ordering::Release);
                let _ = writeln!(
                    console,
                    "NOLX: input read(2) T2: inputread (pid {pid}) opening {path}; \
                     send one key now"
                );
                *state = 1;
            }
            Err(error) => {
                let _ = writeln!(console, "NOLX: input read(2) T2: spawn FAILED: {error:?}");
                SKIPPED.store(true, core::sync::atomic::Ordering::Release);
                *state = 2;
            }
        }
    }

    /// Whether the phase was skipped because the machine has no PS/2 keyboard.
    ///
    /// A skip is reported, not counted as a pass: the harness's marker check is what decides
    /// whether a machine that should have a keyboard actually produced the evidence.
    #[must_use]
    pub fn skipped() -> bool {
        SKIPPED.load(core::sync::atomic::Ordering::Acquire)
    }

    /// Report the phase once the reader has exited, and return whether it passed.
    pub fn report(console: &mut Console) -> bool {
        {
            let state = PHASE.lock();
            if *state != 1 || user::active() {
                return false;
            }
        }
        let pid = PID.load(core::sync::atomic::Ordering::Acquire);
        let status = process_status(pid);
        let passed =
            status.is_some_and(|(zombie, code, wrote)| zombie && code == STATUS && wrote >= WROTE);
        let _ = writeln!(
            console,
            "NOLX: input read(2) T2: device descriptors still open: {}",
            user::input::open_count()
        );
        match status {
            Some((zombie, code, wrote)) => {
                let _ = writeln!(
                    console,
                    "NOLX: input read(2) T2: inputread zombie={zombie} status={code} wrote={wrote} \
                     (wanted status={STATUS} wrote>={WROTE})"
                );
            }
            None => {
                let _ = writeln!(
                    console,
                    "NOLX: input read(2) T2: inputread has no process record"
                );
            }
        }
        if passed {
            let _ = writeln!(console, "{PASS_MARKER}");
        }
        // The ring-3 VFS is released here: this phase is the last user of it when the feature
        // is compiled in, and the heap selftest that follows expects it back.
        user::cleanup_vfs();
        *PHASE.lock() = 2;
        passed
    }
}

/// The scheduler-blocking `poll(2)` phase: a ring-3 program suspends on a pointer device.
///
/// The evidence chain is deliberately split across two contexts. The program asks for a short
/// timeout (which must expire), then blocks for up to five seconds; the kernel prints
/// `input: poll blocked` from the syscall and `input: poll woke` from the idle loop that
/// completes it. The harness injects the pointer report only after it has read the block, so
/// the report cannot have been queued before the program suspended — which is what makes this
/// a block and not a race that happened to win.
#[cfg(feature = "hal-input-wait-test")]
pub mod poll_test {
    use core::fmt::Write;

    use nolx_input_core::objects::Class;

    use crate::exec::user::{self, process_status};
    use crate::hal::console::Console;
    use crate::hal::input;

    /// The marker the fixture greps for.
    pub const PASS_MARKER: &str = "NOLX: input poll T2: ring-3 poll(2) suspended the caller, \
        woke on the injected pointer report and the following read(2) matched it";

    /// The status `inputwait` exits with when every check held.
    const STATUS: i32 = 42;
    /// Bytes `inputwait` writes when it succeeds: its whole pass message.
    const WROTE: u64 = 64;
    /// Phase state: zero not started, one running, two reported.
    static PHASE: crate::sync::IrqMutex<u8> = crate::sync::IrqMutex::new(0);
    /// The pid `inputwait` was given, or zero before the spawn.
    static PID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// Whether the machine had no USB pointer object to wait on.
    static SKIPPED: crate::sync::IrqMutex<bool> = crate::sync::IrqMutex::new(false);
    /// The tick the phase first found no USB pointer, so the wait for one is bounded.
    static WAITING_SINCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
    /// Milliseconds to wait for a USB pointer to enumerate before reporting the phase skipped.
    const SETTLE_MS: u64 = 5000;

    /// Whether the phase has resolved: either a waiter was spawned or the phase was skipped.
    ///
    /// A waiter can only start once a USB pointer exists, so the driver loop asks again on
    /// every pass until this turns true.
    #[must_use]
    pub fn started() -> bool {
        *PHASE.lock() != 0
    }

    /// Whether the phase is running: started, and the waiter has not exited yet.
    #[must_use]
    pub fn running() -> bool {
        *PHASE.lock() == 1 && user::active()
    }

    /// Find a published USB pointer device's Executive path.
    ///
    /// USB, not any pointer: the fixture injects the report through QEMU's USB HID path, and
    /// the ring-3 program checks the record's source byte, so waiting on the PS/2 auxiliary
    /// port would test a different producer than the one the fixture drives.
    fn pointer_path() -> Option<&'static str> {
        let mut devices = [None; 16];
        let count = input::enumerate(&mut devices).ok()?;
        devices
            .into_iter()
            .take(count)
            .flatten()
            .filter(|info| {
                info.connected
                    && info.class == Class::Pointer
                    && info.source == nolx_input_core::events::Source::UsbHid
            })
            .find_map(|info| crate::hal::input::executive::name_of(info.id))
            .map(crate::hal::input::executive::path)
    }

    /// Start the phase once a USB pointer exists; return whether the phase is resolved.
    ///
    /// A USB device can take a moment to enumerate, so this is called every idle pass until it
    /// either spawns the waiter or gives up. The give-up bound matters: without it a machine
    /// with no USB pointer would hold the boot image open for ever.
    pub fn begin(console: &mut Console) -> bool {
        let mut state = PHASE.lock();
        if *state != 0 {
            return true;
        }
        let Some(path) = pointer_path() else {
            let now = crate::hal::timer::monotonic_ns() / 1_000_000;
            let first = WAITING_SINCE.load(core::sync::atomic::Ordering::Acquire);
            if first == 0 {
                WAITING_SINCE.store(now.max(1), core::sync::atomic::Ordering::Release);
                return false;
            }
            if now.saturating_sub(first) < SETTLE_MS {
                return false;
            }
            let _ = writeln!(
                console,
                "NOLX: input poll T2: no USB pointer device on this machine; phase skipped"
            );
            *SKIPPED.lock() = true;
            *state = 2;
            return true;
        };
        // An earlier ring-3 phase closed the descriptor table this process needs for `open`.
        user::mount_user_vfs();
        match user::spawn(
            0,
            "inputwait",
            nolx_exec_core::programs_gen::INPUTWAIT_ELF,
            &["inputwait", path],
        ) {
            Ok(pid) => {
                PID.store(pid, core::sync::atomic::Ordering::Release);
                let _ = writeln!(
                    console,
                    "NOLX: input poll T2: inputwait (pid {pid}) opening {path}; \
                     inject relative motion after it blocks"
                );
                *state = 1;
            }
            Err(error) => {
                let _ = writeln!(console, "NOLX: input poll T2: spawn FAILED: {error:?}");
                *SKIPPED.lock() = true;
                *state = 2;
            }
        }
        true
    }

    /// Whether the phase was skipped because the machine has no pointer device.
    #[must_use]
    pub fn skipped() -> bool {
        *SKIPPED.lock()
    }

    /// Report the phase once the waiter has exited; return whether its evidence chain held.
    pub fn report(console: &mut Console) -> bool {
        {
            let state = PHASE.lock();
            if *state != 1 || user::active() {
                return false;
            }
        }
        let pid = PID.load(core::sync::atomic::Ordering::Acquire);
        let status = process_status(pid);
        let passed =
            status.is_some_and(|(zombie, code, wrote)| zombie && code == STATUS && wrote >= WROTE);
        let _ = writeln!(
            console,
            "NOLX: input poll T2: device descriptors still open: {}",
            user::input::open_count()
        );
        match status {
            Some((zombie, code, wrote)) => {
                let _ = writeln!(
                    console,
                    "NOLX: input poll T2: inputwait zombie={zombie} status={code} wrote={wrote} \
                     (wanted status={STATUS} wrote>={WROTE})"
                );
            }
            None => {
                let _ = writeln!(
                    console,
                    "NOLX: input poll T2: inputwait has no process record"
                );
            }
        }
        if passed {
            let _ = writeln!(console, "{PASS_MARKER}");
        }
        user::cleanup_vfs();
        *PHASE.lock() = 2;
        passed
    }
}
