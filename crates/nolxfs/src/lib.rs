//! NOLXFS: the native NOLX filesystem.
//!
//! Copy-on-write, transactional, per-object checksummed, snapshot capable. The
//! architecture is shaped like APFS -- container, checkpoint ring, object map, typed
//! B-trees -- because that shape solves the right problems, but every magic number, type
//! code, and layout here is NOLX's own. Nothing reads or writes an APFS volume.
//!
//! See `docs/fs/nolxfs-format.md` for the format and
//! `tools/nolxfs-codegen/nolxfs_format.py` for the machine-readable declaration that
//! generates [`on_disk_gen`].
//!
//! # Crate layout
//!
//! * [`on_disk`] -- the generated structures plus the typed accessors over them.
//! * [`fletcher`] -- the checksum used by every on-disk object.
//! * [`blockdev`] -- the device abstraction and an in-memory implementation for tests.
//! * [`btree`] -- the B-tree every index in the filesystem is built from.
//! * [`omap`] -- the object map: `(oid, xid)` to physical block.
//! * [`mkfs`] -- formatting a device, shared by the kernel and `mkfs.nolxfs`.
//! * [`spaceman`] -- the space manager (chunk-info blocks, bitmaps, reaper).
//! * [`checkpoint`] -- the checkpoint ring: atomic commit of volume state.
//! * [`volume`] -- the mounted filesystem itself: files, directories, links,
//!   xattrs, snapshots.
//! * [`fsck`] / [`dump`] -- the offline checker and the read-only dumper behind
//!   `fsck.nolxfs` and `nolxfs-dump`.
//!
//! # Testing
//!
//! Every module here runs on the host against [`blockdev::MemBlockDevice`], which is how
//! the filesystem gets real test coverage without booting anything. The crash-recovery
//! tests in [`mkfs`] inject failures at every write boundary.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

extern crate alloc;

pub mod blockdev;
pub mod btree;
pub mod checkpoint;
pub mod dump;
pub mod fletcher;
pub mod fsck;
pub mod mkfs;
pub mod omap;
pub mod on_disk;
pub mod spaceman;
pub mod volume;

pub mod on_disk_gen;

pub use crate::blockdev::{BlockDevice, DevError};
pub use crate::on_disk::{ObjHeader, ObjRef};

/// Marker for a plain-old-data on-disk structure.
///
/// # Safety
///
/// Implementors must be `#[repr(C)]`, contain only integers and integer arrays, have no
/// padding whose value matters, and accept every bit pattern as a valid value. The
/// generated structures in [`on_disk_gen`] satisfy this by construction -- they contain
/// nothing but `u8`/`u16`/`u32`/`u64`/`i64` fields, and any implicit padding is turned
/// into an explicit `_reserved` field by the generator, which asserts the layout has no
/// gaps.
pub unsafe trait Pod: Copy + 'static {
    /// A zero-initialised instance.
    #[must_use]
    fn zeroed() -> Self {
        // SAFETY: `Pod` guarantees every bit pattern is a valid value, so an all-zero
        // buffer is a valid `Self`. `MaybeUninit::zeroed` initialises the bytes before
        // `assume_init` reads them, and `Self: Copy` means no destructor is skipped.
        unsafe { core::mem::MaybeUninit::<Self>::zeroed().assume_init() }
    }

    /// The structure's bytes, for checksumming and writing.
    #[must_use]
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `Self` is `Copy` and `Pod`, so reinterpreting its bytes cannot produce
        // an invalid value, and `size_of::<Self>()` is exactly the number of bytes the
        // reference covers.
        unsafe {
            core::slice::from_raw_parts(
                core::ptr::from_ref(self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    /// The structure's bytes, mutably.
    #[must_use]
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as [`Pod::as_bytes`], plus the mutable reference is exclusive, so the
        // returned slice aliases nothing else.
        unsafe {
            core::slice::from_raw_parts_mut(
                core::ptr::from_mut(self).cast::<u8>(),
                core::mem::size_of::<Self>(),
            )
        }
    }

    /// Borrow `bytes` as a `&Self`, checking length and alignment.
    ///
    /// Returns `None` rather than reading past the buffer or producing a misaligned
    /// reference, which is what makes parsing an untrusted image safe.
    #[must_use]
    fn from_bytes(bytes: &[u8]) -> Option<&Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let ptr = bytes.as_ptr().cast::<Self>();
        if ptr.align_offset(core::mem::align_of::<Self>()) != 0 {
            return None;
        }
        // SAFETY: the length and alignment checks above hold, and `Pod` guarantees every
        // bit pattern is valid, so the borrow cannot be UB whatever the bytes contain.
        Some(unsafe { &*ptr })
    }

    /// Copy `bytes` into an owned `Self`, checking length.
    ///
    /// This is the form to use on a device buffer that is not suitably aligned, which is
    /// the common case when a block was read into a generic scratch buffer.
    #[must_use]
    fn read_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let mut out = Self::zeroed();
        out.as_bytes_mut()
            .copy_from_slice(&bytes[..core::mem::size_of::<Self>()]);
        Some(out)
    }
}

/// The size of every field in a `Pod` type must equal its declared on-disk size. Called
/// from each module's tests so a layout change cannot silently desynchronise the Rust
/// structures from the generated ones.
#[cfg(test)]
pub(crate) fn assert_pod_size<T: Pod>(expected: usize, name: &str) {
    assert_eq!(
        core::mem::size_of::<T>(),
        expected,
        "{name}: size_of disagrees with the generated DISK_SIZE"
    );
}
