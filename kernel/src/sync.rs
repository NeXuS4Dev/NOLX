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
        Self { inner: UnsafeCell::new(value) }
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
