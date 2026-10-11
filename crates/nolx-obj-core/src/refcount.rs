//! Reference counting rules for object headers, as the object manager applies them.
//!
//! An object starts with one pointer reference, held by whoever created it (usually the
//! namespace binding). Every handle implies a pointer reference too, so opening a handle adds
//! one to both counts and closing it drops the handle count and then releases the pointer.
//! The object is deleted by whichever release takes the pointer count to zero, and
//! `handles <= pointers` holds at every step.
//!
//! The kernel keeps these counts in atomics on the object header (`kernel/src/obj/mod.rs`).
//! This type states the same rules without atomics so that the host tests can drive them.

/// A reference-count rule was broken by the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefError {
    /// A count would go below zero: a release or close without a matching reference.
    Underflow,
    /// More handles than pointer references: a handle outlived its pointer.
    HandlesExceedPointers,
}

/// The `(pointer_count, handle_count)` pair of one object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefCount {
    pointers: u32,
    handles: u32,
}

impl RefCount {
    /// A new object: one pointer reference, no handles.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pointers: 1,
            handles: 0,
        }
    }

    /// The number of pointer references.
    #[must_use]
    pub const fn pointers(&self) -> u32 {
        self.pointers
    }

    /// The number of open handles.
    #[must_use]
    pub const fn handles(&self) -> u32 {
        self.handles
    }

    /// A handle was granted: it adds a handle reference and the pointer reference it implies.
    pub fn add_handle(&mut self) {
        self.pointers += 1;
        self.handles += 1;
    }

    /// A handle was closed: only the handle reference goes. The caller then calls
    /// [`RefCount::release`] for the pointer reference the handle carried.
    pub fn drop_handle(&mut self) -> Result<(), RefError> {
        self.handles = self.handles.checked_sub(1).ok_or(RefError::Underflow)?;
        self.check()
    }

    /// One pointer reference was dropped. Returns `true` when this was the last one, which
    /// means the object is deleted and must not be referred to again.
    pub fn release(&mut self) -> Result<bool, RefError> {
        self.pointers = self.pointers.checked_sub(1).ok_or(RefError::Underflow)?;
        self.check()?;
        Ok(self.pointers == 0)
    }

    fn check(&self) -> Result<(), RefError> {
        if self.handles > self.pointers {
            Err(RefError::HandlesExceedPointers)
        } else {
            Ok(())
        }
    }
}

impl Default for RefCount {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lifecycle_matches_the_boot_self_test() {
        // (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, as in kernel/src/obj/boot.rs.
        let mut c = RefCount::new();
        assert_eq!((c.pointers(), c.handles()), (1, 0));
        c.add_handle();
        assert_eq!((c.pointers(), c.handles()), (2, 1));
        c.add_handle();
        assert_eq!((c.pointers(), c.handles()), (3, 2));
        c.drop_handle().unwrap();
        assert_eq!(c.release(), Ok(false));
        assert_eq!((c.pointers(), c.handles()), (2, 1));
        // Binding removed: the pointer that the namespace held goes, and the object lives on
        // through its handle.
        assert_eq!(c.release(), Ok(false));
        assert_eq!((c.pointers(), c.handles()), (1, 1));
        // Last handle closes, then its pointer goes: deleted.
        c.drop_handle().unwrap();
        assert_eq!(c.release(), Ok(true));
    }

    #[test]
    fn underflow_is_reported_not_wrapped() {
        let mut c = RefCount::new();
        assert_eq!(c.drop_handle(), Err(RefError::Underflow));
        assert_eq!(c.release(), Ok(true));
        assert_eq!(c.release(), Err(RefError::Underflow));
    }

    #[test]
    fn releasing_the_pointer_under_a_live_handle_is_refused() {
        let mut c = RefCount::new();
        c.add_handle();
        // The namespace's pointer goes first: one pointer is left for the open handle.
        assert_eq!(c.release(), Ok(false));
        // Dropping the last pointer while the handle is still open is refused.
        assert_eq!(c.release(), Err(RefError::HandlesExceedPointers));
    }
}

#[cfg(test)]
mod fuzz_tests {
    use super::*;
    use crate::handle::{Handle, HandleError, HandleTable, ACCESS_ALL};

    const OBJECTS: usize = 4;
    const SLOTS: usize = 8;
    const STEPS: usize = 30_000;

    /// A small deterministic generator, so a failure reproduces exactly.
    struct Rng(u64);

    impl Rng {
        fn next_u64(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n as u64) as usize
        }
    }

    /// Pick a random index whose flag is set, or `None` if no flag is set.
    fn pick(rng: &mut Rng, flags: &[bool]) -> Option<usize> {
        let count = flags.iter().filter(|flag| **flag).count();
        if count == 0 {
            return None;
        }
        let mut k = rng.below(count);
        for (index, flag) in flags.iter().enumerate() {
            if *flag {
                if k == 0 {
                    return Some(index);
                }
                k -= 1;
            }
        }
        None
    }

    /// Oracle state for one object: its counts (`None` once deleted) and whether the
    /// namespace still binds it.
    #[derive(Clone, Copy)]
    struct Model {
        counts: Option<RefCount>,
        bound: bool,
    }

    impl Model {
        /// Drop one pointer reference. Returns `true` if the object was deleted.
        fn release(&mut self) -> bool {
            let counts = self
                .counts
                .as_mut()
                .expect("released an object that was deleted");
            let deleted = counts.release().expect("pointer count underflowed");
            if deleted {
                self.counts = None;
            }
            deleted
        }
    }

    #[test]
    fn random_open_close_unbind_keeps_counts_exact_and_frees_once() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let mut table: HandleTable<usize, SLOTS> = HandleTable::new();
        let mut model = [Model {
            counts: Some(RefCount::new()),
            bound: true,
        }; OBJECTS];
        let mut held: [Option<(Handle, usize)>; SLOTS] = [None; SLOTS];
        let mut dead: [Option<Handle>; 32] = [None; 32];
        let mut dead_next = 0usize;
        let mut created = OBJECTS as u64;
        let mut deleted = 0u64;

        for _ in 0..STEPS {
            match rng.below(5) {
                // Open a handle to a live object.
                0 => {
                    let live: [bool; OBJECTS] = core::array::from_fn(|i| model[i].counts.is_some());
                    if let Some(obj) = pick(&mut rng, &live) {
                        match table.insert(obj, ACCESS_ALL, false) {
                            Ok(handle) => {
                                let slot = held.iter_mut().find(|s| s.is_none()).unwrap();
                                *slot = Some((handle, obj));
                                model[obj].counts.as_mut().unwrap().add_handle();
                            }
                            Err(error) => {
                                assert_eq!(error, HandleError::TableFull);
                                assert!(held.iter().all(Option::is_some));
                            }
                        }
                    }
                }
                // Close a held handle: drop the handle reference, then its pointer.
                1 => {
                    let used: [bool; SLOTS] = core::array::from_fn(|i| held[i].is_some());
                    if let Some(slot) = pick(&mut rng, &used) {
                        let (handle, obj) = held[slot].take().unwrap();
                        assert_eq!(table.close(handle).unwrap().object, obj);
                        dead[dead_next] = Some(handle);
                        dead_next = (dead_next + 1) % dead.len();
                        model[obj].counts.as_mut().unwrap().drop_handle().unwrap();
                        if model[obj].release() {
                            deleted += 1;
                        }
                    }
                }
                // Remove a namespace binding: drop the namespace's pointer reference.
                2 => {
                    let bound: [bool; OBJECTS] = core::array::from_fn(|i| model[i].bound);
                    if let Some(obj) = pick(&mut rng, &bound) {
                        model[obj].bound = false;
                        if model[obj].release() {
                            deleted += 1;
                        }
                    }
                }
                // Look up a held handle.
                3 => {
                    let used: [bool; SLOTS] = core::array::from_fn(|i| held[i].is_some());
                    if let Some(slot) = pick(&mut rng, &used) {
                        let (handle, obj) = held[slot].unwrap();
                        assert_eq!(table.get(handle).unwrap().object, obj);
                    }
                }
                // Create a new object in a deleted object's place.
                _ => {
                    let gone: [bool; OBJECTS] = core::array::from_fn(|i| model[i].counts.is_none());
                    if let Some(obj) = pick(&mut rng, &gone) {
                        model[obj] = Model {
                            counts: Some(RefCount::new()),
                            bound: true,
                        };
                        created += 1;
                    }
                }
            }

            // Every handle closed so far stays dead, even after its slot is reused.
            for handle in dead.iter().flatten() {
                assert_eq!(table.get(*handle).err(), Some(HandleError::Invalid));
            }
            // The counts agree exactly with what the oracle holds.
            let mut held_per_object = [0u32; OBJECTS];
            for (_, obj) in held.iter().flatten() {
                held_per_object[*obj] += 1;
            }
            for (obj, state) in model.iter().enumerate() {
                match state.counts {
                    Some(counts) => {
                        assert_eq!(counts.handles(), held_per_object[obj]);
                        assert_eq!(
                            counts.pointers(),
                            held_per_object[obj] + u32::from(state.bound)
                        );
                    }
                    None => {
                        // Deleted exactly once: no handle and no binding can still name it.
                        assert_eq!(held_per_object[obj], 0);
                        assert!(!state.bound);
                    }
                }
            }
            let live = model.iter().filter(|m| m.counts.is_some()).count() as u64;
            assert_eq!(created - deleted, live);
            assert_eq!(table.len(), held.iter().flatten().count());
        }
    }
}
