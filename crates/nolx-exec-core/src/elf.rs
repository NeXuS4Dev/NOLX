//! Bounded validation of x86-64 ELF executable images before the kernel maps user pages.
//!
//! This module deliberately produces a load plan rather than touching page tables or
//! allocating memory. It accepts fixed-address `ET_EXEC` images and position-independent
//! `ET_DYN` images and records the program headers used by the next loader stage.
//! Page mapping, relocation application, auxv construction, and the user-mode transition
//! are in [`crate::image`]; this module decides whether an image may be loaded at all.

use nolx_mm_core::vad::{USER_SPACE_HIGH, USER_SPACE_LOW};

const ELF_HEADER_SIZE: usize = 64;
const ELF_HEADER_SIZE_U16: u16 = 64;
#[cfg(test)]
const ELF_HEADER_SIZE_U64: u64 = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const PROGRAM_HEADER_SIZE_U16: u16 = 56;
const PROGRAM_HEADER_SIZE_U64: u64 = 56;
const ELF_MAGIC: &[u8; 4] = b"\x7fELF";
const ELFCLASS64: u8 = 2;
const ELFDATA2LSB: u8 = 1;
const EV_CURRENT: u32 = 1;
const EV_CURRENT_IDENT: u8 = 1;
const ET_EXEC: u16 = 2;
const ET_DYN: u16 = 3;
const EM_X86_64: u16 = 62;
const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const PT_INTERP: u32 = 3;
const PT_TLS: u32 = 7;
const PT_GNU_STACK: u32 = 0x6474_e551;
const PF_W: u32 = 2;
const PF_X: u32 = 1;
const MAX_PROGRAM_HEADERS: usize = 128;
const MAX_LOAD_SEGMENTS: usize = 32;

/// Why an ELF64 image could not be converted into a load plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfError {
    /// The file is shorter than the fixed ELF64 header.
    FileTooShort,
    /// The file does not have the ELF magic bytes.
    BadMagic,
    /// The image is not ELF class 64-bit.
    UnsupportedClass,
    /// The image is not little-endian, as required by x86-64.
    UnsupportedEndian,
    /// The ELF identification or header version is not the current version.
    UnsupportedVersion,
    /// The image is not an x86-64 executable (`ET_EXEC`) or shared object (`ET_DYN`).
    UnsupportedTypeOrMachine,
    /// The ELF header does not have the ELF64 header size.
    InvalidHeaderSize,
    /// Program headers do not have the ELF64 program-header size.
    InvalidProgramHeaderSize,
    /// The image has no program headers.
    NoProgramHeaders,
    /// The program-header table exceeds the parser's fixed bound.
    TooManyProgramHeaders,
    /// The program-header table extends outside the input file.
    ProgramHeaderTableOutOfBounds,
    /// More loadable segments were found than the fixed plan can hold.
    TooManyLoadSegments,
    /// The image has no nonempty loadable segments.
    EmptyLoadImage,
    /// A file-backed segment range extends outside the input file.
    FileRangeOutOfBounds,
    /// A segment's file size is larger than its memory size.
    SegmentSizeInconsistent,
    /// Address arithmetic overflowed while checking a segment.
    AddressOverflow,
    /// A segment falls outside the process user-address interval.
    SegmentOutsideUserRange,
    /// Segment alignment is invalid or its file/address offsets do not agree.
    InvalidSegmentAlignment,
    /// A load segment would be both writable and executable.
    WritableExecutableSegment,
    /// Two load segments overlap in virtual memory.
    OverlappingLoadSegments,
    /// The entry point is not inside an executable load segment.
    EntryPointNotExecutable,
    /// More than one `PT_INTERP` header was present.
    DuplicateInterpreter,
    /// The `PT_INTERP` payload is not one nonempty NUL-terminated path.
    InvalidInterpreter,
    /// More than one `PT_TLS` header was present.
    DuplicateTlsSegment,
    /// The TLS segment is malformed or not contained by a load segment.
    InvalidTlsSegment,
    /// More than one `PT_GNU_STACK` header was present.
    DuplicateGnuStack,
    /// More than one `PT_DYNAMIC` header was present.
    DuplicateDynamic,
    /// The `PT_DYNAMIC` payload lies outside every load segment.
    DynamicOutsideLoadSegments,
}

/// Which ELF object type the image carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElfKind {
    /// Fixed-address executable: its virtual addresses are absolute.
    Exec,
    /// Position-independent object: its virtual addresses are relative and need a slide.
    Dyn,
}

/// Permission flags from an ELF `PT_LOAD` program header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentFlags(u32);

impl SegmentFlags {
    /// Whether the segment has the ELF read permission.
    #[must_use]
    pub const fn readable(self) -> bool {
        self.0 & 4 != 0
    }

    /// Whether the segment has the ELF write permission.
    #[must_use]
    pub const fn writable(self) -> bool {
        self.0 & PF_W != 0
    }

    /// Whether the segment has the ELF execute permission.
    #[must_use]
    pub const fn executable(self) -> bool {
        self.0 & PF_X != 0
    }
}

/// A validated, half-open `PT_LOAD` memory range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadSegment {
    file_offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
    flags: SegmentFlags,
}

impl LoadSegment {
    /// Offset of the file-backed bytes in the ELF image.
    #[must_use]
    pub const fn file_offset(self) -> u64 {
        self.file_offset
    }

    /// First virtual address in the segment.
    #[must_use]
    pub const fn virtual_address(self) -> u64 {
        self.virtual_address
    }

    /// Number of initialized bytes copied from the file.
    #[must_use]
    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    /// Number of bytes mapped for the segment, including zero-filled BSS.
    #[must_use]
    pub const fn memory_size(self) -> u64 {
        self.memory_size
    }

    /// Required segment alignment, or zero/one when no alignment is specified.
    #[must_use]
    pub const fn alignment(self) -> u64 {
        self.alignment
    }

    /// Access permissions declared by the ELF image.
    #[must_use]
    pub const fn flags(self) -> SegmentFlags {
        self.flags
    }

    /// One byte past the end of this segment's validated virtual-memory range.
    #[must_use]
    pub const fn end_address(self) -> u64 {
        self.virtual_address + self.memory_size
    }
}

/// The thread-local storage template described by a `PT_TLS` program header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TlsSegment {
    file_offset: u64,
    virtual_address: u64,
    file_size: u64,
    memory_size: u64,
    alignment: u64,
}

impl TlsSegment {
    /// Offset of initialized TLS bytes in the ELF image.
    #[must_use]
    pub const fn file_offset(self) -> u64 {
        self.file_offset
    }

    /// Virtual address corresponding to the TLS template.
    #[must_use]
    pub const fn virtual_address(self) -> u64 {
        self.virtual_address
    }

    /// Number of initialized TLS bytes.
    #[must_use]
    pub const fn file_size(self) -> u64 {
        self.file_size
    }

    /// Total per-thread TLS size, including zero-filled bytes.
    #[must_use]
    pub const fn memory_size(self) -> u64 {
        self.memory_size
    }

    /// Required TLS alignment.
    #[must_use]
    pub const fn alignment(self) -> u64 {
        self.alignment
    }
}

/// Validated metadata needed by the next stage of the ELF loader.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Elf64LoadPlan<'a> {
    kind: ElfKind,
    entry_point: u64,
    image_base: u64,
    image_end: u64,
    segments: [Option<LoadSegment>; MAX_LOAD_SEGMENTS],
    segment_count: usize,
    interpreter: Option<&'a [u8]>,
    tls: Option<TlsSegment>,
    stack_executable: bool,
    dynamic: Option<DynamicSegment>,
    program_header_address: Option<u64>,
    program_header_count: u16,
}

/// The `PT_DYNAMIC` file range, kept for relocation discovery in [`crate::image`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DynamicSegment {
    file_offset: u64,
    file_size: u64,
}

impl DynamicSegment {
    /// Offset of the dynamic table in the file.
    #[must_use]
    pub const fn file_offset(self) -> u64 {
        self.file_offset
    }

    /// Size in bytes of the dynamic table in the file.
    #[must_use]
    pub const fn file_size(self) -> u64 {
        self.file_size
    }
}

impl<'a> Elf64LoadPlan<'a> {
    /// Validate a little-endian x86-64 `ET_EXEC` or `ET_DYN` image without allocating or
    /// mapping pages.
    pub fn parse(image: &'a [u8]) -> Result<Self, ElfError> {
        if image.len() < ELF_HEADER_SIZE {
            return Err(ElfError::FileTooShort);
        }
        if &image[..4] != ELF_MAGIC {
            return Err(ElfError::BadMagic);
        }
        if image[4] != ELFCLASS64 {
            return Err(ElfError::UnsupportedClass);
        }
        if image[5] != ELFDATA2LSB {
            return Err(ElfError::UnsupportedEndian);
        }
        if image[6] != EV_CURRENT_IDENT || read_u32(image, 20)? != EV_CURRENT {
            return Err(ElfError::UnsupportedVersion);
        }
        let kind = match read_u16(image, 16)? {
            ET_EXEC => ElfKind::Exec,
            ET_DYN => ElfKind::Dyn,
            _ => return Err(ElfError::UnsupportedTypeOrMachine),
        };
        if read_u16(image, 18)? != EM_X86_64 {
            return Err(ElfError::UnsupportedTypeOrMachine);
        }
        if read_u16(image, 52)? != ELF_HEADER_SIZE_U16 {
            return Err(ElfError::InvalidHeaderSize);
        }
        if read_u16(image, 54)? != PROGRAM_HEADER_SIZE_U16 {
            return Err(ElfError::InvalidProgramHeaderSize);
        }

        let program_header_offset = read_u64(image, 32)?;
        let program_header_count_u16 = read_u16(image, 56)?;
        let program_header_count = usize::from(program_header_count_u16);
        if program_header_count == 0 {
            return Err(ElfError::NoProgramHeaders);
        }
        if program_header_count > MAX_PROGRAM_HEADERS {
            return Err(ElfError::TooManyProgramHeaders);
        }
        let table_size = u64::from(program_header_count_u16)
            .checked_mul(PROGRAM_HEADER_SIZE_U64)
            .ok_or(ElfError::ProgramHeaderTableOutOfBounds)?;
        let program_headers = file_range(image, program_header_offset, table_size)
            .map_err(|_| ElfError::ProgramHeaderTableOutOfBounds)?;

        let entry_point = read_u64(image, 24)?;
        let mut segments = [None; MAX_LOAD_SEGMENTS];
        let mut segment_count = 0;
        let mut interpreter = None;
        let mut tls = None;
        let mut dynamic = None;
        let mut stack_executable = false;
        let mut saw_gnu_stack = false;
        let mut image_base = u64::MAX;
        let mut image_end = 0;

        for index in 0..program_header_count {
            let start = index * PROGRAM_HEADER_SIZE;
            let header = program_headers
                .get(start..start + PROGRAM_HEADER_SIZE)
                .ok_or(ElfError::ProgramHeaderTableOutOfBounds)?;
            let phdr_kind = read_u32(header, 0)?;
            let flags = read_u32(header, 4)?;
            let file_offset = read_u64(header, 8)?;
            let virtual_address = read_u64(header, 16)?;
            let file_size = read_u64(header, 32)?;
            let memory_size = read_u64(header, 40)?;
            let alignment = read_u64(header, 48)?;

            match phdr_kind {
                PT_LOAD => {
                    if file_size > memory_size {
                        return Err(ElfError::SegmentSizeInconsistent);
                    }
                    file_range(image, file_offset, file_size)?;
                    validate_alignment(file_offset, virtual_address, alignment)?;
                    if memory_size == 0 {
                        continue;
                    }
                    let end = virtual_address
                        .checked_add(memory_size)
                        .ok_or(ElfError::AddressOverflow)?;
                    validate_image_range(kind, virtual_address, end)?;
                    if flags & PF_W != 0 && flags & PF_X != 0 {
                        return Err(ElfError::WritableExecutableSegment);
                    }
                    let segment = LoadSegment {
                        file_offset,
                        virtual_address,
                        file_size,
                        memory_size,
                        alignment,
                        flags: SegmentFlags(flags),
                    };
                    if segments.iter().flatten().any(|existing: &LoadSegment| {
                        segment.virtual_address < existing.end_address()
                            && existing.virtual_address < end
                    }) {
                        return Err(ElfError::OverlappingLoadSegments);
                    }
                    if segment_count == MAX_LOAD_SEGMENTS {
                        return Err(ElfError::TooManyLoadSegments);
                    }
                    segments[segment_count] = Some(segment);
                    segment_count += 1;
                    image_base = image_base.min(virtual_address);
                    image_end = image_end.max(end);
                }
                PT_INTERP => {
                    if interpreter.is_some() {
                        return Err(ElfError::DuplicateInterpreter);
                    }
                    let path = file_range(image, file_offset, file_size)?;
                    if path.len() < 2
                        || path.last() != Some(&0)
                        || path[..path.len() - 1].contains(&0)
                    {
                        return Err(ElfError::InvalidInterpreter);
                    }
                    interpreter = Some(&path[..path.len() - 1]);
                }
                PT_TLS => {
                    if tls.is_some() {
                        return Err(ElfError::DuplicateTlsSegment);
                    }
                    if file_size > memory_size || memory_size == 0 {
                        return Err(ElfError::InvalidTlsSegment);
                    }
                    file_range(image, file_offset, file_size)
                        .map_err(|_| ElfError::InvalidTlsSegment)?;
                    validate_alignment(file_offset, virtual_address, alignment)
                        .map_err(|_| ElfError::InvalidTlsSegment)?;
                    let end = virtual_address
                        .checked_add(memory_size)
                        .ok_or(ElfError::InvalidTlsSegment)?;
                    validate_image_range(kind, virtual_address, end)
                        .map_err(|_| ElfError::InvalidTlsSegment)?;
                    tls = Some(TlsSegment {
                        file_offset,
                        virtual_address,
                        file_size,
                        memory_size,
                        alignment,
                    });
                }
                PT_DYNAMIC => {
                    if dynamic.is_some() {
                        return Err(ElfError::DuplicateDynamic);
                    }
                    if file_size == 0 {
                        continue;
                    }
                    file_range(image, file_offset, file_size)?;
                    let end = virtual_address
                        .checked_add(file_size)
                        .ok_or(ElfError::AddressOverflow)?;
                    if end <= virtual_address {
                        return Err(ElfError::AddressOverflow);
                    }
                    dynamic = Some(DynamicSegment {
                        file_offset,
                        file_size,
                    });
                }
                PT_GNU_STACK => {
                    if saw_gnu_stack {
                        return Err(ElfError::DuplicateGnuStack);
                    }
                    saw_gnu_stack = true;
                    stack_executable = flags & PF_X != 0;
                }
                _ => {}
            }
        }

        if segment_count == 0 {
            return Err(ElfError::EmptyLoadImage);
        }
        if !segments.iter().flatten().any(|segment| {
            segment.flags.executable()
                && entry_point >= segment.virtual_address
                && entry_point < segment.end_address()
        }) {
            return Err(ElfError::EntryPointNotExecutable);
        }
        if let Some(tls_segment) = tls {
            validate_tls_containment(tls_segment, &segments)?;
        }

        // The dynamic table is read from the file directly, but the tags inside it are
        // virtual addresses, so the table itself must live where the file-to-virtual
        // translation is defined: inside a load segment.
        if let Some(table) = dynamic {
            let table_end = table.file_offset + table.file_size;
            let contained = segments.iter().flatten().any(|segment| {
                let segment_file_end = segment.file_offset + segment.file_size;
                segment.file_offset <= table.file_offset && table_end <= segment_file_end
            });
            if !contained {
                return Err(ElfError::DynamicOutsideLoadSegments);
            }
        }

        let program_header_address = segments.iter().flatten().find_map(|segment| {
            let segment_file_end = segment.file_offset + segment.file_size;
            if segment.file_offset <= program_header_offset
                && program_header_offset + table_size <= segment_file_end
            {
                Some(segment.virtual_address + (program_header_offset - segment.file_offset))
            } else {
                None
            }
        });

        Ok(Self {
            kind,
            entry_point,
            image_base,
            image_end,
            segments,
            segment_count,
            interpreter,
            tls,
            stack_executable,
            dynamic,
            program_header_address,
            program_header_count: program_header_count_u16,
        })
    }

    /// Whether the image is `ET_EXEC` or `ET_DYN`.
    #[must_use]
    pub const fn kind(&self) -> ElfKind {
        self.kind
    }

    /// The validated initial instruction pointer.
    #[must_use]
    pub const fn entry_point(&self) -> u64 {
        self.entry_point
    }

    /// Lowest virtual address covered by a load segment.
    #[must_use]
    pub const fn image_base(&self) -> u64 {
        self.image_base
    }

    /// One byte past the highest virtual address covered by a load segment.
    #[must_use]
    pub const fn image_end(&self) -> u64 {
        self.image_end
    }

    /// Validated load segments in the order they appeared in the ELF table.
    pub fn segments(&self) -> impl Iterator<Item = &LoadSegment> {
        self.segments[..self.segment_count]
            .iter()
            .filter_map(Option::as_ref)
    }

    /// The validated dynamic interpreter path, if the image declares `PT_INTERP`.
    #[must_use]
    pub const fn interpreter(&self) -> Option<&'a [u8]> {
        self.interpreter
    }

    /// The validated thread-local storage template, if the image declares `PT_TLS`.
    #[must_use]
    pub const fn tls(&self) -> Option<TlsSegment> {
        self.tls
    }

    /// Whether `PT_GNU_STACK` explicitly requests an executable initial stack.
    ///
    /// When the header is absent, the value is false so a caller can choose a non-executable
    /// stack by default.
    #[must_use]
    pub const fn stack_executable(&self) -> bool {
        self.stack_executable
    }

    /// The `PT_DYNAMIC` file range, when the image carries a dynamic table.
    #[must_use]
    pub const fn dynamic(&self) -> Option<DynamicSegment> {
        self.dynamic
    }

    /// Virtual address of the program-header table once loaded, when it lies inside a load
    /// segment. `AT_PHDR` is built from this.
    #[must_use]
    pub const fn program_headers_address(&self) -> Option<u64> {
        self.program_header_address
    }

    /// Number of program-header entries, for `AT_PHNUM`.
    #[must_use]
    pub const fn program_header_count(&self) -> u16 {
        self.program_header_count
    }

    /// Size of one program-header entry, for `AT_PHENT`.
    #[must_use]
    pub const fn program_header_entry_size(&self) -> u16 {
        PROGRAM_HEADER_SIZE_U16
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, ElfError> {
    let end = offset.checked_add(2).ok_or(ElfError::FileTooShort)?;
    let value = bytes.get(offset..end).ok_or(ElfError::FileTooShort)?;
    Ok(u16::from_le_bytes([value[0], value[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, ElfError> {
    let end = offset.checked_add(4).ok_or(ElfError::FileTooShort)?;
    let value = bytes.get(offset..end).ok_or(ElfError::FileTooShort)?;
    Ok(u32::from_le_bytes([value[0], value[1], value[2], value[3]]))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, ElfError> {
    let end = offset.checked_add(8).ok_or(ElfError::FileTooShort)?;
    let value = bytes.get(offset..end).ok_or(ElfError::FileTooShort)?;
    Ok(u64::from_le_bytes([
        value[0], value[1], value[2], value[3], value[4], value[5], value[6], value[7],
    ]))
}

fn file_range(image: &[u8], offset: u64, size: u64) -> Result<&[u8], ElfError> {
    let start = usize::try_from(offset).map_err(|_| ElfError::FileRangeOutOfBounds)?;
    let length = usize::try_from(size).map_err(|_| ElfError::FileRangeOutOfBounds)?;
    let end = start
        .checked_add(length)
        .ok_or(ElfError::FileRangeOutOfBounds)?;
    image.get(start..end).ok_or(ElfError::FileRangeOutOfBounds)
}

fn validate_alignment(
    file_offset: u64,
    virtual_address: u64,
    alignment: u64,
) -> Result<(), ElfError> {
    if alignment > 1
        && (!alignment.is_power_of_two() || file_offset % alignment != virtual_address % alignment)
    {
        return Err(ElfError::InvalidSegmentAlignment);
    }
    Ok(())
}

fn validate_image_range(kind: ElfKind, start: u64, end: u64) -> Result<(), ElfError> {
    if start >= end {
        return Err(ElfError::SegmentOutsideUserRange);
    }
    match kind {
        // Fixed addresses must already lie in the process user interval.
        ElfKind::Exec => {
            if start < USER_SPACE_LOW || end > USER_SPACE_HIGH {
                return Err(ElfError::SegmentOutsideUserRange);
            }
        }
        // `ET_DYN` addresses are relative to the load bias; only the span is checkable
        // here, and [`crate::image`] re-checks the biased range when it picks the slide.
        ElfKind::Dyn => {
            if end > USER_SPACE_HIGH || end - start > USER_SPACE_HIGH - USER_SPACE_LOW {
                return Err(ElfError::SegmentOutsideUserRange);
            }
        }
    }
    Ok(())
}

fn validate_tls_containment(
    tls: TlsSegment,
    segments: &[Option<LoadSegment>; MAX_LOAD_SEGMENTS],
) -> Result<(), ElfError> {
    let tls_end = tls
        .virtual_address
        .checked_add(tls.memory_size)
        .ok_or(ElfError::InvalidTlsSegment)?;
    let tls_file_end = tls
        .file_offset
        .checked_add(tls.file_size)
        .ok_or(ElfError::InvalidTlsSegment)?;
    let contained = segments.iter().flatten().any(|segment| {
        let segment_file_end = segment.file_offset.checked_add(segment.file_size);
        segment.virtual_address <= tls.virtual_address
            && tls_end <= segment.end_address()
            && (tls.file_size == 0
                || (segment.file_offset <= tls.file_offset
                    && segment_file_end.is_some_and(|end| tls_file_end <= end)
                    && tls.virtual_address - segment.virtual_address
                        == tls.file_offset - segment.file_offset))
    });
    if contained {
        Ok(())
    } else {
        Err(ElfError::InvalidTlsSegment)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE_SIZE: usize = 0x400;
    const TEXT_OFFSET: u64 = 0x100;
    const DATA_OFFSET: u64 = 0x200;
    const TEXT_ADDRESS: u64 = 0x0040_0000;
    const DATA_ADDRESS: u64 = 0x0040_1000;

    fn valid_image() -> Vec<u8> {
        let mut image = vec![0; FILE_SIZE];
        image[..4].copy_from_slice(ELF_MAGIC);
        image[4] = ELFCLASS64;
        image[5] = ELFDATA2LSB;
        image[6] = EV_CURRENT_IDENT;
        put_u16(&mut image, 16, ET_EXEC);
        put_u16(&mut image, 18, EM_X86_64);
        put_u32(&mut image, 20, EV_CURRENT);
        put_u64(&mut image, 24, TEXT_ADDRESS);
        put_u64(&mut image, 32, ELF_HEADER_SIZE_U64);
        put_u16(&mut image, 52, ELF_HEADER_SIZE_U16);
        put_u16(&mut image, 54, PROGRAM_HEADER_SIZE_U16);
        put_u16(&mut image, 56, 2);

        put_program_header(
            &mut image,
            0,
            TestProgramHeader {
                kind: PT_LOAD,
                flags: 5,
                file_offset: TEXT_OFFSET,
                virtual_address: TEXT_ADDRESS,
                file_size: 0x20,
                memory_size: 0x100,
                alignment: 0x10,
            },
        );
        put_program_header(
            &mut image,
            1,
            TestProgramHeader {
                kind: PT_LOAD,
                flags: 6,
                file_offset: DATA_OFFSET,
                virtual_address: DATA_ADDRESS,
                file_size: 0x10,
                memory_size: 0x30,
                alignment: 0x10,
            },
        );
        image[0x100] = 0x90;
        image
    }

    struct TestProgramHeader {
        kind: u32,
        flags: u32,
        file_offset: u64,
        virtual_address: u64,
        file_size: u64,
        memory_size: u64,
        alignment: u64,
    }

    fn put_program_header(image: &mut [u8], index: usize, header: TestProgramHeader) {
        let offset = ELF_HEADER_SIZE + index * PROGRAM_HEADER_SIZE;
        put_u32(image, offset, header.kind);
        put_u32(image, offset + 4, header.flags);
        put_u64(image, offset + 8, header.file_offset);
        put_u64(image, offset + 16, header.virtual_address);
        put_u64(image, offset + 32, header.file_size);
        put_u64(image, offset + 40, header.memory_size);
        put_u64(image, offset + 48, header.alignment);
    }

    fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn plans_nonoverlapping_executable_and_data_segments() {
        let image = valid_image();
        let plan = Elf64LoadPlan::parse(&image).expect("valid static ELF image");
        let segments: Vec<_> = plan.segments().copied().collect();

        assert_eq!(plan.entry_point(), TEXT_ADDRESS);
        assert_eq!(plan.image_base(), TEXT_ADDRESS);
        assert_eq!(plan.image_end(), DATA_ADDRESS + 0x30);
        assert_eq!(segments.len(), 2);
        assert!(segments[0].flags().readable());
        assert!(segments[0].flags().executable());
        assert!(!segments[0].flags().writable());
        assert_eq!(segments[0].file_size(), 0x20);
        assert_eq!(segments[0].memory_size(), 0x100);
        assert!(segments[1].flags().writable());
        assert!(!plan.stack_executable());
        assert_eq!(plan.interpreter(), None);
        assert_eq!(plan.tls(), None);
    }

    #[test]
    fn rejects_short_bad_magic_and_unsupported_images() {
        assert_eq!(Elf64LoadPlan::parse(&[0; 20]), Err(ElfError::FileTooShort));

        let mut image = valid_image();
        image[0] = 0;
        assert_eq!(Elf64LoadPlan::parse(&image), Err(ElfError::BadMagic));

        let mut image = valid_image();
        image[4] = 1;
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::UnsupportedClass)
        );

        let mut image = valid_image();
        image[5] = 2;
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::UnsupportedEndian)
        );

        let mut image = valid_image();
        image[6] = 0;
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::UnsupportedVersion)
        );

        let mut image = valid_image();
        put_u16(&mut image, 52, 63);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::InvalidHeaderSize)
        );

        let mut image = valid_image();
        put_u16(&mut image, 54, 55);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::InvalidProgramHeaderSize)
        );

        let mut image = valid_image();
        put_u16(&mut image, 18, 3);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::UnsupportedTypeOrMachine)
        );

        let mut image = valid_image();
        put_u16(&mut image, 16, 4); // ET_CORE
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::UnsupportedTypeOrMachine)
        );

        // `ET_DYN` is accepted: the loader biases it in the second stage.
        let mut image = valid_image();
        put_u16(&mut image, 16, 3);
        assert_eq!(Elf64LoadPlan::parse(&image).unwrap().kind(), ElfKind::Dyn);
    }

    #[test]
    fn rejects_program_header_table_outside_the_file() {
        let mut image = valid_image();
        put_u64(&mut image, 32, u64::MAX);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::ProgramHeaderTableOutOfBounds)
        );
    }

    #[test]
    fn rejects_missing_or_unbounded_program_header_tables() {
        let mut image = valid_image();
        put_u16(&mut image, 56, 0);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::NoProgramHeaders)
        );

        let mut image = valid_image();
        put_u16(&mut image, 56, 129);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::TooManyProgramHeaders)
        );
    }

    #[test]
    fn rejects_more_load_segments_than_the_fixed_plan_holds() {
        let count = MAX_LOAD_SEGMENTS + 1;
        let mut image = vec![0; ELF_HEADER_SIZE + count * PROGRAM_HEADER_SIZE];
        image[..4].copy_from_slice(ELF_MAGIC);
        image[4] = ELFCLASS64;
        image[5] = ELFDATA2LSB;
        image[6] = EV_CURRENT_IDENT;
        put_u16(&mut image, 16, ET_EXEC);
        put_u16(&mut image, 18, EM_X86_64);
        put_u32(&mut image, 20, EV_CURRENT);
        put_u64(&mut image, 24, 0x0050_0000);
        put_u64(&mut image, 32, ELF_HEADER_SIZE_U64);
        put_u16(&mut image, 52, ELF_HEADER_SIZE_U16);
        put_u16(&mut image, 54, PROGRAM_HEADER_SIZE_U16);
        put_u16(&mut image, 56, 33);

        for index in 0..count {
            put_program_header(
                &mut image,
                index,
                TestProgramHeader {
                    kind: PT_LOAD,
                    flags: if index == 0 { 5 } else { 4 },
                    file_offset: 0,
                    virtual_address: 0x0050_0000 + index as u64 * 0x1000,
                    file_size: 0,
                    memory_size: 0x100,
                    alignment: 1,
                },
            );
        }
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::TooManyLoadSegments)
        );
    }

    #[test]
    fn rejects_inconsistent_sizes_and_writable_executable_segments() {
        let mut image = valid_image();
        let first_header = ELF_HEADER_SIZE;
        put_u64(&mut image, first_header + 32, 0x101);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::SegmentSizeInconsistent)
        );

        let mut image = valid_image();
        put_u32(&mut image, first_header + 4, 7);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::WritableExecutableSegment)
        );

        let mut image = valid_image();
        put_u64(&mut image, first_header + 8, 0x3f0);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::FileRangeOutOfBounds)
        );

        let mut image = valid_image();
        put_u64(&mut image, first_header + 48, 3);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::InvalidSegmentAlignment)
        );
    }

    #[test]
    fn rejects_out_of_user_range_overlapping_and_non_executable_entry() {
        let mut image = valid_image();
        put_u64(&mut image, ELF_HEADER_SIZE + 16, USER_SPACE_HIGH - 0x80);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::SegmentOutsideUserRange)
        );

        let mut image = valid_image();
        let second_header = ELF_HEADER_SIZE + PROGRAM_HEADER_SIZE;
        put_u64(&mut image, second_header + 16, TEXT_ADDRESS + 0x80);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::OverlappingLoadSegments)
        );

        let mut image = valid_image();
        put_u64(&mut image, 24, DATA_ADDRESS);
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::EntryPointNotExecutable)
        );

        let mut image = valid_image();
        let first_header = ELF_HEADER_SIZE;
        put_u64(&mut image, first_header + 16, u64::MAX - 0x7f);
        put_u64(&mut image, first_header + 48, 1);
        assert_eq!(Elf64LoadPlan::parse(&image), Err(ElfError::AddressOverflow));
    }

    #[test]
    fn records_interpreter_tls_and_executable_stack_metadata() {
        let mut image = valid_image();
        put_u16(&mut image, 56, 5);
        put_program_header(
            &mut image,
            2,
            TestProgramHeader {
                kind: PT_INTERP,
                flags: 0,
                file_offset: 0x280,
                virtual_address: 0,
                file_size: 16,
                memory_size: 16,
                alignment: 1,
            },
        );
        image[0x280..0x290].copy_from_slice(b"/lib/ld-nolx.so\0");
        put_program_header(
            &mut image,
            3,
            TestProgramHeader {
                kind: PT_TLS,
                flags: 4,
                file_offset: DATA_OFFSET,
                virtual_address: DATA_ADDRESS,
                file_size: 0x10,
                memory_size: 0x30,
                alignment: 0x10,
            },
        );
        put_program_header(
            &mut image,
            4,
            TestProgramHeader {
                kind: PT_GNU_STACK,
                flags: PF_W | PF_X,
                file_offset: 0,
                virtual_address: 0,
                file_size: 0,
                memory_size: 0,
                alignment: 16,
            },
        );

        let plan = Elf64LoadPlan::parse(&image).expect("valid interpreter and TLS metadata");
        assert_eq!(plan.interpreter(), Some(&b"/lib/ld-nolx.so"[..]));
        assert_eq!(
            plan.tls(),
            Some(TlsSegment {
                file_offset: DATA_OFFSET,
                virtual_address: DATA_ADDRESS,
                file_size: 0x10,
                memory_size: 0x30,
                alignment: 0x10,
            })
        );
        assert!(plan.stack_executable());
    }

    #[test]
    fn rejects_malformed_interpreter_path() {
        let mut image = valid_image();
        put_u16(&mut image, 56, 3);
        put_program_header(
            &mut image,
            2,
            TestProgramHeader {
                kind: PT_INTERP,
                flags: 0,
                file_offset: 0x280,
                virtual_address: 0,
                file_size: 5,
                memory_size: 5,
                alignment: 1,
            },
        );
        image[0x280..0x285].copy_from_slice(b"ld.so");
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::InvalidInterpreter)
        );
    }

    #[test]
    fn rejects_tls_not_backed_by_a_load_segment() {
        let mut image = valid_image();
        put_u16(&mut image, 56, 3);
        put_program_header(
            &mut image,
            2,
            TestProgramHeader {
                kind: PT_TLS,
                flags: 4,
                file_offset: DATA_OFFSET,
                virtual_address: DATA_ADDRESS + 0x100,
                file_size: 0x10,
                memory_size: 0x20,
                alignment: 0x10,
            },
        );
        assert_eq!(
            Elf64LoadPlan::parse(&image),
            Err(ElfError::InvalidTlsSegment)
        );
    }
}
