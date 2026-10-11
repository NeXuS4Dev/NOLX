//! The handle table.
//!
//! A handle is an opaque 32-bit value a process holds in place of a kernel pointer. Its
//! layout:
//!
//! ```text
//!  31            16 15              2  1  0
//! +----------------+-----------------+---+
//! |   generation   | slot + 1        | 0 0|
//! +----------------+-----------------+---+
//! ```
//!
//! * The low two bits are always zero, so a handle is never a valid pointer-looking value
//!   and the low bits can be used by callers that want tag bits later.
//! * `slot + 1` is stored so that the zero value is always the null handle.
//! * The generation is bumped every time a slot is freed and reused. A handle that outlived
//!   its entry therefore fails the generation check instead of silently naming whatever now
//!   occupies the slot -- the classic handle-reuse bug this layout exists to prevent.
//!
//! Generation zero is never used, so a handle built from a zeroed slot is always invalid.

/// Bits of the handle value that hold `slot + 1`.
const SLOT_MASK: u32 = 0x3FFF;
/// Shift to the slot field.
const SLOT_SHIFT: u32 = 2;
/// Shift to the generation field.
const GENERATION_SHIFT: u32 = 16;

/// The most entries a table can hold. The slot field is 14 bits wide and `slot + 1` must fit
/// in it, so the largest index is `MAX_ENTRIES - 1`.
pub const MAX_ENTRIES: usize = SLOT_MASK as usize;

/// The access bit that means "every access the object supports". A duplicate with this
/// access is granted exactly the source's access rather than a subset.
pub const ACCESS_ALL: u32 = u32::MAX;

/// An opaque handle value. `Handle::NULL` never refers to an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Handle(u32);

impl Handle {
    /// The null handle.
    pub const NULL: Handle = Handle(0);

    /// The raw value, for handing to a user process or a debugger.
    #[must_use]
    pub const fn raw(self) -> u32 {
        self.0
    }

    /// Reconstruct a handle from a raw value received from a user process. Validity is not
    /// checked here; [`HandleTable::get`] and friends do that.
    #[must_use]
    pub const fn from_raw(value: u32) -> Self {
        Handle(value)
    }

    fn encode(slot: usize, generation: u16) -> Self {
        // `slot < MAX_ENTRIES`, so `slot + 1` fits in the 14-bit field.
        let slot_field = (slot as u32 + 1) & SLOT_MASK;
        Handle((u32::from(generation) << GENERATION_SHIFT) | (slot_field << SLOT_SHIFT))
    }

    /// Decode into `(slot, generation)`, or `None` if the low bits are set or the slot field
    /// is zero.
    fn decode(self) -> Option<(usize, u16)> {
        if self.0 & 0b11 != 0 {
            return None;
        }
        let slot_field = (self.0 >> SLOT_SHIFT) & SLOT_MASK;
        if slot_field == 0 {
            return None;
        }
        let generation = (self.0 >> GENERATION_SHIFT) as u16;
        if generation == 0 {
            return None;
        }
        Some((slot_field as usize - 1, generation))
    }
}

/// Why a handle operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleError {
    /// The handle is null, malformed, out of range, closed, or stale.
    Invalid,
    /// Every slot is in use.
    TableFull,
    /// The requested access is not a subset of the access the source handle holds.
    AccessDenied,
}

/// The per-process table entry: the object pointer (or any payload `T`) plus the rights and
/// the inheritance flag the handle was created with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HandleEntry<T> {
    /// What the handle refers to. For the kernel this is an object pointer whose reference
    /// count the table's owner is responsible for.
    pub object: T,
    /// The access mask granted to this handle.
    pub access: u32,
    /// Whether a child process created with inheritance receives a copy of this handle.
    pub inheritable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Slot<T> {
    /// Generation of the entry currently in the slot, or of the last one freed here.
    generation: u16,
    entry: Option<HandleEntry<T>>,
}

/// A fixed-capacity handle table holding up to `N` entries.
///
/// Fixed capacity is deliberate: the table is part of a process, and a process's handle
/// quota is a number the object manager charges, not a `Vec` that grows until the nonpaged
/// pool is gone. `N` must be at most [`MAX_ENTRIES`].
#[derive(Debug)]
pub struct HandleTable<T, const N: usize> {
    slots: [Slot<T>; N],
    in_use: usize,
}

impl<T, const N: usize> Default for HandleTable<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> HandleTable<T, N> {
    /// An empty table.
    ///
    /// # Panics
    ///
    /// If `N` exceeds [`MAX_ENTRIES`]. That is a compile-time configuration error, and the
    /// first call to any constructor reports it.
    #[must_use]
    pub fn new() -> Self {
        assert!(N <= MAX_ENTRIES, "handle table larger than the handle format allows");
        Self {
            slots: core::array::from_fn(|_| Slot {
                generation: 1,
                entry: None,
            }),
            in_use: 0,
        }
    }

    /// The number of live handles.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.in_use
    }

    /// `true` if no handles are live.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.in_use == 0
    }

    /// The capacity fixed at compile time.
    #[must_use]
    pub const fn capacity(&self) -> usize {
        N
    }

    /// Create a handle to `object` with `access`.
    ///
    /// # Errors
    ///
    /// [`HandleError::TableFull`] if every slot is in use.
    pub fn insert(
        &mut self,
        object: T,
        access: u32,
        inheritable: bool,
    ) -> Result<Handle, HandleError> {
        let slot = self
            .slots
            .iter()
            .position(|slot| slot.entry.is_none())
            .ok_or(HandleError::TableFull)?;
        let entry = &mut self.slots[slot];
        entry.entry = Some(HandleEntry {
            object,
            access,
            inheritable,
        });
        self.in_use += 1;
        Ok(Handle::encode(slot, entry.generation))
    }

    fn entry_index(&self, handle: Handle) -> Result<usize, HandleError> {
        let (slot, generation) = handle.decode().ok_or(HandleError::Invalid)?;
        let entry = self.slots.get(slot).ok_or(HandleError::Invalid)?;
        if entry.generation != generation || entry.entry.is_none() {
            return Err(HandleError::Invalid);
        }
        Ok(slot)
    }

    /// The entry a handle refers to.
    ///
    /// # Errors
    ///
    /// [`HandleError::Invalid`] for a null, malformed, closed, or stale handle.
    pub fn get(&self, handle: Handle) -> Result<&HandleEntry<T>, HandleError> {
        let slot = self.entry_index(handle)?;
        self.slots[slot].entry.as_ref().ok_or(HandleError::Invalid)
    }

    /// Mutable access to the entry a handle refers to.
    ///
    /// # Errors
    ///
    /// As [`HandleTable::get`].
    pub fn get_mut(&mut self, handle: Handle) -> Result<&mut HandleEntry<T>, HandleError> {
        let slot = self.entry_index(handle)?;
        self.slots[slot].entry.as_mut().ok_or(HandleError::Invalid)
    }

    /// Close a handle and return its entry, so the caller can drop the reference the handle
    /// implied. The slot's generation advances, so this handle and every copy of it is dead.
    ///
    /// # Errors
    ///
    /// [`HandleError::Invalid`] if the handle is not live.
    pub fn close(&mut self, handle: Handle) -> Result<HandleEntry<T>, HandleError> {
        let slot = self.entry_index(handle)?;
        let entry = &mut self.slots[slot];
        let taken = entry.entry.take().ok_or(HandleError::Invalid)?;
        // Skip generation zero on wrap, so a zero generation always means "never valid".
        entry.generation = match entry.generation.wrapping_add(1) {
            0 => 1,
            next => next,
        };
        self.in_use -= 1;
        Ok(taken)
    }

    /// Duplicate `source` (a handle in this table) into `target`, following
    /// `DuplicateHandle` rules.
    ///
    /// `access` is the access the new handle gets. [`ACCESS_ALL`] means "the same access as
    /// the source". Any other value must be a subset of the source's access: a duplicate can
    /// narrow rights, never widen them.
    ///
    /// The caller clones the object and retains its reference, because only the caller knows
    /// what a retain means for `T`. The `object` argument is that clone, and the handle is
    /// created in `target`. The source is left untouched.
    ///
    /// # Errors
    ///
    /// [`HandleError::Invalid`] if `source` is not live, [`HandleError::AccessDenied`] if the
    /// requested access widens the source's, and [`HandleError::TableFull`] if `target` has
    /// no free slot. On error, nothing is inserted.
    pub fn duplicate_into<const M: usize>(
        &self,
        source: Handle,
        target: &mut HandleTable<T, M>,
        object: T,
        access: u32,
        inheritable: bool,
    ) -> Result<Handle, HandleError> {
        let source_entry = self.get(source)?;
        let granted = if access == ACCESS_ALL {
            source_entry.access
        } else if access & !source_entry.access != 0 {
            return Err(HandleError::AccessDenied);
        } else {
            access
        };
        target.insert(object, granted, inheritable)
    }

    /// Build the table a child process starts with when it inherits from this one.
    ///
    /// Only inheritable entries are copied. The caller passes `clone_object`, which is where
    /// the caller retains the reference each copied handle now implies. Copied handles are
    /// placed in the child at the same values where possible, so a handle value the parent
    /// already gave out (for example through a command-line argument) still names the same
    /// object in the child.
    ///
    /// Returns the number of handles inherited.
    ///
    /// # Errors
    ///
    /// [`HandleError::TableFull`] if the child's table cannot hold every inheritable entry.
    /// Entries inherited before the failure stay in `child`; the caller tears the child down.
    pub fn inherit_into<const M: usize>(
        &self,
        child: &mut HandleTable<T, M>,
        mut clone_object: impl FnMut(&T) -> T,
    ) -> Result<usize, HandleError> {
        let mut inherited = 0;
        for (index, slot) in self.slots.iter().enumerate() {
            let Some(entry) = slot.entry.as_ref() else {
                continue;
            };
            if !entry.inheritable {
                continue;
            }
            let object = clone_object(&entry.object);
            // Place the copy in the same slot index if the child has it free, so the handle
            // value matches. Otherwise fall back to the first free slot.
            child.insert_at_or_first_free(
                index,
                slot.generation,
                object,
                entry.access,
                entry.inheritable,
            )?;
            inherited += 1;
        }
        Ok(inherited)
    }

    fn insert_at_or_first_free(
        &mut self,
        preferred: usize,
        parent_generation: u16,
        object: T,
        access: u32,
        inheritable: bool,
    ) -> Result<Handle, HandleError> {
        let slot = if preferred < N && self.slots[preferred].entry.is_none() {
            // Taking the parent's slot: adopt the parent's generation, so the value the parent
            // handed out is valid in the child too.
            self.slots[preferred].generation = parent_generation;
            preferred
        } else {
            // Fallback: keep this slot's own generation, so a stale handle that once named
            // this slot in the child stays dead.
            self.slots
                .iter()
                .position(|slot| slot.entry.is_none())
                .ok_or(HandleError::TableFull)?
        };
        let entry = &mut self.slots[slot];
        entry.entry = Some(HandleEntry {
            object,
            access,
            inheritable,
        });
        let generation = entry.generation;
        self.in_use += 1;
        Ok(Handle::encode(slot, generation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: u32 = 0x1F;
    const READ: u32 = 0x1;
    const WRITE: u32 = 0x2;

    #[test]
    fn null_and_malformed_handles_are_invalid() {
        let table: HandleTable<u32, 4> = HandleTable::new();
        assert_eq!(table.get(Handle::NULL), Err(HandleError::Invalid));
        // Low bits set.
        assert_eq!(table.get(Handle::from_raw(0x5)), Err(HandleError::Invalid));
        // Slot field zero with a generation set.
        assert_eq!(table.get(Handle::from_raw(0x0001_0000)), Err(HandleError::Invalid));
        // Slot past capacity.
        assert_eq!(table.get(Handle::from_raw(0x0001_0040)), Err(HandleError::Invalid));
    }

    #[test]
    fn insert_then_get_round_trips() {
        let mut table: HandleTable<&str, 4> = HandleTable::new();
        let h = table.insert("file", FULL, false).unwrap();
        assert_ne!(h, Handle::NULL);
        assert_eq!(h.raw() & 0b11, 0);
        assert_eq!(table.get(h).unwrap().object, "file");
        assert_eq!(table.get(h).unwrap().access, FULL);
        assert_eq!(table.len(), 1);
    }

    #[test]
    fn closed_handle_is_stale_after_slot_reuse() {
        let mut table: HandleTable<u32, 1> = HandleTable::new();
        let first = table.insert(1, FULL, false).unwrap();
        let taken = table.close(first).unwrap();
        assert_eq!(taken.object, 1);
        assert!(table.is_empty());
        let second = table.insert(2, FULL, false).unwrap();
        // Same slot, different generation: the old value must not name the new entry.
        assert_ne!(first, second);
        assert_eq!(table.get(first), Err(HandleError::Invalid));
        assert_eq!(table.close(first), Err(HandleError::Invalid));
        assert_eq!(table.get(second).unwrap().object, 2);
    }

    #[test]
    fn double_close_is_rejected_and_does_not_underflow() {
        let mut table: HandleTable<u32, 2> = HandleTable::new();
        let h = table.insert(7, FULL, false).unwrap();
        table.close(h).unwrap();
        assert_eq!(table.close(h), Err(HandleError::Invalid));
        assert_eq!(table.len(), 0);
    }

    #[test]
    fn table_full_is_reported_and_recovers_after_close() {
        let mut table: HandleTable<u32, 2> = HandleTable::new();
        let a = table.insert(1, FULL, false).unwrap();
        let _b = table.insert(2, FULL, false).unwrap();
        assert_eq!(table.insert(3, FULL, false), Err(HandleError::TableFull));
        table.close(a).unwrap();
        assert!(table.insert(3, FULL, false).is_ok());
    }

    #[test]
    fn generation_wraps_without_reaching_zero() {
        let mut table: HandleTable<u32, 1> = HandleTable::new();
        // Drive one slot through more reuses than the generation field can count without
        // ever producing a generation that decodes as invalid.
        for round in 0..70_000u32 {
            let h = table.insert(round, FULL, false).unwrap();
            assert_eq!(table.get(h).unwrap().object, round);
            table.close(h).unwrap();
        }
    }

    #[test]
    fn duplicate_may_narrow_but_not_widen() {
        let mut parent: HandleTable<u32, 4> = HandleTable::new();
        let mut child: HandleTable<u32, 4> = HandleTable::new();
        let source = parent.insert(9, READ, false).unwrap();

        let same = parent
            .duplicate_into(source, &mut child, 9, ACCESS_ALL, false)
            .unwrap();
        assert_eq!(child.get(same).unwrap().access, READ);

        let narrowed = parent
            .duplicate_into(source, &mut child, 9, 0, false)
            .unwrap();
        assert_eq!(child.get(narrowed).unwrap().access, 0);

        assert_eq!(
            parent.duplicate_into(source, &mut child, 9, READ | WRITE, false),
            Err(HandleError::AccessDenied)
        );
        // Failure inserted nothing.
        assert_eq!(child.len(), 2);
    }

    #[test]
    fn duplicate_of_dead_handle_fails_without_inserting() {
        let mut parent: HandleTable<u32, 4> = HandleTable::new();
        let mut child: HandleTable<u32, 4> = HandleTable::new();
        let source = parent.insert(1, FULL, false).unwrap();
        parent.close(source).unwrap();
        assert_eq!(
            parent.duplicate_into(source, &mut child, 1, ACCESS_ALL, false),
            Err(HandleError::Invalid)
        );
        assert!(child.is_empty());
    }

    #[test]
    fn duplicate_into_full_table_fails_cleanly() {
        let mut parent: HandleTable<u32, 4> = HandleTable::new();
        let mut child: HandleTable<u32, 1> = HandleTable::new();
        let source = parent.insert(1, FULL, false).unwrap();
        child.insert(0, FULL, false).unwrap();
        assert_eq!(
            parent.duplicate_into(source, &mut child, 1, ACCESS_ALL, false),
            Err(HandleError::TableFull)
        );
        assert_eq!(child.len(), 1);
    }

    #[test]
    fn inheritance_copies_only_inheritable_entries() {
        let mut parent: HandleTable<u32, 8> = HandleTable::new();
        let mut child: HandleTable<u32, 8> = HandleTable::new();
        let private = parent.insert(1, FULL, false).unwrap();
        let shared = parent.insert(2, READ, true).unwrap();

        let mut retained = 0;
        let n = parent
            .inherit_into(&mut child, |&object| {
                retained += 1;
                object
            })
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(retained, 1, "only the inheritable entry is retained");
        assert_eq!(child.len(), 1);
        // The inherited handle keeps its value, so the same number names the same object.
        assert_eq!(child.get(shared).unwrap().object, 2);
        assert_eq!(child.get(private), Err(HandleError::Invalid));
    }

    #[test]
    fn inheritance_falls_back_when_the_slot_is_taken() {
        let mut parent: HandleTable<u32, 4> = HandleTable::new();
        let mut child: HandleTable<u32, 4> = HandleTable::new();
        let shared = parent.insert(5, FULL, true).unwrap();
        // The child already occupies the slot the parent's entry would land in.
        child.insert(99, FULL, false).unwrap();
        let n = parent.inherit_into(&mut child, |&o| o).unwrap();
        assert_eq!(n, 1);
        assert_eq!(child.len(), 2);
        // The copy is in a different slot, so the parent's value does not name it: it names
        // the child's pre-existing entry, never the copy.
        assert_eq!(child.get(shared).unwrap().object, 99);
    }

    #[test]
    fn inheritance_reports_child_overflow() {
        let mut parent: HandleTable<u32, 4> = HandleTable::new();
        let mut child: HandleTable<u32, 1> = HandleTable::new();
        parent.insert(1, FULL, true).unwrap();
        parent.insert(2, FULL, true).unwrap();
        assert_eq!(parent.inherit_into(&mut child, |&o| o), Err(HandleError::TableFull));
    }

    #[test]
    fn get_mut_changes_access_in_place() {
        let mut table: HandleTable<u32, 2> = HandleTable::new();
        let h = table.insert(1, FULL, false).unwrap();
        table.get_mut(h).unwrap().access = READ;
        assert_eq!(table.get(h).unwrap().access, READ);
    }
}
