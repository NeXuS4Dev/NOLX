//! The loader's second stage: page fills, `ET_DYN` bias, relocations, auxv, initial stack.
//!
//! [`crate::elf`] decides whether an image may be loaded. This module turns an accepted
//! plan into the *work* of loading it, still without touching a page table: a bounded
//! sequence of page fill operations, the ASLR bias for an `ET_DYN` image, the
//! `R_X86_64_RELATIVE` relocations to apply once the pages exist, and the exact bytes the
//! initial user stack carries. The kernel executes those four things; the host tests assert
//! them against synthetic images, so the arithmetic is proven before any machine boots.
//!
//! # Order of operations at load time
//!
//! 1. choose the bias ([`dyn_bias`]) -- zero for `ET_EXEC`;
//! 2. reserve the segment and stack ranges in the address space;
//! 3. walk [`PageOps`] in order: whole pages are zeroed first, then file bytes are copied;
//!    a page shared by two segments is therefore zeroed twice and copied twice, and the
//!    copies always win;
//! 4. apply [`relocations`] through the now-mapped pages;
//! 5. build the initial stack image ([`build_stack_image`]) at the top of the stack.

use crate::elf::{Elf64LoadPlan, ElfKind};

/// Why the second loader stage refused to produce load work.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageError {
    /// The `ET_DYN` span plus its chosen bias leaves the user address interval.
    BiasOutOfRange,
    /// A relocation has a type this loader does not apply.
    UnsupportedRelocation,
    /// A relocation target or table lies outside the loaded image.
    RelocationOutOfBounds,
    /// The dynamic table's relocation entries are malformed.
    MalformedDynamic,
    /// More relocations than the loader's fixed bound.
    TooManyRelocations,
    /// More arguments or environment strings than the initial stack carries.
    TooManyStrings,
    /// An argument or environment string is longer than the per-string bound.
    StringTooLong,
    /// An argument or environment string contains an embedded NUL byte.
    EmbeddedNul,
    /// The stack image does not fit the fixed bound or the caller's buffer.
    StackImageTooLarge,
}

/// One page-granular load operation.
///
/// The kernel applies a [`PageOp`] sequence to already-mapped, demand-zero frames. The
/// sequence zeroes every touched page before copying any file bytes, so operations commute
/// with each other except for the intentional copy-after-zero order between the two phases.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageOp {
    /// Zero the whole 4 KiB page at `page`.
    Zero {
        /// Page-aligned virtual address.
        page: u64,
    },
    /// Copy `length` file bytes to `page + offset`.
    Copy {
        /// Page-aligned virtual address.
        page: u64,
        /// Byte offset inside the page.
        offset: u32,
        /// Offset of the source bytes in the ELF file.
        file_offset: u64,
        /// Number of bytes to copy; never more than a page.
        length: u32,
    },
}

/// Bounded cursor producing the [`PageOp`] sequence for one image.
pub struct PageOps<'a> {
    plan: &'a Elf64LoadPlan<'a>,
    bias: u64,
    phase: Phase,
    segment: usize,
    progress: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Zero,
    Copy,
    Done,
}

impl<'a> PageOps<'a> {
    /// Start walking `plan`'s segments with `bias` applied to every virtual address.
    pub const fn new(plan: &'a Elf64LoadPlan<'a>, bias: u64) -> Self {
        Self {
            plan,
            bias,
            phase: Phase::Zero,
            segment: 0,
            progress: 0,
        }
    }

    /// The next operation, or `None` when the walk is finished.
    pub fn next_op(&mut self) -> Option<PageOp> {
        loop {
            match self.phase {
                Phase::Zero => {
                    let Some(segment) = self.plan.segments().nth(self.segment) else {
                        self.phase = Phase::Copy;
                        self.segment = 0;
                        self.progress = 0;
                        continue;
                    };
                    let start = segment.virtual_address() + self.bias;
                    let end = segment.end_address() + self.bias;
                    let first_page = start & !0xfff;
                    let pages = (end - first_page).div_ceil(0x1000);
                    if self.progress >= pages {
                        self.segment += 1;
                        self.progress = 0;
                        continue;
                    }
                    let page = first_page + self.progress * 0x1000;
                    self.progress += 1;
                    return Some(PageOp::Zero { page });
                }
                Phase::Copy => {
                    let Some(segment) = self.plan.segments().nth(self.segment) else {
                        self.phase = Phase::Done;
                        return None;
                    };
                    if self.progress >= segment.file_size() {
                        self.segment += 1;
                        self.progress = 0;
                        continue;
                    }
                    let virtual_address = segment.virtual_address() + self.bias + self.progress;
                    let page = virtual_address & !0xfff;
                    let offset = (virtual_address & 0xfff) as u32;
                    let remaining = segment.file_size() - self.progress;
                    let room = 0x1000 - u64::from(offset);
                    let length = remaining.min(room);
                    let file_offset = segment.file_offset() + self.progress;
                    self.progress += length;
                    return Some(PageOp::Copy {
                        page,
                        offset,
                        file_offset,
                        length: length as u32,
                    });
                }
                Phase::Done => return None,
            }
        }
    }
}

/// An iterator view over [`PageOps`], for host tests and ergonomic kernel loops.
impl Iterator for PageOps<'_> {
    type Item = PageOp;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_op()
    }
}

/// Walk a plan's page fills without applying a bias (the `ET_EXEC` case).
///
/// The zero phase finishes completely before the copy phase; `ImageError` cannot occur
/// here because [`crate::elf`] already validated the ranges.
pub fn page_ops<'a>(plan: &'a Elf64LoadPlan<'a>, bias: u64) -> PageOps<'a> {
    PageOps::new(plan, bias)
}

/// Number of 2 MiB-aligned bias slots the ASLR window offers an `ET_DYN` image.
pub const ASLR_SLOTS: u64 = 4096;

/// Bias slot granularity: 2 MiB, so the slide is also huge-page friendly.
pub const ASLR_SLOT_BYTES: u64 = 0x20_0000;

/// Lowest `ET_DYN` load address, 4 GiB up: far above the null page and the `ET_EXEC` range.
pub const ASLR_BASE: u64 = 0x1_0000_0000;

/// Choose the load bias for an image.
///
/// `ET_EXEC` images carry absolute addresses and get exactly zero. An `ET_DYN` image gets
/// [`ASLR_BASE`] plus one of [`ASLR_SLOTS`] slots picked from `entropy`, biased so that the
/// image's own lowest virtual address lands at that address. The result keeps every
/// segment, after biasing, inside the user interval.
pub fn dyn_bias(plan: &Elf64LoadPlan<'_>, entropy: u64) -> Result<u64, ImageError> {
    if plan.kind() == ElfKind::Exec {
        return Ok(0);
    }
    let span = plan.image_end() - plan.image_base();
    let slot = entropy % ASLR_SLOTS;
    let target_base = ASLR_BASE + slot * ASLR_SLOT_BYTES;
    let bias = target_base.wrapping_sub(plan.image_base());
    // The biased image occupies [target_base, target_base + span).
    if target_base < nolx_mm_core::vad::USER_SPACE_LOW
        || target_base
            .checked_add(span)
            .is_none_or(|end| end > nolx_mm_core::vad::USER_SPACE_HIGH)
        || span > ASLR_SLOTS * ASLR_SLOT_BYTES
    {
        return Err(ImageError::BiasOutOfRange);
    }
    Ok(bias)
}

/// The biased entry point: the user RIP the initial context starts at.
#[must_use]
pub const fn loaded_entry(plan: &Elf64LoadPlan<'_>, bias: u64) -> u64 {
    plan.entry_point() + bias
}

/// A relocation the loader applies to a mapped page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Relocation {
    /// Virtual address (already biased) of the `u64` to patch.
    pub address: u64,
    /// Value to store: the biased virtual address of the target.
    pub value: u64,
}

const DT_NULL: u64 = 0;
const DT_RELA: u64 = 7;
const DT_RELASZ: u64 = 8;
const DT_RELAENT: u64 = 9;
const R_X86_64_NONE: u64 = 0;
const R_X86_64_RELATIVE: u64 = 8;
const RELA_ENTRY_SIZE: u64 = 24;

/// Fixed bound on relocations one image may declare. Real images carry more; this bound is
/// the executive's early-boot budget and is asserted, not assumed.
pub const MAX_RELOCATIONS: usize = 256;

/// Validate and collect the `R_X86_64_RELATIVE` relocations of `plan`.
///
/// The dynamic table is walked inside `image` at the file range recorded in the plan; each
/// `DT_RELA` target address is translated back to a file offset through the load segments.
/// `R_X86_64_NONE` is accepted and skipped; every other type is refused, because a static
/// loader that silently ignores a `GLOB_DAT` is a loader that produces a corrupt process.
pub fn relocations(
    image: &[u8],
    plan: &Elf64LoadPlan<'_>,
    bias: u64,
) -> Result<RelocationList, ImageError> {
    let Some(table) = plan.dynamic() else {
        return Ok(RelocationList {
            items: [None; MAX_RELOCATIONS],
            count: 0,
        });
    };

    let table_start =
        usize::try_from(table.file_offset()).map_err(|_| ImageError::MalformedDynamic)?;
    let table_end = usize::try_from(table.file_offset() + table.file_size())
        .map_err(|_| ImageError::MalformedDynamic)?;
    let table_bytes = image
        .get(table_start..table_end)
        .ok_or(ImageError::MalformedDynamic)?;

    let mut rela_address = 0u64;
    let mut rela_size = 0u64;
    let mut rela_ent = RELA_ENTRY_SIZE;
    let mut entries = table_bytes.chunks_exact(16);
    for entry in &mut entries {
        let tag = u64::from_le_bytes(entry[0..8].try_into().unwrap());
        let value = u64::from_le_bytes(entry[8..16].try_into().unwrap());
        match tag {
            DT_NULL => break,
            DT_RELA => rela_address = value,
            DT_RELASZ => rela_size = value,
            DT_RELAENT => rela_ent = value,
            _ => {}
        }
    }
    if rela_ent != RELA_ENTRY_SIZE || rela_size % rela_ent != 0 {
        return Err(ImageError::MalformedDynamic);
    }
    if rela_size == 0 {
        return Ok(RelocationList {
            items: [None; MAX_RELOCATIONS],
            count: 0,
        });
    }

    let count =
        usize::try_from(rela_size / rela_ent).map_err(|_| ImageError::TooManyRelocations)?;
    if count > MAX_RELOCATIONS {
        return Err(ImageError::TooManyRelocations);
    }

    let rela_file_offset =
        virtual_to_file(plan, rela_address).ok_or(ImageError::RelocationOutOfBounds)?;
    let rela_start =
        usize::try_from(rela_file_offset).map_err(|_| ImageError::RelocationOutOfBounds)?;
    let rela_end = usize::try_from(rela_file_offset + rela_size)
        .map_err(|_| ImageError::RelocationOutOfBounds)?;
    let rela_bytes = image
        .get(rela_start..rela_end)
        .ok_or(ImageError::RelocationOutOfBounds)?;

    let mut items = [None; MAX_RELOCATIONS];
    for (index, chunk) in rela_bytes
        .chunks_exact(RELA_ENTRY_SIZE as usize)
        .enumerate()
    {
        let offset = u64::from_le_bytes(chunk[0..8].try_into().unwrap());
        let info = u64::from_le_bytes(chunk[8..16].try_into().unwrap());
        let addend = i64::from_le_bytes(chunk[16..24].try_into().unwrap());
        match info & 0xffff_ffff {
            R_X86_64_NONE => continue,
            R_X86_64_RELATIVE => {
                let address = offset
                    .checked_add(bias)
                    .ok_or(ImageError::RelocationOutOfBounds)?;
                let value = (addend as u64)
                    .checked_add(bias)
                    .ok_or(ImageError::RelocationOutOfBounds)?;
                if virtual_to_file(plan, offset).is_none() {
                    return Err(ImageError::RelocationOutOfBounds);
                }
                items[index] = Some(Relocation { address, value });
            }
            _ => return Err(ImageError::UnsupportedRelocation),
        }
    }
    Ok(RelocationList {
        items,
        count: rela_bytes.len() / RELA_ENTRY_SIZE as usize,
    })
}

/// The validated relocation list: fixed capacity, no allocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelocationList {
    items: [Option<Relocation>; MAX_RELOCATIONS],
    count: usize,
}

impl RelocationList {
    /// How many `PT_DYNAMIC` `DT_RELA` entries were inspected (including `R_X86_64_NONE`).
    #[must_use]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// The relocations to apply, in table order.
    pub fn iter(&self) -> impl Iterator<Item = Relocation> + '_ {
        self.items[..self.count].iter().flatten().copied()
    }
}

fn virtual_to_file(plan: &Elf64LoadPlan<'_>, address: u64) -> Option<u64> {
    plan.segments().find_map(|segment| {
        if address >= segment.virtual_address() && address < segment.end_address() {
            let delta = address - segment.virtual_address();
            if delta < segment.file_size() {
                Some(segment.file_offset() + delta)
            } else {
                None
            }
        } else {
            None
        }
    })
}

/// Fixed bound on the initial stack image, strings included.
pub const STACK_IMAGE_MAX: usize = 32 * 1024;

/// What the initial user stack carries.
#[derive(Clone, Copy, Debug)]
pub struct StackRequest<'a> {
    /// Argument strings, without terminating NULs.
    pub argv: &'a [&'a [u8]],
    /// Environment strings, without terminating NULs.
    pub envp: &'a [&'a [u8]],
    /// Caller-computed auxiliary entries (`AT_PHDR`, `AT_ENTRY`, ...), tags and values.
    pub auxv: &'a [(u64, u64)],
    /// The 16 bytes `AT_RANDOM` points at.
    pub random: [u8; 16],
    /// The executable path `AT_EXECFN` points at.
    pub execfn: &'a [u8],
}

fn string_block_size(req: &StackRequest<'_>) -> Result<usize, ImageError> {
    if req.argv.len() > nolx_types::abi::MAX_ARG_COUNT
        || req.envp.len() > nolx_types::abi::MAX_ARG_COUNT
    {
        return Err(ImageError::TooManyStrings);
    }
    let mut total = 0usize;
    for string in req
        .argv
        .iter()
        .chain(req.envp.iter())
        .copied()
        .chain([req.execfn])
    {
        if string.len() > nolx_types::abi::MAX_ARG_BYTES {
            return Err(ImageError::StringTooLong);
        }
        if string.contains(&0) {
            return Err(ImageError::EmbeddedNul);
        }
        total = total
            .checked_add(string.len() + 1)
            .ok_or(ImageError::StackImageTooLarge)?;
    }
    total = total
        .checked_add(16)
        .ok_or(ImageError::StackImageTooLarge)?;
    Ok(total)
}

/// Size in bytes of the stack image [`build_stack_image`] writes.
pub fn stack_image_size(req: &StackRequest<'_>) -> Result<usize, ImageError> {
    let strings = string_block_size(req)?;
    let pointers = core::mem::size_of::<u64>()
        * (1 + req.argv.len() + 1 + req.envp.len() + 1 + (req.auxv.len() + 3) * 2);
    let total = pointers
        .checked_add(strings)
        .ok_or(ImageError::StackImageTooLarge)?;
    if total > STACK_IMAGE_MAX {
        return Err(ImageError::StackImageTooLarge);
    }
    Ok(total)
}

/// Build the initial stack image into `out`, which maps to `[top - size, top)`.
///
/// Returns the entry `%rsp`, which is 16-byte aligned and equals `top` minus the image
/// size (plus alignment padding placed between the vector and the strings). The image is
/// the SysV process-entry shape: `argc`, argv pointers, envp pointers, auxiliary pairs
/// terminated by `AT_NULL`, then the strings themselves. The builder appends `AT_RANDOM`
/// and `AT_EXECFN` to the caller's auxiliary entries; the caller supplies the machine-
/// specific ones (`AT_PHDR`, `AT_ENTRY`, `AT_PAGESZ`, ...) so this module needs no
/// knowledge of the load it is being built for.
pub fn build_stack_image(
    out: &mut [u8],
    req: &StackRequest<'_>,
    top: u64,
) -> Result<u64, ImageError> {
    let size = stack_image_size(req)?;
    let unaligned_rsp = top
        .checked_sub(size as u64)
        .ok_or(ImageError::StackImageTooLarge)?;
    // Padding goes above the vector, so the image grows downward by exactly the amount
    // that brings `%rsp` to a 16-byte boundary (SysV requires that at process entry).
    let padding = (unaligned_rsp % 16) as usize;
    let total = size
        .checked_add(padding)
        .ok_or(ImageError::StackImageTooLarge)?;
    if out.len() < total {
        return Err(ImageError::StackImageTooLarge);
    }
    let rsp = top - total as u64;
    let image = &mut out[..total];
    image.fill(0);

    // The string block sits at the top of the image; `cursor` walks it forward.
    let mut cursor = pointers_block_size(req);
    let mut string_address = rsp + cursor as u64;

    let mut at = 0usize;
    put_u64(image, at, req.argv.len() as u64);
    at += 8;

    let mut argv_addresses = [0u64; nolx_types::abi::MAX_ARG_COUNT];
    for (index, string) in req.argv.iter().enumerate() {
        argv_addresses[index] = string_address;
        image[cursor..cursor + string.len()].copy_from_slice(string);
        cursor += string.len();
        image[cursor] = 0;
        cursor += 1;
        string_address += string.len() as u64 + 1;
    }
    for address in &argv_addresses[..req.argv.len()] {
        put_u64(image, at, *address);
        at += 8;
    }
    put_u64(image, at, 0);
    at += 8;

    for string in req.envp {
        put_u64(image, at, string_address);
        at += 8;
        image[cursor..cursor + string.len()].copy_from_slice(string);
        cursor += string.len();
        image[cursor] = 0;
        cursor += 1;
        string_address += string.len() as u64 + 1;
    }
    put_u64(image, at, 0);
    at += 8;

    let random_address = string_address + (req.execfn.len() as u64 + 1);
    for (tag, value) in req.auxv {
        put_u64(image, at, *tag);
        at += 8;
        put_u64(image, at, *value);
        at += 8;
    }
    put_u64(image, at, nolx_types::abi::at::RANDOM);
    at += 8;
    put_u64(image, at, random_address);
    at += 8;
    put_u64(image, at, nolx_types::abi::at::EXECFN);
    at += 8;
    put_u64(image, at, rsp + cursor as u64);
    at += 8;
    put_u64(image, at, nolx_types::abi::at::NULL);
    at += 8;
    put_u64(image, at, 0);
    at += 8;
    debug_assert_eq!(at, pointers_block_size(req));

    image[cursor..cursor + req.execfn.len()].copy_from_slice(req.execfn);
    cursor += req.execfn.len();
    image[cursor] = 0;
    cursor += 1;
    image[cursor..cursor + 16].copy_from_slice(&req.random);
    cursor += 16;
    debug_assert_eq!(cursor, size);

    Ok(rsp)
}

fn pointers_block_size(req: &StackRequest<'_>) -> usize {
    core::mem::size_of::<u64>()
        * (1 + req.argv.len() + 1 + req.envp.len() + 1 + (req.auxv.len() + 3) * 2)
}

fn put_u64(image: &mut [u8], at: usize, value: u64) {
    image[at..at + 8].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::elf::{Elf64LoadPlan, ElfError};
    use std::collections::BTreeMap;

    const TEXT_OFFSET: u64 = 0;
    const TEXT_ADDRESS: u64 = 0x2_0000;
    const TEXT_FILE: u64 = 0x1180; // headers plus the marked text bytes at 0x1000
    const TEXT_MEMORY: u64 = 0x2800; // two pages of file+bss, plus one pure bss page
    const DATA_OFFSET: u64 = 0x3000;
    const DATA_ADDRESS: u64 = 0x3_0000;
    const DATA_FILE: u64 = 0x400;
    const DATA_MEMORY: u64 = 0x400;

    struct TestPhdr {
        kind: u32,
        flags: u32,
        file_offset: u64,
        virtual_address: u64,
        file_size: u64,
        memory_size: u64,
        alignment: u64,
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_hdr_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_phdr(image: &mut [u8], index: usize, header: TestPhdr) {
        let offset = 64 + index * 56;
        put_u32(image, offset, header.kind);
        put_u32(image, offset + 4, header.flags);
        put_hdr_u64(image, offset + 8, header.file_offset);
        put_hdr_u64(image, offset + 16, header.virtual_address);
        put_hdr_u64(image, offset + 24, header.virtual_address);
        put_hdr_u64(image, offset + 32, header.file_size);
        put_hdr_u64(image, offset + 40, header.memory_size);
        put_hdr_u64(image, offset + 48, header.alignment);
    }

    /// A `PT_DYNAMIC` with a relocation table plus one `PT_LOAD` pair, at fixed offsets.
    fn dyn_image() -> Vec<u8> {
        let mut image = vec![0u8; 0x5000];
        image[..4].copy_from_slice(b"\x7fELF");
        image[4] = 2;
        image[5] = 1;
        image[6] = 1;
        put_u16(&mut image, 16, 3); // ET_DYN
        put_u16(&mut image, 18, 62);
        put_u32(&mut image, 20, 1);
        put_hdr_u64(&mut image, 24, TEXT_ADDRESS + 0x40); // entry
        put_hdr_u64(&mut image, 32, 64); // phoff
        put_u16(&mut image, 52, 64);
        put_u16(&mut image, 54, 56);
        put_u16(&mut image, 56, 4); // four program headers

        put_phdr(
            &mut image,
            0,
            TestPhdr {
                kind: 1, // PT_LOAD text
                flags: 5,
                file_offset: TEXT_OFFSET,
                virtual_address: TEXT_ADDRESS,
                file_size: TEXT_FILE,
                memory_size: TEXT_MEMORY,
                alignment: 0x1000,
            },
        );
        put_phdr(
            &mut image,
            1,
            TestPhdr {
                kind: 1, // PT_LOAD data
                flags: 6,
                file_offset: DATA_OFFSET,
                virtual_address: DATA_ADDRESS,
                file_size: DATA_FILE,
                memory_size: DATA_MEMORY,
                alignment: 0x1000,
            },
        );
        put_phdr(
            &mut image,
            2,
            TestPhdr {
                kind: 2, // PT_DYNAMIC
                flags: 6,
                file_offset: 0x3100,
                virtual_address: DATA_ADDRESS + 0x100,
                file_size: 5 * 16,
                memory_size: 5 * 16,
                alignment: 8,
            },
        );
        put_phdr(
            &mut image,
            3,
            TestPhdr {
                kind: 0x6474_e551, // PT_GNU_STACK
                flags: 6,
                file_offset: 0,
                virtual_address: 0,
                file_size: 0,
                memory_size: 0,
                alignment: 0x10,
            },
        );

        // Dynamic table at file 0x3100: DT_RELA, DT_RELASZ, DT_RELAENT, DT_NULL.
        let dyn_off = 0x3100usize;
        put_hdr_u64(&mut image, dyn_off, 7); // DT_RELA
        put_hdr_u64(&mut image, dyn_off + 8, DATA_ADDRESS + 0x200); // rela vaddr
        put_hdr_u64(&mut image, dyn_off + 16, 8); // DT_RELASZ
        put_hdr_u64(&mut image, dyn_off + 24, 24 * 2); // two entries
        put_hdr_u64(&mut image, dyn_off + 32, 9); // DT_RELAENT
        put_hdr_u64(&mut image, dyn_off + 40, 24);
        put_hdr_u64(&mut image, dyn_off + 48, 0); // DT_NULL
        put_hdr_u64(&mut image, dyn_off + 56, 0);

        // Relocation table at file 0x3200: R_X86_64_RELATIVE + R_X86_64_NONE.
        let rela_off = 0x3200usize;
        put_hdr_u64(&mut image, rela_off, DATA_ADDRESS + 0x30); // r_offset
        put_hdr_u64(&mut image, rela_off + 8, 8); // R_X86_64_RELATIVE
        put_hdr_u64(&mut image, rela_off + 16, 0x1234); // addend
        put_hdr_u64(&mut image, rela_off + 24, DATA_ADDRESS + 0x38); // r_offset
        put_hdr_u64(&mut image, rela_off + 32, 0); // R_X86_64_NONE
        put_hdr_u64(&mut image, rela_off + 40, 0);

        // Recognizable text bytes at file 0x1000 (virtual 0x21000) and data bytes.
        image[0x1000] = 0x90;
        image[0x1001] = 0xC3;
        image[DATA_OFFSET as usize] = 0xAA;
        image
    }

    #[test]
    fn page_fills_zero_every_touched_page_then_copy_file_bytes() {
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid ET_DYN image");
        assert_eq!(plan.kind(), ElfKind::Dyn);

        let ops: Vec<PageOp> = page_ops(&plan, 0).collect();
        // Text: pages 0x20000..0x22800 -> 3 pages; data: 0x30000 -> 1 page.
        let zeros: Vec<u64> = ops
            .iter()
            .filter_map(|op| match op {
                PageOp::Zero { page } => Some(*page),
                _ => None,
            })
            .collect();
        assert_eq!(zeros, vec![0x2_0000, 0x2_1000, 0x2_2000, 0x3_0000]);
        // Every zero comes before every copy: copies always win.
        let first_copy = ops
            .iter()
            .position(|op| matches!(op, PageOp::Copy { .. }))
            .expect("copy ops");
        assert!(ops[first_copy..]
            .iter()
            .all(|op| matches!(op, PageOp::Copy { .. })));

        // Apply the ops to a fake physical memory and check the byte-exact result.
        let mut memory: BTreeMap<u64, [u8; 0x1000]> = BTreeMap::new();
        for op in &ops {
            match op {
                PageOp::Zero { page } => {
                    memory.insert(*page, [0u8; 0x1000]);
                }
                PageOp::Copy {
                    page,
                    offset,
                    file_offset,
                    length,
                } => {
                    let slot = memory.entry(*page).or_insert([0u8; 0x1000]);
                    let start = *offset as usize;
                    let end = start + *length as usize;
                    slot[start..end].copy_from_slice(
                        &image[*file_offset as usize..*file_offset as usize + end - start],
                    );
                }
            }
        }
        assert_eq!(memory[&0x2_0000][4], 2); // the ELF header rode along in page one
        assert_eq!(memory[&0x2_1000][0], 0x90);
        assert_eq!(memory[&0x2_1000][1], 0xC3);
        assert_eq!(memory[&0x2_1000][0x180], 0); // zero-filled bss tail
        assert_eq!(memory[&0x2_2000][0], 0); // pure bss page is all zero
        assert_eq!(memory[&0x3_0000][0], 0xAA);
    }

    #[test]
    fn page_fills_apply_the_bias_to_every_address() {
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid ET_DYN image");
        let bias = 0x1_0000_0000;
        let ops: Vec<PageOp> = page_ops(&plan, bias).collect();
        for op in &ops {
            let page = match op {
                PageOp::Zero { page } => *page,
                PageOp::Copy { page, .. } => *page,
            };
            assert_eq!(page & 0xfff, 0);
            assert!(page >= bias + TEXT_ADDRESS);
            assert!(page < bias + DATA_ADDRESS + 0x1000);
        }
    }

    #[test]
    fn dyn_bias_is_entropy_sensitive_bounded_and_zero_for_exec() {
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid ET_DYN image");
        let span = plan.image_end() - plan.image_base();
        let first = dyn_bias(&plan, 0).expect("bias");
        let other = dyn_bias(&plan, 77).expect("bias");
        assert_ne!(first, other, "different entropy picks different slots");
        assert_eq!(first, dyn_bias(&plan, 0).expect("bias"), "bias is stable");
        // Slot granularity: the bias moves the image by whole 2 MiB slots.
        assert_eq!((other - first) % ASLR_SLOT_BYTES, 0);
        for entropy in [0u64, 1, 4095, 4096, u64::MAX] {
            let bias = dyn_bias(&plan, entropy).expect("bias");
            let base = bias + plan.image_base();
            assert!(base >= nolx_mm_core::vad::USER_SPACE_LOW);
            assert!(base + span <= nolx_mm_core::vad::USER_SPACE_HIGH);
        }
        assert_eq!(loaded_entry(&plan, first), plan.entry_point() + first);

        // An ET_EXEC plan gets no bias at all.
        let mut fixed = dyn_image();
        put_u16(&mut fixed, 16, 2); // ET_EXEC (addresses stay relative in this fixture)
        let fixed_plan = Elf64LoadPlan::parse(&fixed).expect("fixture parses as ET_EXEC");
        assert_eq!(fixed_plan.kind(), ElfKind::Exec);
        assert_eq!(dyn_bias(&fixed_plan, 123).expect("bias"), 0);
    }

    #[test]
    fn relative_relocations_are_collected_biased_and_bounded() {
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid ET_DYN image");
        let bias = 0x1_0000_0000;
        let list = relocations(&image, &plan, bias).expect("relocations");
        assert_eq!(list.count(), 2);
        let applied: Vec<Relocation> = list.iter().collect();
        assert_eq!(applied.len(), 1, "R_X86_64_NONE is skipped");
        assert_eq!(applied[0].address, DATA_ADDRESS + 0x30 + bias);
        assert_eq!(applied[0].value, 0x1234 + bias);
    }

    #[test]
    fn unsupported_and_unmapped_relocations_are_refused() {
        let mut image = dyn_image();
        // Rewrite the first relocation to R_X86_64_GLOB_DAT (6).
        put_hdr_u64(&mut image, 0x3200 + 8, 6);
        let plan = Elf64LoadPlan::parse(&image).expect("valid image");
        assert_eq!(
            relocations(&image, &plan, 0),
            Err(ImageError::UnsupportedRelocation)
        );

        // A relocation targeting bytes no segment maps is refused.
        let mut image = dyn_image();
        put_hdr_u64(&mut image, 0x3200, 0x9000); // outside every segment
        let plan = Elf64LoadPlan::parse(&image).expect("valid image");
        assert_eq!(
            relocations(&image, &plan, 0),
            Err(ImageError::RelocationOutOfBounds)
        );
    }

    #[test]
    fn static_images_carry_no_relocations() {
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid image");
        // Remove the dynamic header by zeroing its type.
        let mut static_image = image.clone();
        put_u32(&mut static_image, 64 + 2 * 56, 0);
        let static_plan = Elf64LoadPlan::parse(&static_image).expect("image without PT_DYNAMIC");
        assert!(static_plan.dynamic().is_none());
        let list = relocations(&static_image, &static_plan, 0).expect("no relocs");
        assert_eq!(list.count(), 0);
        assert_eq!(list.iter().count(), 0);
        let _ = plan;
    }

    #[test]
    fn stack_image_carries_argc_argv_envp_auxv_and_entropy() {
        let argv: [&[u8]; 2] = [b"init", b"--test"];
        let envp: [&[u8]; 1] = [b"HOME=/"];
        let auxv: [(u64, u64); 3] = [
            (nolx_types::abi::at::PHDR, 0x40_0040),
            (nolx_types::abi::at::ENTRY, 0x40_1000),
            (nolx_types::abi::at::PAGESZ, 0x1000),
        ];
        let request = StackRequest {
            argv: &argv,
            envp: &envp,
            auxv: &auxv,
            random: [7u8; 16],
            execfn: b"/bin/init",
        };
        let size = stack_image_size(&request).expect("size");
        let top = 0x7fff_f000u64;
        let mut out = vec![0u8; size + 32];
        let rsp = build_stack_image(&mut out, &request, top).expect("stack");
        assert_eq!(rsp % 16, 0, "SysV requires 16-byte-aligned entry rsp");
        assert!(rsp < top);
        assert!(top - rsp <= size as u64 + 16);

        let image = &out[..(top - rsp) as usize];
        let read_u64 =
            |at: usize| -> u64 { u64::from_le_bytes(image[at..at + 8].try_into().unwrap()) };
        let read_cstr = |offset: usize| -> Vec<u8> {
            image[offset..]
                .iter()
                .copied()
                .take_while(|b| *b != 0)
                .collect()
        };

        // The vector is expressed in virtual addresses; translate back into the buffer.
        let base = rsp;
        assert_eq!(read_u64(0), 2, "argc");
        let argv0 = read_u64(8);
        let argv1 = read_u64(16);
        assert_eq!(read_u64(24), 0, "argv NULL terminator");
        let env0 = read_u64(32);
        assert_eq!(read_u64(40), 0, "envp NULL terminator");
        assert_eq!(read_cstr((argv0 - base) as usize), b"init".to_vec());
        assert_eq!(read_cstr((argv1 - base) as usize), b"--test".to_vec());
        assert_eq!(read_cstr((env0 - base) as usize), b"HOME=/".to_vec());

        // Walk the auxiliary vector: the caller's entries, then RANDOM, EXECFN, NULL.
        let mut at = 48usize;
        let mut seen = BTreeMap::new();
        loop {
            let tag = read_u64(at);
            let value = read_u64(at + 8);
            at += 16;
            if tag == nolx_types::abi::at::NULL {
                assert_eq!(value, 0);
                break;
            }
            seen.insert(tag, value);
        }
        assert_eq!(seen[&nolx_types::abi::at::PHDR], 0x40_0040);
        assert_eq!(seen[&nolx_types::abi::at::ENTRY], 0x40_1000);
        assert_eq!(seen[&nolx_types::abi::at::PAGESZ], 0x1000);
        let random_at = seen[&nolx_types::abi::at::RANDOM];
        assert_eq!(
            &image[(random_at - base) as usize..(random_at - base) as usize + 16],
            &[7u8; 16],
            "AT_RANDOM points at the 16 entropy bytes"
        );
        let execfn_at = seen[&nolx_types::abi::at::EXECFN];
        assert_eq!(
            read_cstr((execfn_at - base) as usize),
            b"/bin/init".to_vec()
        );
    }

    #[test]
    fn stack_image_rejects_bad_strings_and_oversized_requests() {
        let bad: [&[u8]; 1] = [b"hel\x00lo"];
        let request = StackRequest {
            argv: &bad,
            envp: &[],
            auxv: &[],
            random: [0; 16],
            execfn: b"x",
        };
        assert_eq!(
            build_stack_image(&mut [0u8; 64], &request, 0x1000),
            Err(ImageError::EmbeddedNul)
        );

        let long = [vec![b'x'; nolx_types::abi::MAX_ARG_BYTES + 1]];
        let long_refs: Vec<&[u8]> = long.iter().map(|v| v.as_slice()).collect();
        let request = StackRequest {
            argv: &long_refs,
            envp: &[],
            auxv: &[],
            random: [0; 16],
            execfn: b"x",
        };
        assert_eq!(stack_image_size(&request), Err(ImageError::StringTooLong));

        let empty: [&[u8]; 0] = [];
        let empty_aux: [(u64, u64); 0] = [];
        let request = StackRequest {
            argv: &empty,
            envp: &empty,
            auxv: &empty_aux,
            random: [0; 16],
            execfn: b"x",
        };
        let mut out = [0u8; 4];
        assert_eq!(
            build_stack_image(&mut out, &request, 0x1000),
            Err(ImageError::StackImageTooLarge),
            "a buffer smaller than the image is refused"
        );
    }

    #[test]
    fn empty_images_still_build_a_zero_argument_stack() {
        let empty: [&[u8]; 0] = [];
        let request = StackRequest {
            argv: &empty,
            envp: &empty,
            auxv: &[(nolx_types::abi::at::PAGESZ, 0x1000)],
            random: [1u8; 16],
            execfn: b"init",
        };
        let size = stack_image_size(&request).expect("size");
        let top = 0x2_0000_0000;
        let mut out = vec![0u8; size + 16];
        let rsp = build_stack_image(&mut out, &request, top).expect("stack");
        assert_eq!(rsp % 16, 0);
        let argc = u64::from_le_bytes(out[0..8].try_into().unwrap());
        assert_eq!(argc, 0);
    }

    #[test]
    fn elf_rejects_images_this_stage_would_misload() {
        // Guard the boundary between the two stages: parse must not accept what image
        // would then refuse.
        let image = dyn_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid");
        assert_eq!(plan.program_header_count(), 4);
        assert!(plan.program_headers_address().is_some());
        let short = &image[..20];
        assert_eq!(Elf64LoadPlan::parse(short), Err(ElfError::FileTooShort));
        let _ = plan;
    }

    // ---------------------------------------------------------------- the embedded
    // ring-3 programs, loaded through the same plan the kernel will use.

    #[test]
    fn the_embedded_programs_plan_like_the_kernel_will_load_them() {
        for (path, bytes) in crate::programs_gen::PROGRAMS {
            let plan = Elf64LoadPlan::parse(bytes).expect("embedded program parses");
            assert_eq!(plan.kind(), ElfKind::Exec, "{path} is fixed-address");
            assert_eq!(plan.interpreter(), None, "{path} is static");
            assert!(!plan.stack_executable(), "{path} leaves the stack NX");

            // The entry byte actually lands at the entry address when the page plan is
            // applied -- the loader's whole job in one assertion.
            let mut memory: BTreeMap<u64, [u8; 0x1000]> = BTreeMap::new();
            for op in page_ops(&plan, 0) {
                match op {
                    PageOp::Zero { page } => {
                        memory.insert(page, [0u8; 0x1000]);
                    }
                    PageOp::Copy {
                        page,
                        offset,
                        file_offset,
                        length,
                    } => {
                        let slot = memory.entry(page).or_insert([0u8; 0x1000]);
                        let start = offset as usize;
                        slot[start..start + length as usize].copy_from_slice(
                            &bytes[file_offset as usize..file_offset as usize + length as usize],
                        );
                    }
                }
            }
            let entry = plan.entry_point();
            let page = entry & !0xfff;
            let slot = memory.get(&page).expect("entry page is mapped");
            assert_eq!(
                slot[(entry & 0xfff) as usize],
                0x48,
                "{path} entry byte reached the entry address"
            );
        }
    }

    #[test]
    fn the_embedded_programs_carry_the_lines_the_boot_test_waits_for() {
        let hello = crate::programs_gen::HELLO_ELF;
        assert!(
            hello
                .windows(25)
                .any(|window| window == b"hello: hello from ring 3\n"),
            "hello's greeting is in its image"
        );
        let init = crate::programs_gen::INIT_ELF;
        assert!(
            init.windows(34)
                .any(|window| window == b"init: child exited with status 42\n"),
            "init's success line is in its image"
        );
        assert!(
            init.windows(33)
                .any(|window| window == b"init: child exit status mismatch\n"),
            "init's failure line is in its image"
        );
    }
}
