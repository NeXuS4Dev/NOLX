/*
 * 256 long-mode interrupt stubs. They normalise exception stack frames, preserve every
 * integer register, and enter the safe Rust dispatcher with an InterruptFrame pointer.
 * The boot assembly owns an earlier 64-entry fault-only IDT until the Rust HAL installs
 * this table.
 */

    .section .text
    .code64

    .extern nolx_interrupt_dispatch

    .pushsection .rodata
    .p2align 3
    .globl nolx_isr_stub_table
    .type nolx_isr_stub_table, @object
nolx_isr_stub_table:
    .popsection

    .macro ISR_NOERR vector
    .globl nolx_isr_\vector
    .type  nolx_isr_\vector, @function
nolx_isr_\vector:
    pushq   $0                      /* synthetic error code */
    pushq   $\vector
    jmp     nolx_interrupt_common
    .pushsection .rodata
    .quad nolx_isr_\vector
    .popsection
    .endm

    .macro ISR_ERROR vector
    .globl nolx_isr_\vector
    .type  nolx_isr_\vector, @function
nolx_isr_\vector:
    pushq   $\vector               /* CPU already pushed the error code */
    jmp     nolx_interrupt_common
    .pushsection .rodata
    .quad nolx_isr_\vector
    .popsection
    .endm

    .irp vector, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63, 64, 65, 66, 67, 68, 69, 70, 71, 72, 73, 74, 75, 76, 77, 78, 79, 80, 81, 82, 83, 84, 85, 86, 87, 88, 89, 90, 91, 92, 93, 94, 95, 96, 97, 98, 99, 100, 101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112, 113, 114, 115, 116, 117, 118, 119, 120, 121, 122, 123, 124, 125, 126, 127, 128, 129, 130, 131, 132, 133, 134, 135, 136, 137, 138, 139, 140, 141, 142, 143, 144, 145, 146, 147, 148, 149, 150, 151, 152, 153, 154, 155, 156, 157, 158, 159, 160, 161, 162, 163, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175, 176, 177, 178, 179, 180, 181, 182, 183, 184, 185, 186, 187, 188, 189, 190, 191, 192, 193, 194, 195, 196, 197, 198, 199, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221, 222, 223, 224, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234, 235, 236, 237, 238, 239, 240, 241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 251, 252, 253, 254, 255
        .if (\vector == 8) || (\vector == 10) || (\vector == 11) || (\vector == 12) || (\vector == 13) || (\vector == 14) || (\vector == 17) || (\vector == 21) || (\vector == 29) || (\vector == 30)
            ISR_ERROR \vector
        .else
            ISR_NOERR \vector
        .endif
    .endr

    .pushsection .rodata
    .size nolx_isr_stub_table, . - nolx_isr_stub_table
    .popsection

    .type nolx_interrupt_common, @function
nolx_interrupt_common:
    /* The interrupted code may have set DF. The C ABI requires it clear in Rust. */
    cld

    /*
     * Ring-3 entries must switch %gs to the kernel's per-CPU block first: between the
     * interrupt and this point the processor still holds the user GS base. The interrupted
     * %rax is pushed first so it can be used as scratch; the saved copy is untouched.
     */
    pushq   %rax
    movq    0x20(%rsp), %rax            /* saved CS, at vector+error+rip */
    andq    $3, %rax
    cmpq    $3, %rax
    jne     1f
    swapgs
1:

    /* Push order is the reverse of InterruptFrame's field order. */
    pushq   %rbx
    pushq   %rcx
    pushq   %rdx
    pushq   %rsi
    pushq   %rdi
    pushq   %rbp
    pushq   %r8
    pushq   %r9
    pushq   %r10
    pushq   %r11
    pushq   %r12
    pushq   %r13
    pushq   %r14
    pushq   %r15

    /*
     * The saved registers remain in place while we align the call stack. The Rust dispatcher
     * returns the frame to resume in RAX: normally the frame passed here, or another thread's
     * saved frame after a timer preemption. Before CALL, RSP is 16-byte aligned; the callee
     * therefore sees the required 8-byte return-address skew.
     */
    movq    %rsp, %rdi
    andq    $-16, %rsp
    call    nolx_interrupt_dispatch
    jmp     nolx_frame_restore

    /*
     * Shared restore path: RAX names the InterruptFrame to resume. The restore target may
     * run at another privilege level than the one that was interrupted, so return-to-user
     * swaps %gs back exactly when the frame's saved CS has RPL 3 -- independent of whether
     * this entry swapped on the way in.
     */
    .globl nolx_frame_restore
    .type nolx_frame_restore, @function
nolx_frame_restore:
    movq    %rax, %rsp
    movq    0x90(%rsp), %rax            /* InterruptFrame.code_segment */
    andq    $3, %rax
    cmpq    $3, %rax
    jne     2f
    swapgs
2:
    popq    %r15
    popq    %r14
    popq    %r13
    popq    %r12
    popq    %r11
    popq    %r10
    popq    %r9
    popq    %r8
    popq    %rbp
    popq    %rdi
    popq    %rsi
    popq    %rdx
    popq    %rcx
    popq    %rbx
    popq    %rax
    addq    $16, %rsp              /* vector + error code */
    iretq

    .section .note.GNU-stack, "", @progbits
