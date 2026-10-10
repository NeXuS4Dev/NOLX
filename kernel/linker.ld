/*
 * NOLX kernel linker script.
 *
 * The kernel is linked at a fixed physical address for now (1 MiB, above the BIOS data
 * area and the real-mode structures). M3 moves the kernel to a higher-half virtual
 * address and drops the identity mapping; when that happens this script gains a second
 * set of VMA/LMA pairs, not a rewrite.
 *
 * Section order matters for two reasons:
 *   - .multiboot must land within the first 8 KiB of the image, or the loader will not
 *     find the header;
 *   - _edata and _end bound the region the Multiboot a.out kludge asks the loader to
 *     zero, so they must be exactly right.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(__start32)

KERNEL_LOAD_ADDR = 0x00100000;
STACK_SIZE       = 0x10000;

SECTIONS
{
    . = KERNEL_LOAD_ADDR;

    /* The Multiboot header, first and page aligned. */
    .multiboot ALIGN(4K) : {
        KEEP(*(.multiboot))
    }

    .text ALIGN(4K) : {
        *(.text .text.*)
    }

    .rodata ALIGN(4K) : {
        *(.rodata .rodata.*)
        /* Rust emits vtables and type info here; keep them contiguous so a future
         * kernel module loader can find them in one range. */
        __eh_frame_start = .;
        *(.eh_frame)
        __eh_frame_end = .;
    }

    .data ALIGN(4K) : {
        *(.data .data.*)
    }

    /* Rust needs .got/.got.plt present even when empty, and they must be writable. */
    .got ALIGN(4K) : {
        *(.got)
        *(.got.plt)
    }

    . = ALIGN(4K);
    _edata = .;                 /* end of initialised data (Multiboot load_end_addr) */

    .bss ALIGN(4K) (NOLOAD) : {
        _sbss = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        _ebss = .;
    }

    . = ALIGN(4K);
    _end = .;                   /* loader zeroes _edata.._end (Multiboot bss_end_addr) */

    /* Per-CPU data and the kernel heap arena are carved out of the direct map at
     * runtime; these symbols exist so the memory manager can find its own boundaries
     * without parsing the ELF again. */
    .kernel_heap (NOLOAD) : {
        _kernel_heap_start = .;
        . += 16M;
        _kernel_heap_end = .;
    }

    /DISCARD/ : {
        *(.comment)
        *(.note.*)
        *(.rela.*)
    }
}

ASSERT(SIZEOF(.multiboot) >= 28, "the Multiboot header (with kludge fields) must be present")
ASSERT(_edata >= KERNEL_LOAD_ADDR, "_edata must lie above the load address")
ASSERT(_end >= _edata, "_end must lie at or above _edata")
