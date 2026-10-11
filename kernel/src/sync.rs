//! A mutable static whose `Sync` assumption is stated once instead of at every use site.

use core::cell::UnsafeCell;

/// A `static` that can be mutated.
///
/// `static mut` is being removed from the language, and rightly: taking a reference to one
/// is undefined behaviour the moment anything else can observe it, and the compiler has no
/// way to know whether that has happened. The `static_mut_refs` lint fires on exactly the
/// pattern the kernel's early boot needs -- a static array touched during bring-up.
///
/// This type does not weaken anything. It moves the assumption into one place and states
/// it: NOLX's early-boot statics are either written once before any reader exists, or
/// reached only by the single boot processor with interrupts disabled. Once there is more
/// than one processor, these become lock-protected structures and this type stops being the
/// right answer for them.
///
/// Every access is still `unsafe`, and every access site still says why it is safe.
pub struct SyncCell<T> {
    inner: UnsafeCell<T>,
}

// SAFETY: this is the assumption the whole type exists to name. Nothing about `SyncCell`
// serialises access on its own; each use site upholds the boot-ordering invariant, and a
// use site that does not is a bug the same way an unguarded `static mut` use was.
unsafe impl<T> Sync for SyncCell<T> {}

impl<T> SyncCell<T> {
    /// A cell holding `value`, usable in a `static` initialiser.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            inner: UnsafeCell::new(value),
        }
    }

    /// A raw pointer to the value.
    ///
    /// # Safety
    ///
    /// The caller must ensure that no other pointer or reference to the value is live, and
    /// that no other processor can reach it, for as long as the returned pointer is used.
    #[must_use]
    pub const unsafe fn get(&self) -> *mut T {
        self.inner.get()
    }
}

use core::sync::atomic::{AtomicBool, Ordering};

use crate::hal::cpu::InterruptGuard;

/// A spin lock that masks interrupts while it is held.
///
/// The second of the two boot-time synchronisation primitives, and the one that allocator and
/// page-table code needs: a lock that is released by a different code path than the one that
/// took it -- or worse, by an interrupt handler that ran while the lock was held -- is a
/// deadlock on a single processor. Disabling interrupts for the duration removes that class of
/// bug entirely.
///
/// It does not, by itself, make anything safe on more than one processor: the spin is a real
/// spin, and a second processor spinning with interrupts off is still spinning. M5 replaces this
/// with a queued spinlock at `DISPATCH` level; until then the kernel is single-processor by
/// construction, and the places that use this are the ones that will be rewritten.
pub struct IrqLock {
    locked: AtomicBool,
}

impl Default for IrqLock {
    fn default() -> Self {
        Self::new()
    }
}

impl IrqLock {
    /// An unlocked lock, usable in a `static` initialiser.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
        }
    }

    /// Acquire the lock with interrupts disabled, returning a guard that releases both.
    pub fn lock(&self) -> IrqGuard<'_> {
        let interrupts = InterruptGuard::disable();
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // The holder is another processor (or, in a bug, this one, which would spin here
            // forever with interrupts off). Let the cache line settle between attempts.
            core::hint::spin_loop();
        }
        IrqGuard {
            lock: self,
            _interrupts: interrupts,
        }
    }
}

/// The guard from [`IrqLock::lock`]: releases the lock, then restores interrupts.
pub struct IrqGuard<'a> {
    lock: &'a IrqLock,
    /// Dropped after `lock`: the guard restores IF only once the lock is free. The underscore
    /// says the field is not read -- its whole purpose is the `Drop` that runs when it goes.
    _interrupts: InterruptGuard,
}

impl Drop for IrqGuard<'_> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
