//! Typed access to the generated on-disk structures.
//!
//! [`crate::on_disk_gen`] is generated and deliberately dumb: plain `#[repr(C)]` structs
//! with no logic. Everything that interprets them lives here, so there is exactly one
//! place that knows what a checksum covers, how a j-key is packed, or which magic belongs
//! to which object.

use core::fmt;

use crate::fletcher;
use crate::on_disk_gen as gen;
use crate::Pod;

/// A filesystem-level error: the bytes are readable but they do not describe a valid
/// NOLXFS.
///
/// A malformed image must never panic the kernel. Every parse path returns one of these,
/// and the mount path refuses the volume. The fuzzing requirement in ROADMAP M13 is that
/// no input reaches a `panic!`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsError {
    /// The object's Fletcher-64 checksum did not match.
    BadChecksum {
        /// The object id whose checksum failed, if it could be read.
        oid: u64,
    },
    /// The object's type is not the one the reader expected.
    BadType {
        /// What was found.
        found: u32,
        /// What was wanted.
        expected: u32,
    },
    /// A magic number did not match.
    BadMagic {
        /// What was found.
        found: u32,
        /// What was wanted.
        expected: u32,
    },
    /// The buffer is too short to hold the structure.
    TooShort {
        /// Bytes available.
        have: usize,
        /// Bytes needed.
        need: usize,
    },
    /// A field holds a value the format forbids.
    InvalidField(&'static str),
    /// A block index is past the end of the container.
    BlockOutOfRange(u64),
    /// The volume requires a feature this reader does not have.
    UnsupportedFeature(u64),
    /// A B-tree node is internally inconsistent.
    CorruptNode(&'static str),
    /// The operation cannot be done on a read-only mount.
    ReadOnly,
    /// The device failed.
    Device(crate::DevError),
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadChecksum { oid } => write!(f, "object {oid:#x} failed its checksum"),
            Self::BadType { found, expected } => {
                write!(f, "object type {found:#x}, expected {expected:#x}")
            }
            Self::BadMagic { found, expected } => {
                write!(f, "magic {found:#010x}, expected {expected:#010x}")
            }
            Self::TooShort { have, need } => write!(f, "need {need} bytes, have {have}"),
            Self::InvalidField(what) => write!(f, "invalid field: {what}"),
            Self::BlockOutOfRange(b) => write!(f, "block {b} is past the end of the volume"),
            Self::UnsupportedFeature(bits) => write!(f, "unsupported feature bits {bits:#x}"),
            Self::CorruptNode(what) => write!(f, "corrupt B-tree node: {what}"),
            Self::ReadOnly => write!(f, "volume is mounted read-only"),
            Self::Device(e) => write!(f, "device error: {e:?}"),
        }
    }
}

impl From<crate::DevError> for FsError {
    fn from(value: crate::DevError) -> Self {
        Self::Device(value)
    }
}

/// Convenience alias used throughout the crate.
pub type Result<T> = core::result::Result<T, FsError>;

/// The common object header, with the interpretation the format gives it.
#[derive(Clone, Copy)]
pub struct ObjHeader(gen::nolx_obj_phys_t);

impl ObjHeader {
    /// Wrap a raw header without validating it.
    #[must_use]
    pub const fn new(raw: gen::nolx_obj_phys_t) -> Self {
        Self(raw)
    }

    /// A header for a new object of `obj_type`/`subtype` at `xid`.
    #[must_use]
    pub const fn make(oid: u64, xid: u64, obj_type: u32, subtype: u16) -> Self {
        Self(gen::nolx_obj_phys_t {
            checksum: 0,
            oid,
            xid,
            r#type: obj_type,
            subtype,
            flags: 0,
        })
    }

    /// The stored checksum.
    #[must_use]
    pub const fn checksum(&self) -> u64 {
        self.0.checksum
    }
    /// The object id.
    #[must_use]
    pub const fn oid(&self) -> u64 {
        self.0.oid
    }
    /// The transaction id of the version that created this object.
    #[must_use]
    pub const fn xid(&self) -> u64 {
        self.0.xid
    }
    /// The object type.
    #[must_use]
    pub const fn obj_type(&self) -> u32 {
        self.0.r#type
    }
    /// The object subtype (the role of a B-tree).
    #[must_use]
    pub const fn subtype(&self) -> u16 {
        self.0.subtype
    }
    /// The object flags.
    #[must_use]
    pub const fn flags(&self) -> u16 {
        self.0.flags
    }
    /// `true` if the object changes during mount and is pinned by the checkpoint map.
    #[must_use]
    pub const fn is_ephemeral(&self) -> bool {
        self.0.flags & (gen::OBJ_FLAG_EPHEMERAL as u16) != 0
    }

    /// The raw structure, for writing back.
    #[must_use]
    pub const fn raw(&self) -> gen::nolx_obj_phys_t {
        self.0
    }
}

impl fmt::Debug for ObjHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObjHeader")
            .field("oid", &self.0.oid)
            .field("xid", &self.0.xid)
            .field("type", &self.0.r#type)
            .field("subtype", &self.0.subtype)
            .field("flags", &self.0.flags)
            .finish()
    }
}

/// A parsed reference to an on-disk object: its header plus the body that follows.
///
/// This is what a reader works with. It borrows the block buffer, so it cannot outlive
/// the read.
#[derive(Debug)]
pub struct ObjRef<'a> {
    header: ObjHeader,
    body: &'a [u8],
}

impl<'a> ObjRef<'a> {
    /// Parse and validate an object from `block`.
    ///
    /// Checks the checksum, then the type. `expected_type` is what the reader was told to
    /// expect by whatever pointed here; a mismatch means the object map, a B-tree pointer,
    /// or the disk itself is wrong, and continuing would risk interpreting one structure
    /// as another.
    ///
    /// # Errors
    ///
    /// [`FsError::TooShort`], [`FsError::BadChecksum`], or [`FsError::BadType`].
    pub fn parse(block: &'a [u8], expected_type: u32) -> Result<Self> {
        // `read_from` copies the header out, so an unaligned scratch buffer is fine. The
        // body stays a borrow because `u8` has no alignment requirement.
        let header = gen::nolx_obj_phys_t::read_from(block).ok_or(FsError::TooShort {
            have: block.len(),
            need: gen::nolx_obj_phys_t::DISK_SIZE,
        })?;

        if !fletcher::verify(block, header.checksum) {
            return Err(FsError::BadChecksum { oid: header.oid });
        }
        if header.r#type != expected_type {
            return Err(FsError::BadType {
                found: header.r#type,
                expected: expected_type,
            });
        }

        Ok(Self {
            header: ObjHeader::new(header),
            body: &block[gen::nolx_obj_phys_t::DISK_SIZE..],
        })
    }

    /// The header.
    #[must_use]
    pub const fn header(&self) -> &ObjHeader {
        &self.header
    }

    /// The bytes after the header.
    #[must_use]
    pub const fn body(&self) -> &'a [u8] {
        self.body
    }
}

/// Fill in a structure's header and checksum it, ready to write.
///
/// `object` must be a fixed-size object whose first `nolx_obj_phys_t::DISK_SIZE` bytes are
/// the header. The function writes the header, zeroes the checksum field, computes the
/// Fletcher-64 over the whole object, and stores it.
///
/// # Errors
///
/// [`FsError::TooShort`] if `object` is shorter than the header.
pub fn seal(object: &mut [u8], header: ObjHeader) -> Result<()> {
    if object.len() < gen::nolx_obj_phys_t::DISK_SIZE {
        return Err(FsError::TooShort {
            have: object.len(),
            need: gen::nolx_obj_phys_t::DISK_SIZE,
        });
    }
    let mut raw = header.raw();
    raw.checksum = 0;
    object[..gen::nolx_obj_phys_t::DISK_SIZE].copy_from_slice(raw.as_bytes());
    let sum = fletcher::compute(object).ok_or(FsError::InvalidField(
        "object length is not a multiple of four",
    ))?;
    object[..8].copy_from_slice(&sum.to_le_bytes());
    Ok(())
}

// --------------------------------------------------------------------- j-keys

/// Number of bits an inode number occupies inside a j-key.
pub const JKEY_OID_BITS: u32 = 60;
/// Mask selecting the inode number from a j-key.
pub const JKEY_OID_MASK: u64 = (1u64 << JKEY_OID_BITS) - 1;

/// Pack a record type and an inode number into a j-key.
///
/// The record type occupies the top four bits so that all records for one inode sort
/// together, and within an inode the record types are ordered by their numeric value.
///
/// # Panics
///
/// Panics in debug builds if `record_type` needs more than four bits or `oid` needs more
/// than [`JKEY_OID_BITS`]. Both are format violations, not runtime conditions.
#[must_use]
pub const fn pack_jkey(record_type: u32, oid: u64) -> u64 {
    debug_assert!(record_type < 16, "record type must fit in four bits");
    debug_assert!(oid <= JKEY_OID_MASK, "inode number exceeds the j-key field");
    ((record_type as u64) << JKEY_OID_BITS) | (oid & JKEY_OID_MASK)
}

/// The record type from a j-key.
#[must_use]
pub const fn jkey_type(key: u64) -> u32 {
    (key >> JKEY_OID_BITS) as u32
}

/// The inode number from a j-key.
#[must_use]
pub const fn jkey_oid(key: u64) -> u64 {
    key & JKEY_OID_MASK
}

/// Split a packed extent length: the low 56 bits are a byte count, the high 8 are flags.
///
/// # Panics
///
/// Panics in debug builds if `len` exceeds the 56-bit field.
#[must_use]
pub const fn pack_len_and_flags(len: u64, flags: u8) -> u64 {
    debug_assert!(len <= gen::MAX_EXTENT_LEN, "extent length exceeds 56 bits");
    (len & gen::MAX_EXTENT_LEN) | ((flags as u64) << 56)
}

/// The byte count from a packed `len_and_flags`/`len_and_kind` field.
#[must_use]
pub const fn extent_len(packed: u64) -> u64 {
    packed & gen::MAX_EXTENT_LEN
}

/// The flags or kind from a packed `len_and_flags`/`len_and_kind` field.
#[must_use]
pub const fn extent_flags(packed: u64) -> u8 {
    (packed >> 56) as u8
}

// --------------------------------------------------------------------- validation

/// `true` if `block_size` is a legal NOLXFS block size: a power of two between
/// [`gen::MIN_BLOCK_SIZE`] and [`gen::MAX_BLOCK_SIZE`].
#[must_use]
pub const fn is_valid_block_size(block_size: u32) -> bool {
    block_size.is_power_of_two()
        && block_size as u64 >= gen::MIN_BLOCK_SIZE
        && block_size as u64 <= gen::MAX_BLOCK_SIZE
}

/// Validate the parts of a container superblock that a reader must not trust.
///
/// # Errors
///
/// Returns the first problem found. A caller should report it and refuse to mount rather
/// than attempt a repair; `fsck.nolxfs` is the repair tool.
pub fn validate_container(sb: &gen::nolx_nx_superblock_t) -> Result<()> {
    if sb.magic != gen::MAGIC_CONTAINER_SUPERBLOCK {
        return Err(FsError::BadMagic {
            found: sb.magic,
            expected: gen::MAGIC_CONTAINER_SUPERBLOCK,
        });
    }
    if !is_valid_block_size(sb.block_size) {
        return Err(FsError::InvalidField("block_size"));
    }
    if sb.block_count == 0 {
        return Err(FsError::InvalidField("block_count is zero"));
    }
    if sb.fs_oid_count as u64 > gen::MAX_VOLUMES_PER_CONTAINER {
        return Err(FsError::InvalidField(
            "fs_oid_count exceeds the container limit",
        ));
    }
    let unsupported = sb.features_required & !supported_features();
    if unsupported != 0 {
        return Err(FsError::UnsupportedFeature(unsupported));
    }
    // The checkpoint rings must lie inside the container, or reading them reads past the
    // end of the device.
    let desc_end = sb
        .xp_desc_base
        .checked_add(u64::from(sb.xp_desc_blocks))
        .ok_or(FsError::InvalidField("xp_desc_base overflows"))?;
    let data_end = sb
        .xp_data_base
        .checked_add(u64::from(sb.xp_data_blocks))
        .ok_or(FsError::InvalidField("xp_data_base overflows"))?;
    if desc_end > sb.block_count || data_end > sb.block_count {
        return Err(FsError::BlockOutOfRange(sb.block_count));
    }
    Ok(())
}

/// Validate a volume superblock.
///
/// # Errors
///
/// Returns the first problem found.
pub fn validate_volume(sb: &gen::nolx_fs_superblock_t) -> Result<()> {
    if sb.magic != gen::MAGIC_VOLUME_SUPERBLOCK {
        return Err(FsError::BadMagic {
            found: sb.magic,
            expected: gen::MAGIC_VOLUME_SUPERBLOCK,
        });
    }
    if !is_valid_block_size(sb.block_size) {
        return Err(FsError::InvalidField("block_size"));
    }
    let unsupported = sb.features_required & !supported_features();
    if unsupported != 0 {
        return Err(FsError::UnsupportedFeature(unsupported));
    }
    if sb.root_tree_oid == 0 || sb.omap_oid == 0 {
        return Err(FsError::InvalidField(
            "the volume has no fs-tree or object map",
        ));
    }
    let name = &sb.volume_name[..];
    let len = name.iter().position(|&b| b == 0).unwrap_or(name.len());
    if len > gen::MAX_VOLUME_NAME_LEN as usize {
        return Err(FsError::InvalidField("volume name is too long"));
    }
    if core::str::from_utf8(&name[..len]).is_err() {
        return Err(FsError::InvalidField("volume name is not valid UTF-8"));
    }
    Ok(())
}

/// The feature bits this reader understands.
///
/// Bumping this is a deliberate act: it says the code can actually handle the feature.
#[must_use]
pub const fn supported_features() -> u64 {
    gen::NX_FEATURE_REQ_V1_BASE as u64
}

/// Decode a `d_name` field from a directory entry buffer.
///
/// # Errors
///
/// [`FsError::InvalidField`] if the name is longer than the buffer, contains a NUL before
/// its declared end, or is not valid UTF-8. POSIX does not require names to be UTF-8, but
/// NOLXFS stores them that way and `mkfs` enforces it, so a non-UTF-8 name means
/// corruption.
pub fn decode_name(buffer: &[u8], declared_len: usize) -> Result<&str> {
    if declared_len > buffer.len() {
        return Err(FsError::InvalidField("name length exceeds the record"));
    }
    if declared_len > gen::MAX_NAME_LEN as usize {
        return Err(FsError::InvalidField("name exceeds NAME_MAX"));
    }
    let name = &buffer[..declared_len];
    if name.contains(&0) {
        return Err(FsError::InvalidField("name contains an embedded NUL"));
    }
    core::str::from_utf8(name).map_err(|_| FsError::InvalidField("name is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn pod_sizes_match_the_generated_declarations() {
        crate::assert_pod_size::<gen::nolx_obj_phys_t>(32, "nolx_obj_phys_t");
        crate::assert_pod_size::<gen::nolx_nx_superblock_t>(4096, "nolx_nx_superblock_t");
        crate::assert_pod_size::<gen::nolx_fs_superblock_t>(4096, "nolx_fs_superblock_t");
        crate::assert_pod_size::<gen::nolx_omap_phys_t>(4096, "nolx_omap_phys_t");
        crate::assert_pod_size::<gen::nolx_omap_key_t>(16, "nolx_omap_key_t");
        crate::assert_pod_size::<gen::nolx_omap_val_t>(16, "nolx_omap_val_t");
        crate::assert_pod_size::<gen::nolx_inode_t>(128, "nolx_inode_t");
    }

    #[test]
    fn jkey_round_trips() {
        for (rtype, oid) in [(1u32, 1u64), (3, 1024), (9, JKEY_OID_MASK), (12, 7)] {
            let key = pack_jkey(rtype, oid);
            assert_eq!(jkey_type(key), rtype);
            assert_eq!(jkey_oid(key), oid);
        }
    }

    #[test]
    fn jkey_orders_by_inode_then_record_type() {
        // Records for one inode must sort together, which is what makes a directory
        // listing a range scan.
        let inode_a_dir = pack_jkey(gen::DREC_TYPE_DIR_REC, 10);
        let inode_a_ino = pack_jkey(gen::DREC_TYPE_INODE, 10);
        let inode_b_ino = pack_jkey(gen::DREC_TYPE_INODE, 11);
        assert!(inode_a_dir < inode_a_ino);
        assert!(inode_a_ino < inode_b_ino);
    }

    #[test]
    #[should_panic(expected = "four bits")]
    fn jkey_rejects_an_oversized_record_type() {
        let _ = pack_jkey(16, 1);
    }

    #[test]
    fn extent_length_round_trips() {
        let packed = pack_len_and_flags(1 << 40, 0x02);
        assert_eq!(extent_len(packed), 1 << 40);
        assert_eq!(extent_flags(packed), 0x02);
        assert_eq!(
            extent_len(pack_len_and_flags(gen::MAX_EXTENT_LEN, 0)),
            gen::MAX_EXTENT_LEN
        );
    }

    #[test]
    fn block_size_validation_rejects_nonsense() {
        assert!(is_valid_block_size(4096));
        assert!(is_valid_block_size(1024));
        assert!(is_valid_block_size(16384));
        assert!(!is_valid_block_size(0));
        assert!(!is_valid_block_size(512), "below the format minimum");
        assert!(!is_valid_block_size(32768), "above the format maximum");
        assert!(!is_valid_block_size(4000), "not a power of two");
    }

    fn good_container() -> gen::nolx_nx_superblock_t {
        let mut sb = gen::nolx_nx_superblock_t::zeroed();
        sb.magic = gen::MAGIC_CONTAINER_SUPERBLOCK;
        sb.block_size = 4096;
        sb.block_count = 1_000_000;
        sb.features_required = u64::from(gen::NX_FEATURE_REQ_V1_BASE);
        sb.xp_desc_base = 1;
        sb.xp_desc_blocks = 32;
        sb.xp_data_base = 33;
        sb.xp_data_blocks = 32;
        sb
    }

    #[test]
    fn a_well_formed_container_superblock_validates() {
        assert_eq!(validate_container(&good_container()), Ok(()));
    }

    #[test]
    fn container_validation_catches_each_bad_field() {
        let mut sb = good_container();
        sb.magic = 0xDEAD_BEEF;
        assert!(matches!(
            validate_container(&sb),
            Err(FsError::BadMagic { .. })
        ));

        let mut sb = good_container();
        sb.block_size = 4000;
        assert_eq!(
            validate_container(&sb),
            Err(FsError::InvalidField("block_size"))
        );

        let mut sb = good_container();
        sb.block_count = 0;
        assert!(matches!(
            validate_container(&sb),
            Err(FsError::InvalidField("block_count is zero"))
        ));

        let mut sb = good_container();
        sb.fs_oid_count = 9;
        assert!(matches!(
            validate_container(&sb),
            Err(FsError::InvalidField(_))
        ));

        let mut sb = good_container();
        sb.features_required = 1 << 40;
        assert_eq!(
            validate_container(&sb),
            Err(FsError::UnsupportedFeature(1 << 40))
        );

        let mut sb = good_container();
        sb.xp_desc_base = 999_999;
        assert!(matches!(
            validate_container(&sb),
            Err(FsError::BlockOutOfRange(_))
        ));
    }

    fn good_volume() -> gen::nolx_fs_superblock_t {
        let mut sb = gen::nolx_fs_superblock_t::zeroed();
        sb.magic = gen::MAGIC_VOLUME_SUPERBLOCK;
        sb.block_size = 4096;
        sb.features_required = u64::from(gen::NX_FEATURE_REQ_V1_BASE);
        sb.omap_oid = 10;
        sb.root_tree_oid = 11;
        sb.extentref_tree_oid = 12;
        sb.snap_meta_tree_oid = 13;
        sb.volume_name[..5].copy_from_slice(b"nolx\0");
        sb
    }

    #[test]
    fn a_well_formed_volume_superblock_validates() {
        assert_eq!(validate_volume(&good_volume()), Ok(()));
    }

    #[test]
    fn volume_validation_rejects_a_missing_tree() {
        let mut sb = good_volume();
        sb.root_tree_oid = 0;
        assert!(matches!(
            validate_volume(&sb),
            Err(FsError::InvalidField(_))
        ));
    }

    #[test]
    fn volume_validation_rejects_a_non_utf8_name() {
        let mut sb = good_volume();
        sb.volume_name[0] = 0xFF;
        sb.volume_name[1] = 0xFE;
        sb.volume_name[2] = 0;
        assert!(matches!(
            validate_volume(&sb),
            Err(FsError::InvalidField(_))
        ));
    }

    #[test]
    fn seal_then_parse_round_trips_an_object() {
        let mut block = vec![0u8; 4096];
        block[64] = 0xAB;
        block[4095] = 0xCD;
        let header = ObjHeader::make(7, 3, gen::OBJ_BTREE_ROOT, gen::SUB_FSTREE as u16);
        seal(&mut block, header).unwrap();

        let parsed = ObjRef::parse(&block, gen::OBJ_BTREE_ROOT).unwrap();
        assert_eq!(parsed.header().oid(), 7);
        assert_eq!(parsed.header().xid(), 3);
        assert_eq!(parsed.header().obj_type(), gen::OBJ_BTREE_ROOT);
        assert_eq!(parsed.header().subtype(), gen::SUB_FSTREE as u16);
        assert_eq!(parsed.body().len(), 4096 - 32);
    }

    #[test]
    fn parse_detects_a_single_flipped_bit() {
        let mut block = vec![0u8; 4096];
        block[100] = 1;
        seal(&mut block, ObjHeader::make(1, 1, gen::OBJ_OMAP, 0)).unwrap();
        assert!(ObjRef::parse(&block, gen::OBJ_OMAP).is_ok());

        block[100] ^= 0x80;
        assert!(matches!(
            ObjRef::parse(&block, gen::OBJ_OMAP),
            Err(FsError::BadChecksum { .. })
        ));
    }

    #[test]
    fn parse_rejects_the_wrong_object_type() {
        let mut block = vec![0u8; 4096];
        seal(&mut block, ObjHeader::make(1, 1, gen::OBJ_OMAP, 0)).unwrap();
        // `ObjRef` borrows the block it parsed and deliberately does not implement
        // `PartialEq`: two references to the same bytes are not the same object, and
        // comparing them would invite exactly that mistake. Assert on the error.
        assert!(matches!(
            ObjRef::parse(&block, gen::OBJ_BTREE_ROOT),
            Err(FsError::BadType {
                found: gen::OBJ_OMAP,
                expected: gen::OBJ_BTREE_ROOT,
            })
        ));
    }

    #[test]
    fn parse_rejects_a_short_buffer_instead_of_reading_past_it() {
        let short = vec![0u8; 16];
        assert!(matches!(
            ObjRef::parse(&short, gen::OBJ_OMAP),
            Err(FsError::TooShort { have: 16, need: 32 })
        ));
    }

    #[test]
    fn decode_name_accepts_and_rejects_what_it_should() {
        assert_eq!(decode_name(b"hello", 5), Ok("hello"));
        assert_eq!(decode_name(b"hello\0\0", 5), Ok("hello"));
        assert!(decode_name(b"a\0b", 3).is_err(), "embedded NUL");
        assert!(decode_name(b"hi", 5).is_err(), "length past the buffer");
        let long = alloc::vec![b'a'; 256];
        assert!(decode_name(&long, 256).is_err(), "longer than NAME_MAX");
        let max = alloc::vec![b'a'; 255];
        assert_eq!(decode_name(&max, 255).unwrap().len(), 255);
    }
}
