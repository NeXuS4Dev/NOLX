//! Memory-management primitives that are tested on an ordinary host.
//!
//! The memory manager is the subsystem where a mistake is least recoverable: a corrupted
//! free list is not a wrong answer, it is an allocator that hands the same frame to two
//! owners. The response in this project is the same one used for the HAL: keep the parts
//! that are *logic* in a crate with no hardware dependency, and test them here, where a
//! failure is a red test rather than a triple fault in the middle of boot.
//!
//! What lives here:
//!
//! * [`buddy`] -- the buddy allocator: power-of-two blocks, split on allocation, merged on
//!   free, with per-order free lists kept *inside* the free blocks through [`buddy::BlockStore`].
//! * [`magazine`] -- per-processor caches of single frames in front of the buddy allocator,
//!   plus the pool of frames already known to be zero.
//! * [`paging`] -- four-level page-table arithmetic, entry encoding, and a walker that maps
//!   and unmaps pages through [`paging::TableStore`].
//! * [`vad`] -- the virtual address descriptor tree: an AVL tree of mapped regions with
//!   split, merge, and coalesce, ordered by start address.
//! * [`commit`] -- commit charge and pool accounting: what `RLIMIT_AS`, `RLIMIT_DATA`, and
//!   `getrusage` are read from, so they cannot disagree with each other.
//! * [`heap`] -- a boundary-tagged heap with size-class free lists, which is what the
//!   kernel heap is built from.
//! * [`numa`] -- the node list and the zonelists that order it: which memory an allocation
//!   may come from, and in what order.
//!
//! Every module is `no_std` and free of hardware access. The kernel supplies the physical
//! memory, the page tables, and the locks; these modules supply the algorithms, and the
//! traits they use to reach that memory are small enough to implement over a `Vec` in a
//! test.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod buddy;
pub mod commit;
pub mod heap;
pub mod magazine;
pub mod mmio;
pub mod numa;
pub mod paging;
pub mod vad;

/// Bytes in a 4 KiB page. The only page size the kernel uses for mappings it controls.
pub const PAGE_SIZE: usize = 4096;

/// Bits in a page offset.
pub const PAGE_SHIFT: u32 = 12;

/// Mask selecting the offset within a page.
pub const PAGE_MASK: u64 = PAGE_SIZE as u64 - 1;

/// Round an address up to a page boundary.
#[must_use]
pub const fn page_align_up(address: u64) -> u64 {
    (address + PAGE_MASK) & !PAGE_MASK
}

/// Round an address down to a page boundary.
#[must_use]
pub const fn page_align_down(address: u64) -> u64 {
    address & !PAGE_MASK
}

/// The number of pages needed to cover `bytes`, or `None` if the addition overflows.
#[must_use]
pub const fn pages_for(bytes: u64) -> Option<u64> {
    match bytes.checked_add(PAGE_MASK) {
        Some(total) => Some(total >> PAGE_SHIFT),
        None => None,
    }
}

/// A deterministic pseudo-random generator for tests.
///
/// Property tests need varied input, and a *fixed* seed is the point: a failing case is
/// reproducible from the seed alone. This is xorshift64*, which is not a good generator for
/// anything except being small, fast, and identical on every machine.
#[cfg(test)]
pub(crate) mod test_rng {
    /// xorshift64* state.
    pub struct Rng(u64);

    impl Rng {
        /// A generator with the given seed. Seed zero is remapped, because xorshift is
        /// stuck at zero forever.
        #[must_use]
        pub const fn new(seed: u64) -> Self {
            Self(if seed == 0 {
                0x9E37_79B9_7F4A_7C15
            } else {
                seed
            })
        }

        /// The next value.
        pub fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }

        /// A value in `0..bound`. Panics if `bound` is zero.
        pub fn below(&mut self, bound: u64) -> u64 {
            assert!(bound > 0);
            self.next_u64() % bound
        }

        /// True with probability `percent` in 100.
        pub fn chance(&mut self, percent: u64) -> bool {
            self.below(100) < percent
        }
    }
}
