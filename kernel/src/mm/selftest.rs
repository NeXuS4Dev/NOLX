//! The memory manager's power-on self test.
//!
//! An allocator that is wrong is not a subsystem that returns bad answers: it is a machine
//! that hands the same frame to two owners and then does something inexplicable an hour
//! later. So the kernel does not merely trust the allocator it just built -- it exercises it
//! at boot, with interrupts live, and checks its work against the frame map afterwards.
//!
//! Two phases:
//!
//! * **single frames**, allocated and released twice, once in the order they were taken and
//!   once in reverse. The reverse pass is the one that matters: a coalescing bug that only
//!   shows up when the free list is walked from one end or the other gets no place to hide.
//! * **mixed orders**, a deterministic random walk over blocks from one frame to 256, with
//!   the allocator audited every few thousand operations. The phases run until the timer has
//!   ticked at least [`MIN_TICKS`] times, which is what makes this a test *under interrupt
//!   load*: the counter can only advance while frames are being allocated and freed.
//!
//! The test ends by proving the free-frame count came back to exactly where it started and
//! that the allocator, the frame map, and the magazines still agree. A failure is reported
//! and then panics: a kernel whose memory manager did not survive its own self test has no
//! business continuing to initialize.

use core::fmt;

use crate::hal::timer;
use crate::mm;
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::buddy::{FreeError, MAX_ORDER};

/// The stress keeps going until this many timer ticks have elapsed.
const MIN_TICKS: u64 = 3;

/// Frames the single-frame phase allocates at once.
const CHURN_FRAMES: usize = 1024;

/// Blocks the mixed-order phase keeps live at once.
const LIVE_BLOCKS: usize = 64;

/// The largest order the mixed-order phase asks for: 256 frames, or a megabyte, or whatever
/// the allocator's own ceiling is on a build where that is smaller.
const MAX_TEST_ORDER: usize = if MAX_ORDER < 8 { MAX_ORDER } else { 8 };

/// A ceiling on the operation count, so a timer that never ticks cannot hang the boot.
const MAX_OPERATIONS: u64 = 20_000_000;

/// The mixed-order phase audits the allocator every this many operations.
const AUDIT_INTERVAL: u64 = 4096;

/// The seed for the mixed-order walk, so a failure is reproducible from the log.
const SEED: u64 = 0x2545_F491_4F6C_DD1D;

/// Why the self test failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelftestError {
    /// The allocator would not hand out frames it should have had.
    OutOfFrames,
    /// A frame could not be returned to the allocator.
    Free(FreeError),
    /// The allocator, the frame map, or the magazines disagree with each other.
    Inconsistent(&'static str),
    /// The free-frame count did not return to where the test found it.
    NotRestored {
        /// Free frames before the test.
        before: u64,
        /// Free frames after it.
        after: u64,
    },
}

impl fmt::Display for SelftestError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfFrames => write!(formatter, "the frame allocator ran out of frames"),
            Self::Free(error) => write!(formatter, "a frame could not be freed: {error:?}"),
            Self::Inconsistent(what) => {
                write!(formatter, "the memory manager is inconsistent: {what}")
            }
            Self::NotRestored { before, after } => write!(
                formatter,
                "the free-frame count did not return: {before} before, {after} after"
            ),
        }
    }
}

impl From<FreeError> for SelftestError {
    fn from(error: FreeError) -> Self {
        Self::Free(error)
    }
}

/// Run the test, returning the reason it failed.
pub fn run(console: &mut dyn HalConsole) -> Result<(), SelftestError> {
    let free_before = mm::free_frames();
    let start_ticks = timer::ticks();
    let _ = writeln!(
        console,
        "NOLX: mm selftest: starting with {free_before} free frames, {} managed, ticks={start_ticks}",
        mm::managed_frames()
    );

    single_frame_phase(console)?;
    let operations = mixed_order_phase(console)?;
    let ticks = timer::ticks().saturating_sub(start_ticks);

    let free_after = mm::free_frames();
    if free_after != free_before {
        return Err(SelftestError::NotRestored {
            before: free_before,
            after: free_after,
        });
    }
    mm::audit().map_err(SelftestError::Inconsistent)?;

    let _ = writeln!(
        console,
        "NOLX: mm selftest: complete: {operations} operations, audit ok, ticks={ticks}, \\
         free frames {free_before} -> {free_after}"
    );
    Ok(())
}

/// Allocate every frame of a thousand, release them, and do it again backwards.
fn single_frame_phase(console: &mut dyn HalConsole) -> Result<(), SelftestError> {
    let mut frames = [0usize; CHURN_FRAMES];

    let mut allocated = 0;
    for slot in &mut frames {
        match mm::allocate_frame() {
            Some(frame) => {
                *slot = frame;
                allocated += 1;
            }
            None => break,
        }
    }
    if allocated < CHURN_FRAMES {
        return Err(SelftestError::OutOfFrames);
    }
    for slot in &frames {
        mm::free_block(*slot, 0)?;
    }

    let mut reallocated = 0;
    for slot in &mut frames {
        match mm::allocate_frame() {
            Some(frame) => {
                *slot = frame;
                reallocated += 1;
            }
            None => break,
        }
    }
    if reallocated < CHURN_FRAMES {
        return Err(SelftestError::OutOfFrames);
    }
    for slot in frames.iter().rev() {
        mm::free_block(*slot, 0)?;
    }

    mm::audit().map_err(SelftestError::Inconsistent)?;
    let _ = writeln!(
        console,
        "NOLX: mm selftest: single-frame phase allocated and freed {allocated} + \
         {reallocated} frames, audit ok"
    );
    Ok(())
}

/// Walk blocks from one frame to 256, taking and returning them, until the clock moves.
fn mixed_order_phase(console: &mut dyn HalConsole) -> Result<u64, SelftestError> {
    let start_ticks = timer::ticks();
    let mut random = SEED;
    let mut live = [(0usize, 0usize); LIVE_BLOCKS];
    let mut live_count = 0;
    let mut operations = 0u64;
    let mut allocations = 0u64;
    let mut frees = 0u64;

    while timer::ticks().saturating_sub(start_ticks) < MIN_TICKS && operations < MAX_OPERATIONS {
        let roll = next_random(&mut random);
        if roll % 4 != 0 || live_count == 0 {
            if live_count < LIVE_BLOCKS {
                let order = (roll >> 33) as usize % (MAX_TEST_ORDER + 1);
                if let Some(frame) = mm::allocate_frames(order) {
                    live[live_count] = (frame, order);
                    live_count += 1;
                    allocations += 1;
                }
            }
        } else {
            let index = (roll >> 17) as usize % live_count;
            let (frame, order) = live[index];
            live[index] = live[live_count - 1];
            live_count -= 1;
            mm::free_block(frame, order)?;
            frees += 1;
        }

        operations += 1;
        if operations % AUDIT_INTERVAL == 0 {
            mm::audit().map_err(SelftestError::Inconsistent)?;
        }
    }

    for (frame, order) in live.iter().take(live_count) {
        mm::free_block(*frame, *order)?;
    }

    let ticks = timer::ticks().saturating_sub(start_ticks);
    let _ = writeln!(
        console,
        "NOLX: mm selftest: mixed-order phase {operations} operations, {allocations} \
         allocations, {frees} frees in {ticks} ticks, audit ok"
    );
    Ok(operations)
}

/// xorshift64: deterministic, and small enough to keep in a kernel that cannot call `rand`.
fn next_random(state: &mut u64) -> u64 {
    let mut value = *state;
    value ^= value << 13;
    value ^= value >> 7;
    value ^= value << 17;
    *state = value;
    value
}
