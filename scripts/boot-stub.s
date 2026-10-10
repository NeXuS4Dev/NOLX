/*
 * TEST-ONLY stub for the boot layer link check.
 *
 * `make bootcheck` links boot/boot.S against kernel/linker.ld without a Rust toolchain, so
 * the one symbol the boot layer calls into has to come from somewhere. This is that
 * somewhere. It is NOT part of the kernel image; scripts/check-boot-layer.sh links it in
 * deliberately and nothing else may.
 *
 * The real symbol lives in kernel/src/main.rs.
 */

    .section .text
    .code64
    .globl  nolx_probe
    .type   nolx_probe, @function
nolx_probe:
    /* extern "C" fn nolx_probe() -- returns, unlike the real entry point. */
    ret
    .size   nolx_probe, . - nolx_probe

    .globl  nolx_kernel_main
    .type   nolx_kernel_main, @function
nolx_kernel_main:
    /* extern "C" fn nolx_kernel_main(mb_info: *const u8, magic: u32) -> ! */
    cli
1:  hlt
    jmp 1b
    .size   nolx_kernel_main, . - nolx_kernel_main

    .section .note.GNU-stack, "", @progbits
