/*
 * NOLX kernel linker script.
 *
 * The image has two address regions, because the code that installs the higher-half mapping
 * has to run at the physical address the loader put it at:
 *
 *   - the **boot layer** (`.multiboot`, `.boot`, `.boot.data`, `.boot.zero`) is linked where
 *     it loads, at KERNEL_LOAD_ADDR. 32-bit code reaches its own page tables, IDT, GDT, and
 *     stack with ordinary 32-bit addresses while paging is still off, and the ELF entry
 *     point stays a physical address a Multiboot loader can jump to;
 *   - everything else -- the Rust kernel -- is linked in the image window:
 *     VMA = LMA + KERNEL_IMAGE_BASE, in the top 2 GiB where the `kernel` code model can reach
 *     it with 32-bit sign-extended addressing (the same range `nolx_mm_core::paging` pins).
 *     The boot page tables install that window before entering long mode, and no kernel code
 *     ever addresses the image by its load address again.
 *
 * The load addresses stay contiguous from KERNEL_LOAD_ADDR, so the image is still one
 * contiguous physical range and the Multiboot a.out kludge fields -- which are *physical*
 * addresses -- stay meaningful. `_edata` and `_end` are therefore defined as load addresses
 * (they are what the loader reads to decide how much to copy and zero), while the Rust code
 * uses the `*_vma` symbols and converts them back with `mm::physical`.
 *
 * Section order matters for three reasons:
 *   - `.multiboot` must land within the first 8 KiB of the image, or the loader will not
 *     find the header;
 *   - `_edata` and `_end` bound the region the loader is asked to zero, so they must be
 *     exactly right;
 *   - loaders that use the kludge fields copy the image as *one flat block* -- the bytes at
 *     file offset `0x1000 + (p - KERNEL_LOAD_ADDR)` become the bytes at physical address
 *     `p` -- so every section that carries file content must sit at exactly that offset, and
 *     no content-free section may be listed before one that does. That is why the boot
 *     layer's zero-initialised state (`.boot.zero`: page tables, early IDT, boot stack) is
 *     stored in the file as zeros rather than as a NOBITS hole between stored sections, and
 *     why every section after `_edata` is such a hole: the loader zeroes `[_edata, _end)`.
 *     `scripts/verify-multiboot.py` checks the flat-copy property on the linked image.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(__start32)

KERNEL_LOAD_ADDR  = 0x00100000;
KERNEL_IMAGE_BASE = 0xffffffff80000000;   /* top 2 GiB: PML4 slot 511, see paging.rs */
KERNEL_STACK_SIZE = 0x20000;
HEAP_ARENA_SIZE   = 16M;                  /* the static kernel heap arena, MM-031 */

SECTIONS
{
    /* ---------------------------------------------------------- the boot layer, resident */
    . = KERNEL_LOAD_ADDR;

    /* The Multiboot header, first and page aligned. */
    .multiboot ALIGN(4K) : {
        KEEP(*(.multiboot))
    }

    /* The trampoline: 32-bit entry, long-mode entry, the serial console, the early traps.
     * Everything here is gone as soon as the higher-half switch completes. */
    .boot ALIGN(4K) : {
        *(.boot)
    }

    .boot.data ALIGN(16) : {
        *(.boot.data)
    }

    /* Zero-initialised boot state: the early IDT, the boot page tables, and the 32-bit
     * stack. Stored as zeros in the file, not as a NOBITS hole -- a hole here would push
     * every section after it off the flat-copy offset. */
    .boot.zero ALIGN(4K) : {
        _boot_zero_start = .;
        *(.boot.zero)
        . = ALIGN(16);
        _boot_zero_end = .;
    }

    . = ALIGN(4K);
    _boot_end = .;                      /* load address: end of the boot-resident part */

    /* --------------------------------------------------------------- the image window */
    . = KERNEL_IMAGE_BASE + _boot_end;

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_IMAGE_BASE) {
        *(.text .text.*)
    }

    .rodata ALIGN(4K) : AT(ADDR(.rodata) - KERNEL_IMAGE_BASE) {
        *(.rodata .rodata.*)
        /* Rust emits vtables and type info here; keep them contiguous so a future
         * kernel module loader can find them in one range. */
        __eh_frame_start = .;
        *(.eh_frame)
        __eh_frame_end = .;
    }

    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_IMAGE_BASE) {
        *(.data .data.*)
    }

    /* Rust needs .got/.got.plt present even when empty, and they must be writable. */
    .got ALIGN(4K) : AT(ADDR(.got) - KERNEL_IMAGE_BASE) {
        *(.got)
        *(.got.plt)
    }

    . = ALIGN(4K);
    _edata_vma = .;
    _edata = _edata_vma - KERNEL_IMAGE_BASE;    /* load address (Multiboot load_end_addr) */

    /* Everything from here on is zeroed by the loader, so the three regions below are
     * NOBITS and carry no file content. */
    .bss ALIGN(4K) (NOLOAD) : {
        _sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        _ebss = .;
    }

    /* The kernel's own stack. The trampoline switches to it just before it enters Rust, so
     * that nothing runs on the boot layer's stack after the identity map -- the only thing
     * that still names that stack -- is gone. */
    .kernel_stack ALIGN(4K) (NOLOAD) : {
        _kernel_stack_bottom = .;
        . += KERNEL_STACK_SIZE;
        _kernel_stack_top = .;
    }

    /* The static heap arena. Nothing reaches it until MM-031 registers a `GlobalAlloc`
     * over it; it exists here so that the kernel's own reserved range -- and therefore the
     * size of the image window the trampoline builds -- is a link-time constant. */
    .kernel_heap ALIGN(4K) (NOLOAD) : {
        _kernel_heap_start = .;
        . += HEAP_ARENA_SIZE;
        _kernel_heap_end = .;
    }

    . = ALIGN(4K);
    _image_end = .;                             /* virtual address of the last image byte */
    _end = _image_end - KERNEL_IMAGE_BASE;      /* load address (Multiboot bss_end_addr) */

    /* The image window's boundaries, as the memory manager needs them: the window maps the
     * kernel's whole reserved range -- image and arena -- at a fixed offset, and the frame
     * map reserves exactly the frames it covers. */
    _image_window_start = KERNEL_IMAGE_BASE + KERNEL_LOAD_ADDR;
    _image_window_end = _kernel_heap_end;
    _kernel_region_phys_end = _kernel_heap_end - KERNEL_IMAGE_BASE;
    /* Where the higher-half code begins: the boot layer below this is mapped by the window
     * but is never executed again after the switch. */
    _kernel_text_start = KERNEL_IMAGE_BASE + _boot_end;
    /* How many 2 MiB pages the image window needs: the boot trampoline fills that many
     * entries, and nothing else may disagree about the size of the window. */
    _image_window_pages = (_kernel_region_phys_end + 0x1FFFFF) >> 21;

    /DISCARD/ : {
        *(.comment)
        *(.note.*)
        *(.rela.*)
    }
}

ASSERT(SIZEOF(.multiboot) >= 28, "the Multiboot header (with kludge fields) must be present")
ASSERT(_boot_end >= KERNEL_LOAD_ADDR, "_boot_end must lie above the load address")
ASSERT(_boot_zero_end <= _boot_end, "the boot layer's zeroed region must end the boot layer")
ASSERT(_boot_end % 4K == 0, "the boot layer must end page aligned")
ASSERT(_kernel_text_start == KERNEL_IMAGE_BASE + _boot_end,
       "the image window must start where the boot layer ends")
ASSERT(_edata >= KERNEL_LOAD_ADDR, "_edata must lie above the load address")
ASSERT(_end >= _edata, "_end must lie at or above _edata")
ASSERT((_kernel_text_start & 0xFFF) == 0, "the higher-half code must start page aligned")
ASSERT(KERNEL_IMAGE_BASE % 0x200000 == 0, "the image window must be 2 MiB aligned for huge pages")
ASSERT(_kernel_stack_bottom >= _ebss, "the kernel stack must follow the kernel's BSS")
ASSERT(_kernel_heap_start >= _kernel_stack_top, "the heap arena must follow the kernel stack")
ASSERT(_image_end == _kernel_heap_end, "the image must end at the heap arena's end")
ASSERT(_end == _kernel_region_phys_end, "_end and the window's end must be the same address")
ASSERT(_kernel_region_phys_end < 0x80000000, "the kernel must load inside the first 2 GiB")
