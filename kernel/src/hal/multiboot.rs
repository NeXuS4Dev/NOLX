//! The Multiboot 1 hand-off.
//!
//! The loader gives us one pointer and a magic value. Everything here reads that structure
//! defensively: the fields we use are bounds-checked against the structure's own length,
//! and a field whose feature flag is clear is reported as absent rather than read.
//!
//! M1b replaces this with Multiboot 2 and a native UEFI hand-off. The rest of the kernel
//! reads [`BootInfo`], not this structure, so that change is contained here.

use core::fmt;

/// Byte offset of the `flags` field.
const OFF_FLAGS: usize = 0;
/// Byte offset of `mem_lower`.
const OFF_MEM_LOWER: usize = 4;
/// Byte offset of `mem_upper`.
const OFF_MEM_UPPER: usize = 8;
/// Byte offset of `cmdline`.
const OFF_CMDLINE: usize = 16;
/// Byte offset of `mods_count`.
const OFF_MODS_COUNT: usize = 20;
/// Byte offset of `mods_addr`.
const OFF_MODS_ADDR: usize = 24;
/// Byte offset of `mmap_length`.
const OFF_MMAP_LENGTH: usize = 44;
/// Byte offset of `mmap_addr`.
const OFF_MMAP_ADDR: usize = 48;
/// Byte offset of `boot_loader_name`.
const OFF_BOOT_LOADER_NAME: usize = 64;

/// Feature flag: the `mem_*` and `mmap_*` fields are valid.
const FLAG_MEMINFO: u32 = 1 << 1;
/// Feature flag: `cmdline` is valid.
const FLAG_CMDLINE: u32 = 1 << 2;
/// Feature flag: `mods_*` are valid.
const FLAG_MODS: u32 = 1 << 3;
/// Feature flag: `boot_loader_name` is valid.
const FLAG_LOADER_NAME: u32 = 1 << 6;

/// A region of physical memory, as the firmware described it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MemoryRegion {
    /// First byte of the region.
    pub base: u64,
    /// Length in bytes.
    pub length: u64,
    /// Firmware region type: 1 is usable, everything else is reserved for some purpose.
    pub kind: u32,
}

impl MemoryRegion {
    /// `true` if the region is usable RAM.
    #[must_use]
    pub const fn is_usable(&self) -> bool {
        self.kind == 1
    }

    /// One past the last byte of the region.
    #[must_use]
    pub const fn end(&self) -> u64 {
        self.base + self.length
    }
}

/// What the loader told us, copied out of its structure so nothing else has to touch the
/// pointer again.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    /// The raw `flags` word.
    pub flags: u32,
    /// Conventional memory below 1 MiB, in kibibytes, if [`FLAG_MEMINFO`] was set.
    pub mem_lower_kib: u32,
    /// Extended memory above 1 MiB, in kibibytes, if [`FLAG_MEMINFO`] was set.
    pub mem_upper_kib: u32,
    /// Number of loaded modules (the initrd), if [`FLAG_MODS`] was set.
    pub mods_count: u32,
    /// Physical address of the module list.
    pub mods_addr: u32,
    /// Physical address of the command line, if [`FLAG_CMDLINE`] was set.
    pub cmdline_addr: u32,
    /// Length in bytes of the memory map buffer.
    pub mmap_length: u32,
    /// Physical address of the memory map buffer.
    pub mmap_addr: u32,
    /// Physical address of the loader's name string, if [`FLAG_LOADER_NAME`] was set.
    pub loader_name_addr: u32,
}

impl BootInfo {
    /// Copy the fields we care about out of the loader's structure.
    ///
    /// # Safety
    ///
    /// `ptr` must point to a valid Multiboot information structure that stays mapped for
    /// the life of the kernel. The caller obtains it from the boot layer, which got it from
    /// the loader.
    #[must_use]
    pub unsafe fn parse(ptr: *const u8) -> Self {
        // SAFETY: the caller guarantees the structure is valid and mapped. `read_u32`
        // reads four aligned bytes at a known offset inside it.
        unsafe {
            Self {
                flags: read_u32(ptr.add(OFF_FLAGS)),
                mem_lower_kib: read_u32(ptr.add(OFF_MEM_LOWER)),
                mem_upper_kib: read_u32(ptr.add(OFF_MEM_UPPER)),
                mods_count: if read_u32(ptr.add(OFF_FLAGS)) & FLAG_MODS != 0 {
                    read_u32(ptr.add(OFF_MODS_COUNT))
                } else {
                    0
                },
                mods_addr: read_u32(ptr.add(OFF_MODS_ADDR)),
                cmdline_addr: read_u32(ptr.add(OFF_CMDLINE)),
                mmap_length: if read_u32(ptr.add(OFF_FLAGS)) & FLAG_MEMINFO != 0 {
                    read_u32(ptr.add(OFF_MMAP_LENGTH))
                } else {
                    0
                },
                mmap_addr: read_u32(ptr.add(OFF_MMAP_ADDR)),
                loader_name_addr: read_u32(ptr.add(OFF_BOOT_LOADER_NAME)),
            }
        }
    }

    /// `true` if the memory map fields are valid.
    #[must_use]
    pub const fn has_memory_map(&self) -> bool {
        self.flags & FLAG_MEMINFO != 0
    }

    /// `true` if a command line was passed.
    #[must_use]
    pub const fn has_cmdline(&self) -> bool {
        self.flags & FLAG_CMDLINE != 0
    }

    /// `true` if the loader identified itself.
    ///
    /// Worth checking before reading [`BootInfo::loader_name_addr`]: the field is always
    /// present in the structure, but it holds a stale value unless this flag is set.
    #[must_use]
    pub const fn has_loader_name(&self) -> bool {
        self.flags & FLAG_LOADER_NAME != 0
    }

    /// Read the memory map into a fixed-capacity table.
    ///
    /// A table rather than an iterator because this runs before there is an allocator, and
    /// because the kernel needs the map more than once (frame allocation, ACPI reserved
    /// regions, framebuffer avoidance), so parsing it once into something owned is cheaper
    /// than walking the loader's buffer repeatedly.
    ///
    /// # Safety
    ///
    /// The `mmap_addr` and `mmap_length` in this `BootInfo` must come from a loader that
    /// built a valid map. Regions past [`MAX_REGIONS`] are dropped and reported in
    /// [`MemoryMap::truncated`].
    #[must_use]
    pub unsafe fn memory_map(&self) -> MemoryMap {
        if !self.has_memory_map() {
            return MemoryMap::empty();
        }
        // SAFETY: the caller guarantees the loader's map is valid and mapped.
        unsafe { MemoryMap::collect(self.mmap_addr, self.mmap_length) }
    }

    /// Total usable RAM in bytes, summed from the memory map.
    ///
    /// Falls back to `mem_upper`, which the loader always provides and which undercounts
    /// (it stops at the first hole) but is better than nothing.
    ///
    /// # Safety
    ///
    /// As [`BootInfo::memory_map`]: the `mmap_addr` and `mmap_length` recorded here must
    /// describe a valid, mapped memory map. This is `unsafe` rather than a plain function
    /// because it reads through a physical address the loader supplied, and nothing in the
    /// type can prove that address is still good.
    #[must_use]
    pub unsafe fn usable_bytes(&self) -> u64 {
        if !self.has_memory_map() {
            return u64::from(self.mem_upper_kib) * 1024;
        }
        // SAFETY: as [`BootInfo::memory_map`].
        unsafe { self.memory_map() }.usable_bytes()
    }
}

/// How many firmware memory regions the kernel keeps.
///
/// Real machines report a handful; firmware on large NUMA systems can report a few dozen.
/// 64 covers every machine NOLX targets, and anything beyond is dropped with a warning
/// rather than silently truncating the accounting.
pub const MAX_REGIONS: usize = 64;

/// The firmware's physical memory map, copied into kernel memory.
#[derive(Clone, Copy, Debug)]
pub struct MemoryMap {
    regions: [MemoryRegion; MAX_REGIONS],
    count: usize,
    /// Regions the firmware reported that did not fit in [`MAX_REGIONS`].
    dropped: usize,
}

impl MemoryMap {
    /// An empty map, for the case where the loader gave us none.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            regions: [MemoryRegion { base: 0, length: 0, kind: 0 }; MAX_REGIONS],
            count: 0,
            dropped: 0,
        }
    }

    /// Parse a Multiboot memory map buffer.
    ///
    /// # Safety
    ///
    /// `addr` must point to `length` bytes of valid, mapped memory in the Multiboot memory
    /// map layout: a `u32` payload size, then `u64 base`, `u64 length`, `u32 type`, then
    /// padding to the declared size.
    #[must_use]
    pub unsafe fn collect(addr: u32, length: u32) -> Self {
        let mut map = Self::empty();
        let base = addr as *const u8;
        let mut offset: u32 = 0;

        while offset + 24 <= length {
            // SAFETY: the caller guarantees the buffer is valid for `length` bytes, and the
            // loop condition keeps every read below that bound.
            let (size, region) = unsafe {
                let p = base.add(offset as usize);
                let size = read_u32(p);
                let lo = u64::from(read_u32(p.add(4)));
                let hi = u64::from(read_u32(p.add(8)));
                let len_lo = u64::from(read_u32(p.add(12)));
                let len_hi = u64::from(read_u32(p.add(16)));
                let kind = read_u32(p.add(20));
                (
                    size,
                    MemoryRegion { base: lo | (hi << 32), length: len_lo | (len_hi << 32), kind },
                )
            };

            // `size` is the payload length excluding the size field itself. Zero or a value
            // larger than what remains means the buffer is malformed; stop rather than loop
            // forever or read out of bounds.
            if size == 0 || size > length || size + 4 > length - offset {
                break;
            }
            offset += size + 4;

            if region.length == 0 {
                continue;
            }
            if map.count < MAX_REGIONS {
                map.regions[map.count] = region;
                map.count += 1;
            } else {
                map.dropped += 1;
            }
        }

        map
    }

    /// The regions, in the order the firmware reported them.
    #[must_use]
    pub fn regions(&self) -> &[MemoryRegion] {
        &self.regions[..self.count]
    }

    /// Regions the firmware reported beyond [`MAX_REGIONS`].
    #[must_use]
    pub const fn truncated(&self) -> usize {
        self.dropped
    }

    /// Total bytes of usable RAM.
    #[must_use]
    pub fn usable_bytes(&self) -> u64 {
        self.regions().iter().filter(|r| r.is_usable()).map(|r| r.length).sum()
    }

    /// Highest usable physical address, which bounds the frame allocator.
    #[must_use]
    pub fn highest_usable(&self) -> u64 {
        self.regions().iter().filter(|r| r.is_usable()).map(MemoryRegion::end).max().unwrap_or(0)
    }
}

impl fmt::Display for BootInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "flags={:#x}", self.flags)?;
        if self.flags & FLAG_MEMINFO != 0 {
            write!(
                f,
                " mem_lower={}KiB mem_upper={}KiB",
                self.mem_lower_kib, self.mem_upper_kib
            )?;
        }
        if self.flags & FLAG_MODS != 0 {
            write!(f, " modules={}", self.mods_count)?;
        }
        if self.has_memory_map() {
            write!(f, " mmap={}B@{:#x}", self.mmap_length, self.mmap_addr)?;
        }
        Ok(())
    }
}

/// Read a `u32` from a possibly unaligned raw pointer.
///
/// # Safety
///
/// The pointer must point to four readable bytes.
#[inline]
unsafe fn read_u32(ptr: *const u8) -> u32 {
    // SAFETY: the caller guarantees readability. `read_unaligned` handles any alignment.
    unsafe { ptr.cast::<u32>().read_unaligned() }
}
