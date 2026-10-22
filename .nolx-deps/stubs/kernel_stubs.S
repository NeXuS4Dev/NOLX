/*
 * Runtime symbols the kernel image needs when it is linked for the host target.
 *
 * The kernel is normally built for `x86_64-unknown-none`, where `compiler_builtins` supplies
 * `memcpy`, `memset`, `memmove`, `memcmp` and `bcmp`. The restricted-network bootstrap in
 * `.nolx-deps/README.md` builds the same sources for `x86_64-unknown-linux-gnu` instead,
 * because that is the only target the sandboxed toolchain ships a standard library for. On
 * that target those symbols come from the C library, which a freestanding kernel cannot call,
 * so they are provided here.
 *
 * THEY MUST REALLY COPY. An earlier version of this file defined them as bare `ret`
 * instructions, which linked cleanly and then silently dropped every large `memset`: the
 * frame-map fill (16 MiB) is turned into a `memset` call by LLVM, so the machine reported
 * zero usable frames and failed in `mmio::init`. A stub that does nothing is worse than a
 * missing symbol -- the link succeeds and the failure surfaces as a boot-time corruption.
 *
 * `memcpy` and `memset` may not call themselves; `rep movsb`/`rep stosb` are used instead, so
 * these routines do not depend on the compiler's own intrinsics. ERMS (`rep movsb`) is
 * available on every x86-64 processor that can run this kernel.
 */

    .section .text
    .code64

/* void *memcpy(void *dest, const void *src, size_t count) */
    .globl  memcpy
    .type   memcpy, @function
memcpy:
    movq    %rdi, %rax
    movq    %rdx, %rcx
    rep movsb
    ret
    .size   memcpy, . - memcpy

/* void *memmove(void *dest, const void *src, size_t count) */
    .globl  memmove
    .type   memmove, @function
memmove:
    movq    %rdi, %rax
    movq    %rdx, %rcx
    cmpq    %rsi, %rdi
    jbe     1f                      /* dest <= src: forward copy is safe */
    leaq    -1(%rsi, %rcx), %rsi    /* overlapping and dest > src: copy backwards */
    leaq    -1(%rdi, %rcx), %rdi
    std
    rep movsb
    cld
    ret
1:  rep movsb
    ret
    .size   memmove, . - memmove

/* void *memset(void *dest, int value, size_t count) */
    .globl  memset
    .type   memset, @function
memset:
    movq    %rdi, %r8               /* dest is the return value */
    movl    %esi, %eax              /* `stosb` stores al */
    movq    %rdx, %rcx
    rep stosb
    movq    %r8, %rax
    ret
    .size   memset, . - memset

/* int memcmp(const void *lhs, const void *rhs, size_t count) */
    .globl  memcmp
    .type   memcmp, @function
memcmp:
    xorl    %eax, %eax
    testq   %rdx, %rdx
    jz      3f
1:  movzbl  (%rdi), %eax
    movzbl  (%rsi), %ecx
    subl    %ecx, %eax
    jne     2f
    incq    %rdi
    incq    %rsi
    decq    %rdx
    jne     1b
    xorl    %eax, %eax
2:  ret
3:  ret
    .size   memcmp, . - memcmp

/* int bcmp(const void *lhs, const void *rhs, size_t count) -- BSD spelling of memcmp */
    .globl  bcmp
    .type   bcmp, @function
bcmp:
    jmp     memcmp
    .size   bcmp, . - bcmp

/* Rust's unwinding personality. The kernel is built with panic=abort and cannot unwind; if
 * this is ever called the machine is already in a state no unwind can repair. */
    .globl  rust_eh_personality
    .type   rust_eh_personality, @function
rust_eh_personality:
    ret
    .size   rust_eh_personality, . - rust_eh_personality

    .globl  _Unwind_Resume
    .type   _Unwind_Resume, @function
_Unwind_Resume:
    ud2
    .size   _Unwind_Resume, . - _Unwind_Resume

    .section .note.GNU-stack, "", @progbits
