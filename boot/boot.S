/*
 * NOLX boot layer -- x86-64.
 *
 * This is the first code the machine runs. It is deliberately small, dependency free, and
 * written in GNU as syntax so it can be assembled and linked without a Rust toolchain
 * (`make bootcheck`).
 *
 * What it does, in order:
 *   1. Present a Multiboot 1 header so GRUB 2 (via an ISO) or QEMU (`-kernel`, flat image)
 *      can load us.
 *   2. Zero BSS, stash the Multiboot info pointer, bring up COM1 so there is a log from
 *      the very first instruction that can produce one.
 *   3. Verify CPUID and long mode support.
 *   4. Identity map the first 1 GiB with 2 MiB pages.
 *   5. Enable PAE, set EFER.LME, enable paging, and far-jump into 64-bit mode.
 *   6. Load the 64-bit GDT and a real stack, then call the Rust entry point.
 *
 * The Rust entry point never returns; if it does, we halt with a message rather than
 * falling off the end of the code.
 *
 * Higher-half kernel mapping is deliberately NOT done here. M1 runs identity mapped at
 * 1 MiB; M3 introduces the higher-half mapping in the memory manager, where the page
 * tables are real objects rather than assembly constants. See
 * docs/architecture/03-memory-manager.md.
 */

    .set MULTIBOOT_MAGIC,       0x1BADB002
    .set MULTIBOOT_BOOT_MAGIC,  0x2BADB002
    .set MULTIBOOT_ALIGN,       1 << 0      /* align modules to page boundaries */
    .set MULTIBOOT_MEMINFO,     1 << 1      /* fill in mem_* and the mmap list  */
    .set MULTIBOOT_VIDEO,       1 << 2      /* ask the loader for mode info     */
    .set MULTIBOOT_AOUT_KLUDGE, 1 << 16     /* allow loading a flat binary      */
    .set MULTIBOOT_FLAGS,       MULTIBOOT_ALIGN | MULTIBOOT_MEMINFO | MULTIBOOT_AOUT_KLUDGE
    .set MULTIBOOT_CHECKSUM,    -(MULTIBOOT_MAGIC + MULTIBOOT_FLAGS)

    .set KERNEL_LOAD_ADDR,      0x00100000  /* 1 MiB: above the BIOS data area  */

    .set PAGE_PRESENT,          1 << 0
    .set PAGE_WRITABLE,         1 << 1
    .set PAGE_HUGE,             1 << 7
    .set PAGE_TABLE_FLAGS,      PAGE_PRESENT | PAGE_WRITABLE
    .set PAGE_HUGE_FLAGS,       PAGE_PRESENT | PAGE_WRITABLE | PAGE_HUGE

    .set MSR_EFER,              0xC0000080
    .set EFER_LME,              1 << 8

    .set CR0_PAGING,            1 << 31
    .set CR4_PAE,               1 << 5

    .set CPUID_EXT_MAX_FN,      0x80000000
    .set CPUID_EXT_FEATURES,    0x80000001
    .set CPUID_FEAT_LM,         1 << 29

    .set EFLAGS_ID,             1 << 21

    .set BOOT_STACK_SIZE,       65536

/* ================================================================== Multiboot header */

    .section .multiboot, "aw", @progbits
    .align 4
    .globl __multiboot_header
__multiboot_header:
    .long   MULTIBOOT_MAGIC
    .long   MULTIBOOT_FLAGS
    .long   MULTIBOOT_CHECKSUM
    /* a.out kludge. Ignored by loaders that understand the ELF program headers, used by
     * loaders (QEMU -kernel with a flat image) that do not. */
    .long   __multiboot_header      /* header_addr  */
    .long   KERNEL_LOAD_ADDR        /* load_addr    */
    .long   _edata                  /* load_end_addr: end of initialised data */
    .long   _end                    /* bss_end_addr:  loader must zero to here  */
    .long   __start32               /* entry_addr   */

/* ================================================================== messages */

    .section .rodata
msg_hello:
    .asciz  "NOLX: boot layer entered (32-bit protected mode)\r\n"
msg_long_mode:
    .asciz  "NOLX: long mode entered\r\n"
msg_bad_magic:
    .asciz  "NOLX: WARNING: multiboot magic mismatch\r\n"
msg_no_cpuid:
    .asciz  "NOLX: FATAL: CPUID is not supported\r\n"
msg_no_long_mode:
    .asciz  "NOLX: FATAL: long mode is not supported by this CPU\r\n"
msg_returned:
    .asciz  "NOLX: FATAL: kernel entry point returned\r\n"

/* ================================================================== 32-bit entry */

    .section .text
    .code32
    .globl __start32
    .type  __start32, @function
__start32:
    cli
    cld

    /* --- stash the boot information FIRST, before anything can clobber it.
     * %eax holds the loader magic and %ebx the physical address of the Multiboot
     * information structure. The BSS-zeroing loop below writes %eax, %ecx, and %edi, so
     * these two have to be saved before it runs. Both live in .data, which the loader has
     * already populated and which that loop does not touch. */
    movl    %ebx, mb_info_phys
    movl    %eax, mb_magic

    /* --- zero BSS: the page tables and the boot stack live there. */
    movl    $_sbss, %edi
    movl    $_ebss, %ecx
    subl    %edi, %ecx
    shrl    $2, %ecx
    xorl    %eax, %eax
    rep stosl

    call    serial_init
    movl    $msg_hello, %esi
    call    serial_puts

    cmpl    $MULTIBOOT_BOOT_MAGIC, mb_magic
    je      1f
    movl    $msg_bad_magic, %esi
    call    serial_puts
1:

    /* --- CPUID present? Toggle EFLAGS.ID and see if it sticks. */
    pushfl
    popl    %eax
    movl    %eax, %ecx
    xorl    $EFLAGS_ID, %eax
    pushl   %eax
    popfl
    pushfl
    popl    %eax
    pushl   %ecx
    popfl
    xorl    %ecx, %eax
    testl   $EFLAGS_ID, %eax
    jz      fatal_no_cpuid

    /* --- long mode present? */
    movl    $CPUID_EXT_MAX_FN, %eax
    cpuid
    cmpl    $CPUID_EXT_FEATURES, %eax
    jb      fatal_no_long_mode
    movl    $CPUID_EXT_FEATURES, %eax
    cpuid
    testl   $CPUID_FEAT_LM, %edx
    jz      fatal_no_long_mode

    /* --- identity map the first 1 GiB with 2 MiB pages.
     * pml4[0] -> pdpt, pdpt[0] -> pd0, pdpt[1] -> pd1, each pd holding 512 huge pages. */
    movl    $pml4, %edi
    movl    $pdpt, %eax
    orl     $PAGE_TABLE_FLAGS, %eax
    movl    %eax, (%edi)

    movl    $pdpt, %edi
    movl    $pd0, %eax
    orl     $PAGE_TABLE_FLAGS, %eax
    movl    %eax, (%edi)
    movl    $pd1, %eax
    orl     $PAGE_TABLE_FLAGS, %eax
    movl    %eax, 8(%edi)

    movl    $pd0, %edi
    xorl    %eax, %eax              /* physical address of the next 2 MiB page */
    movl    $1024, %ecx             /* 2 page directories x 512 entries        */
1:  movl    %eax, (%edi)
    orl     $PAGE_HUGE_FLAGS, (%edi)
    movl    $0, 4(%edi)
    addl    $0x200000, %eax
    addl    $8, %edi
    loop    1b

    /* --- PAE, then EFER.LME, then paging. Order matters. */
    movl    $pml4, %eax
    movl    %eax, %cr3

    movl    %cr4, %eax
    orl     $CR4_PAE, %eax
    movl    %eax, %cr4

    movl    $MSR_EFER, %ecx
    rdmsr
    orl     $EFER_LME, %eax
    wrmsr

    movl    %cr0, %eax
    orl     $CR0_PAGING, %eax
    movl    %eax, %cr0

    lgdt    gdt64_ptr
    ljmp    $0x08, $long_mode_entry

/* -------------------------------------------------------------- fatal paths (32-bit) */

fatal_no_cpuid:
    movl    $msg_no_cpuid, %esi
    jmp     fatal_halt
fatal_no_long_mode:
    movl    $msg_no_long_mode, %esi
fatal_halt:
    call    serial_puts
    cli
1:  hlt
    jmp     1b

/* ================================================================== 64-bit entry */

    .code64
    .globl long_mode_entry
    .type  long_mode_entry, @function
long_mode_entry:
    /* CS is already the 64-bit kernel code segment from the far jump. Reload the rest. */
    movw    $0x10, %ax
    movw    %ax, %ds
    movw    %ax, %es
    movw    %ax, %fs
    movw    %ax, %gs
    movw    %ax, %ss

    movabsq $boot_stack_top, %rsp
    andq    $-16, %rsp

    /* Zero the general purpose registers so nothing leaks into the first backtrace. */
    xorl    %eax, %eax
    xorl    %ebx, %ebx
    xorl    %ecx, %ecx
    xorl    %edx, %edx
    xorl    %esi, %esi
    xorl    %edi, %edi
    xorl    %ebp, %ebp
    xorl    %r8d, %r8d
    xorl    %r9d, %r9d
    xorl    %r10d, %r10d
    xorl    %r11d, %r11d
    xorl    %r12d, %r12d
    xorl    %r13d, %r13d
    xorl    %r14d, %r14d
    xorl    %r15d, %r15d

    movl    $msg_long_mode, %esi
    call    serial_puts64

    /* extern "C" fn nolx_kernel_main(mb_info: *const u8, magic: u32) -> ! */
    movl    mb_info_phys(%rip), %edi
    movl    mb_magic(%rip), %esi
    call    nolx_kernel_main

    leaq    msg_returned(%rip), %rsi
    call    serial_puts64
    cli
1:  hlt
    jmp     1b

/* ================================================================== serial console */

    .set COM1_DATA, 0x3F8
    .set COM1_LSR,  0x3FD

    .code32
    .type serial_init, @function
/* Bring up COM1 at 115200 8N1. Harmless if there is no serial device attached. */
serial_init:
    pushl   %eax
    pushl   %edx
    movw    $COM1_DATA + 1, %dx
    xorl    %eax, %eax
    outb    %al, %dx                /* disable interrupts          */
    movw    $COM1_DATA + 3, %dx
    movb    $0x80, %al
    outb    %al, %dx                /* DLAB on                     */
    movw    $COM1_DATA, %dx
    movb    $0x01, %al
    outb    %al, %dx                /* divisor low: 115200 baud    */
    movw    $COM1_DATA + 1, %dx
    xorl    %eax, %eax
    outb    %al, %dx                /* divisor high                */
    movw    $COM1_DATA + 3, %dx
    movb    $0x03, %al
    outb    %al, %dx                /* 8 bits, no parity, 1 stop   */
    movw    $COM1_DATA + 2, %dx
    movb    $0xC7, %al
    outb    %al, %dx                /* enable and clear the FIFOs  */
    movw    $COM1_DATA + 4, %dx
    movb    $0x0B, %al
    outb    %al, %dx                /* DTR, RTS, OUT2              */
    popl    %edx
    popl    %eax
    ret

    .type serial_putc, @function
/* serial_putc: %cl = byte. Clobbers %eax, %edx, %cl. */
serial_putc:
    pushl   %eax
    pushl   %edx
    movw    $COM1_LSR, %dx
1:  inb     %dx, %al
    testb   $0x20, %al
    jz      1b
    movw    $COM1_DATA, %dx
    movb    %cl, %al
    outb    %al, %dx
    popl    %edx
    popl    %eax
    ret

    .type serial_puts, @function
/* serial_puts: %esi = NUL-terminated string. Clobbers %eax, %ecx, %edx, %esi. */
serial_puts:
    pushl   %esi
1:  lodsb
    testb   %al, %al
    jz      2f
    movb    %al, %cl
    call    serial_putc
    jmp     1b
2:  popl    %esi
    ret

    .code64
    .type serial_putc64, @function
/* serial_putc64: %cl = byte. Clobbers %rax, %rdx, %cl. */
serial_putc64:
    pushq   %rax
    pushq   %rdx
    movw    $COM1_LSR, %dx
1:  inb     %dx, %al
    testb   $0x20, %al
    jz      1b
    movw    $COM1_DATA, %dx
    movb    %cl, %al
    outb    %al, %dx
    popq    %rdx
    popq    %rax
    ret

    .type serial_puts64, @function
/* serial_puts64: %rsi = NUL-terminated string. Clobbers %rax, %rcx, %rdx, %rsi. */
serial_puts64:
    pushq   %rsi
1:  lodsb
    testb   %al, %al
    jz      2f
    movb    %al, %cl
    call    serial_putc64
    jmp     1b
2:  popq    %rsi
    ret

/* ================================================================== GDT */

    .section .rodata
    .align 8
    .globl gdt64
gdt64:
    .quad 0x0000000000000000            /* 0x00 null               */
    .quad 0x00AF9A000000FFFF            /* 0x08 kernel code, 64-bit */
    .quad 0x00CF92000000FFFF            /* 0x10 kernel data         */
    .quad 0x00AFFA000000FFFF            /* 0x18 user code, 64-bit   */
    .quad 0x00CFF2000000FFFF            /* 0x20 user data           */
tss_desc_placeholder:
    .quad 0x0000000000000000            /* 0x28 TSS low: filled in by the HAL */
    .quad 0x0000000000000000            /* 0x30 TSS high                  */
gdt64_end:

    .align 8
    .globl gdt64_ptr
/*
 * A 10-byte GDT pseudo-descriptor (16-bit limit + 64-bit base).
 *
 * We load it with `lgdt` while still in 32-bit mode, where the instruction reads only the
 * first six bytes: the limit and the *low 32 bits* of the base. That is correct here
 * because the kernel is loaded below 4 GiB. Keeping the 64-bit form means the same
 * structure is what the HAL hands to `lgdt` once we are in long mode, so there is exactly
 * one GDT descriptor in the system instead of two that can drift apart.
 */
gdt64_ptr:
    .word gdt64_end - gdt64 - 1
    .quad gdt64

/* ================================================================== boot data */

    .section .data
    .align 4
    .globl mb_info_phys
mb_info_phys:
    .long 0
    .globl mb_magic
mb_magic:
    .long 0

/* ================================================================== boot BSS */

    .section .bss, "aw", @nobits
    .align 4096
    .globl pml4
pml4:
    .space 4096
    .globl pdpt
pdpt:
    .space 4096
    .globl pd0
pd0:
    .space 4096
    .globl pd1
pd1:
    .space 4096

    .align 16
    .globl boot_stack_bottom
boot_stack_bottom:
    .space BOOT_STACK_SIZE
    .globl boot_stack_top
boot_stack_top:

    .section .note.GNU-stack, "", @progbits
