//! Loading an ELF image into a ring-3 address space, page by page, through the direct map.
//!
//! [`nolx_exec_core::elf`] validates the image and [`nolx_exec_core::image`] turns it into
//! page operations, relocations, and an initial stack image. This module executes those in
//! the kernel's address space machinery *without activating the target space*: each page is
//! demand-zeroed through the VAD/commit rules and then written through the physical direct
//! map, which is what lets `posix_spawn` build a child's space while the parent's space is
//! the one currently in `CR3`.
//!
//! Every byte written here lands in a page the space's own commit rules already approved,
//! and every byte read for a syscall passes [`validate_user_range`] first.

use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicU64, Ordering};

use nolx_exec_core::elf::Elf64LoadPlan;
use nolx_exec_core::image::{self, ImageError, PageOp, StackRequest, STACK_IMAGE_MAX};
use nolx_mm_core::paging::Protection;
use nolx_mm_core::vad::{VadFlags, VadKind, VadRequest};
use nolx_types::abi::{cld, Siginfo, MAX_ARG_COUNT, SIGCHLD};

use crate::mm::space::Space;
use crate::mm::{phys, PAGE_SIZE};
use crate::sync::SyncCell;

use super::{STACK_FLOOR, STACK_TOP, USER_STACK_SIZE};

/// Entry point description the loader produced.
pub struct UserEntry {
    /// The initial instruction pointer.
    pub rip: u64,
    /// The initial stack pointer.
    pub rsp: u64,
}

/// Maximum arguments one `posix_spawn` passes in M6, in kernel temporary storage.
pub const SPAWN_MAX_ARGS: usize = 8;

/// Maximum bytes of one `posix_spawn` argument string in M6.
pub const SPAWN_MAX_STRING: usize = 128;

/// One bounded user string copied out of user space.
#[derive(Clone, Copy)]
pub struct UserString {
    bytes: [u8; SPAWN_MAX_STRING],
    len: usize,
}

impl UserString {
    /// The bytes without their terminating NUL.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

/// The bounded argument set a `posix_spawn` call carries.
#[derive(Clone, Copy)]
pub struct UserArgv {
    strings: [UserString; SPAWN_MAX_ARGS],
    len: usize,
}

impl UserArgv {
    /// The arguments as byte strings, in order.
    pub fn as_slices(&self) -> [&[u8]; SPAWN_MAX_ARGS] {
        let mut out = [&self.strings[0].bytes[..0]; SPAWN_MAX_ARGS];
        for (index, slot) in out.iter_mut().enumerate().take(self.len) {
            *slot = self.strings[index].as_bytes();
        }
        out
    }

    /// How many arguments the set holds.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Whether the set is empty.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Why an image could not be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The ELF validation refused the image.
    Elf(nolx_exec_core::elf::ElfError),
    /// The second stage refused the load work.
    Image(ImageError),
    /// A VAD could not be reserved: the range collides or exceeds the commit charge.
    Reserve,
    /// A page could not be brought in under the space's rules.
    Page,
    /// A required user address is outside every VAD, or not readable/writable as asked.
    UserPointer,
    /// The program arguments do not fit the loader's bounds.
    Arguments,
}

impl From<nolx_exec_core::elf::ElfError> for LoadError {
    fn from(value: nolx_exec_core::elf::ElfError) -> Self {
        Self::Elf(value)
    }
}

impl From<ImageError> for LoadError {
    fn from(value: ImageError) -> Self {
        Self::Image(value)
    }
}

/// Scratch buffer the stack image is assembled in before it is copied to the user pages.
static STACK_IMAGE: SyncCell<MaybeUninit<[u8; STACK_IMAGE_MAX]>> =
    SyncCell::new(core::mem::MaybeUninit::uninit());

/// Entropy source for `AT_RANDOM` and the `ET_DYN` slide: the timestamp counter mixed
/// with a counter that never repeats. Not a CSPRNG; it does not need to be.
static ENTROPY: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);

fn entropy() -> u64 {
    let mix = ENTROPY.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    // SAFETY: `rdtsc` is available on every x86-64 CPU and has no side effects.
    let tsc = unsafe {
        let low: u32;
        let high: u32;
        core::arch::asm!("rdtsc", out("eax") low, out("edx") high, options(nomem, nostack));
        ((high as u64) << 32) | low as u64
    };
    tsc.rotate_left(17) ^ mix
}

/// The physical address behind one page of `space`, demand-zeroed if it was not resident.
///
/// `resolve_here` resolves *faults*: an already-present page is a `Protection` refusal to
/// it (a fault on a present page can only be a permission fault). So the resident case is
/// answered from `translate` first, and only a missing page goes through the fault path --
/// which is exactly what the zero-fill-then-copy order of the page plan needs.
fn resident_frame(space: &mut Space, address: u64) -> Result<u64, LoadError> {
    let page = address & !(PAGE_SIZE as u64 - 1);
    if let Some(mapping) = space.translate(page) {
        return Ok(mapping.physical);
    }
    space.resolve_here(page, 0).map_err(|_| LoadError::Page)?;
    space
        .translate(page)
        .map(|mapping| mapping.physical)
        .ok_or(LoadError::Page)
}

/// Copy `bytes` to user `address` through the direct map, demand-paging as it goes.
pub(crate) fn write_user(space: &mut Space, address: u64, bytes: &[u8]) -> Result<(), LoadError> {
    let mut done = 0usize;
    while done < bytes.len() {
        let current = address + done as u64;
        let frame = resident_frame(space, current)?;
        let offset = (current & (PAGE_SIZE as u64 - 1)) as usize;
        let chunk = (PAGE_SIZE - offset).min(bytes.len() - done);
        // SAFETY: the frame was just resolved in `space` and is exclusively referenced by
        // it; the direct map is the kernel's view of every frame.
        unsafe {
            let target = phys::direct_ptr::<u8>(frame + offset as u64);
            core::ptr::copy_nonoverlapping(bytes[done..].as_ptr(), target, chunk);
        }
        done += chunk;
    }
    Ok(())
}

/// The zeroed fill of one whole page: demand-zero is exactly that.
fn zero_page(space: &mut Space, page: u64) -> Result<(), LoadError> {
    resident_frame(space, page).map(|_| ())
}

/// Validate that `[address, address + length)` lies in one VAD with the asked protection.
///
/// This is the single validation point for user pointers: a handler that needs a buffer
/// calls it once and then copies through the direct map, so a bad pointer is refused
/// before any byte moves.
pub fn validate_user_range(
    space: &Space,
    address: u64,
    length: u64,
    write: bool,
) -> Result<(), LoadError> {
    if length == 0 {
        return Ok(());
    }
    let end = address.checked_add(length).ok_or(LoadError::UserPointer)?;
    let mut cursor = address & !(PAGE_SIZE as u64 - 1);
    while cursor < end {
        let Some(vad) = space.vads().find(cursor) else {
            return Err(LoadError::UserPointer);
        };
        if vad.kind == VadKind::Free || vad.kind == VadKind::Guard {
            return Err(LoadError::UserPointer);
        }
        let protection = vad.protection;
        if write && !protection.can_write() {
            return Err(LoadError::UserPointer);
        }
        if !write && !protection.can_read() {
            return Err(LoadError::UserPointer);
        }
        cursor = vad.end.max(cursor + PAGE_SIZE as u64);
    }
    Ok(())
}

/// Read `length` user bytes at `address` into `out`, validated then copied.
///
/// An untouched committed page is demand-zeroed first, so a read of never-written user
/// memory observes the zeros POSIX promises rather than failing.
pub fn read_user(space: &mut Space, address: u64, out: &mut [u8]) -> Result<(), LoadError> {
    validate_user_range(space, address, out.len() as u64, false)?;
    let mut done = 0usize;
    while done < out.len() {
        let current = address + done as u64;
        let frame = resident_frame(space, current)?;
        let offset = (current & (PAGE_SIZE as u64 - 1)) as usize;
        let chunk = (PAGE_SIZE - offset).min(out.len() - done);
        // SAFETY: the frame was just resolved in `space`; the direct map reads it as the
        // kernel's view of user memory.
        unsafe {
            let source = phys::direct_ptr::<u8>(frame + offset as u64);
            core::ptr::copy_nonoverlapping(source, out[done..].as_mut_ptr(), chunk);
        }
        done += chunk;
    }
    Ok(())
}

/// Read a NUL-terminated string from user space, bounded by [`SPAWN_MAX_STRING`].
pub fn read_user_string(space: &mut Space, address: u64) -> Result<UserString, LoadError> {
    if address == 0 {
        return Err(LoadError::UserPointer);
    }
    let mut bytes = [0u8; SPAWN_MAX_STRING];
    let mut len = 0usize;
    let mut cursor = address;
    loop {
        let mut byte = [0u8; 1];
        read_user(space, cursor, &mut byte)?;
        if byte[0] == 0 {
            return Ok(UserString { bytes, len });
        }
        if len == SPAWN_MAX_STRING {
            return Err(LoadError::Arguments);
        }
        bytes[len] = byte[0];
        len += 1;
        cursor = cursor.checked_add(1).ok_or(LoadError::UserPointer)?;
    }
}

/// Read the user-space NULL-terminated `argv` pointer table, bounded by [`SPAWN_MAX_ARGS`].
pub fn read_user_argv(space: &mut Space, address: u64) -> Result<UserArgv, LoadError> {
    let mut strings = [UserString {
        bytes: [0; SPAWN_MAX_STRING],
        len: 0,
    }; SPAWN_MAX_ARGS];
    let mut len = 0usize;
    if address != 0 {
        let mut index = 0u64;
        loop {
            let mut pointer = [0u8; 8];
            read_user(space, address + index * 8, &mut pointer)?;
            let value = u64::from_le_bytes(pointer);
            if value == 0 {
                break;
            }
            if len == SPAWN_MAX_ARGS {
                return Err(LoadError::Arguments);
            }
            strings[len] = read_user_string(space, value)?;
            len += 1;
            index += 1;
        }
    }
    Ok(UserArgv { strings, len })
}

/// Reserve the load segments and fill them, build the initial stack, and return the entry.
pub fn load(
    space: &mut Space,
    image_bytes: &[u8],
    path: &str,
    argv: &[&str],
) -> Result<UserEntry, LoadError> {
    if argv.len() > MAX_ARG_COUNT {
        return Err(LoadError::Arguments);
    }
    let plan = Elf64LoadPlan::parse(image_bytes)?;
    let bias = image::dyn_bias(&plan, entropy())?;

    for segment in plan.segments() {
        let start = segment.virtual_address() + bias;
        let end = segment.end_address() + bias;
        let flags = segment.flags();
        let mut bits = Protection::READ;
        if flags.writable() {
            bits |= Protection::WRITE;
        }
        if flags.executable() {
            bits |= Protection::EXECUTE;
        }
        let protection = Protection::from_bits(bits);
        space
            .reserve(VadRequest {
                start: start & !(PAGE_SIZE as u64 - 1),
                end: (end + PAGE_SIZE as u64 - 1) & !(PAGE_SIZE as u64 - 1),
                kind: VadKind::Private,
                protection,
                maximum: protection,
                flags: VadFlags::empty(),
                backing: None,
            })
            .map_err(|_| LoadError::Reserve)?;
    }

    for op in image::page_ops(&plan, bias) {
        match op {
            PageOp::Zero { page } => zero_page(space, page)?,
            PageOp::Copy {
                page,
                offset,
                file_offset,
                length,
            } => {
                let bytes = image_bytes
                    .get(file_offset as usize..file_offset as usize + length as usize)
                    .ok_or(LoadError::Page)?;
                write_user(space, page + offset as u64, bytes)?;
            }
        }
    }

    for relocation in image::relocations(image_bytes, &plan, bias)?.iter() {
        write_user(space, relocation.address, &relocation.value.to_le_bytes())?;
    }

    // The stack: a grow-down VAD with the initial image pinned at the top.
    space.set_stack_floor(STACK_FLOOR);
    space
        .reserve(VadRequest {
            start: STACK_TOP - USER_STACK_SIZE,
            end: STACK_TOP,
            kind: VadKind::Stack,
            protection: Protection::read_write(),
            maximum: Protection::read_write(),
            flags: VadFlags::from_bits(VadFlags::GROWSDOWN),
            backing: None,
        })
        .map_err(|_| LoadError::Reserve)?;

    // SAFETY: loading runs with interrupts off on the boot processor, one process at a
    // time under the process table lock, so the scratch buffer has one user.
    let scratch = unsafe {
        let buffer = &mut *STACK_IMAGE.get();
        buffer.assume_init_mut()
    };
    let mut auxv_pairs = [(0u64, 0u64); 16];
    let mut auxv_len = 0usize;
    let push = |auxv: &mut [(u64, u64); 16], len: &mut usize, tag: u64, value: u64| {
        auxv[*len] = (tag, value);
        *len += 1;
    };
    let phdr = plan.program_headers_address().unwrap_or(0) + bias;
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::PHDR,
        phdr,
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::PHENT,
        u64::from(plan.program_header_entry_size()),
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::PHNUM,
        u64::from(plan.program_header_count()),
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::PAGESZ,
        PAGE_SIZE as u64,
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::BASE,
        bias,
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::FLAGS,
        0,
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::ENTRY,
        image::loaded_entry(&plan, bias),
    );
    push(&mut auxv_pairs, &mut auxv_len, nolx_types::abi::at::UID, 0);
    push(&mut auxv_pairs, &mut auxv_len, nolx_types::abi::at::EUID, 0);
    push(&mut auxv_pairs, &mut auxv_len, nolx_types::abi::at::GID, 0);
    push(&mut auxv_pairs, &mut auxv_len, nolx_types::abi::at::EGID, 0);
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::SECURE,
        0,
    );
    push(
        &mut auxv_pairs,
        &mut auxv_len,
        nolx_types::abi::at::CLKTCK,
        100,
    );

    if argv.len() > MAX_ARG_COUNT {
        return Err(LoadError::Arguments);
    }
    let mut argv_storage: [&[u8]; 16] = [&[]; 16];
    for (slot, argument) in argv_storage.iter_mut().zip(argv.iter()) {
        if argument.is_empty() || argument.len() > 4096 || argument.as_bytes().contains(&0) {
            return Err(LoadError::Arguments);
        }
        *slot = argument.as_bytes();
    }
    let mut random_array = [0u8; 16];
    random_array[..8].copy_from_slice(&entropy().to_le_bytes());
    random_array[8..].copy_from_slice(&entropy().to_le_bytes());
    let empty: [&[u8]; 0] = [];
    let request = StackRequest {
        argv: &argv_storage[..argv.len()],
        envp: &empty,
        auxv: &auxv_pairs[..auxv_len],
        random: random_array,
        execfn: path.as_bytes(),
    };
    let rsp = image::build_stack_image(scratch, &request, STACK_TOP)?;
    let size = (STACK_TOP - rsp) as usize;
    write_user(space, rsp, &scratch[..size])?;

    Ok(UserEntry {
        rip: image::loaded_entry(&plan, bias),
        rsp,
    })
}

/// Write the `SIGCHLD` result record `waitid` promises, into a (possibly inactive) space.
///
/// # Safety
///
/// `space` must be live and exclusively borrowed by the caller, and `info_user` must have
/// been validated in it by the `waitid` path.
pub unsafe fn fill_siginfo(
    space: &mut Space,
    info_user: u64,
    child_pid: u64,
    status: i32,
) -> Result<(), LoadError> {
    validate_user_range(space, info_user, Siginfo::SIZE as u64, true)?;
    let mut info = Siginfo::empty();
    info.signo = SIGCHLD;
    info.code = cld::EXITED;
    info.pid = child_pid as i32;
    info.status = status;
    // SAFETY: `info` is a plain local of the fixed ABI size; the bytes are read once and
    // copied out through the validated user pointer by `write_user`.
    let bytes = unsafe {
        core::slice::from_raw_parts(core::ptr::addr_of!(info).cast::<u8>(), Siginfo::SIZE)
    };
    write_user(space, info_user, bytes)
}
