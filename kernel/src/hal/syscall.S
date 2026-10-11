/*
 * The syscall/sysret fast path entry.
 *
 * SYSCALL arrives with %rsp still on the user stack, %rcx = the user return address,
 * %r11 = the user rflags, and %rax = the syscall number. SFMASK has already cleared IF,
 * TF, and DF, so nothing can interrupt the window where GS is still the user base.
 *
 * The stub builds the same InterruptFrame an interrupt stub does -- the shared Rust
 * dispatcher and the shared iretq restore path both see one frame shape -- and returns
 * either through sysretq (same user thread) or through nolx_frame_restore (a switched
 * frame: the syscall blocked, exited, or preempted to another thread).
 */

    .section .text
    .code64

    .extern nolx_syscall_dispatch
    .extern nolx_frame_restore

    /* Mirrors of kernel/src/hal/cpu.rs and kernel/src/hal/syscall.rs constants. */
    .set    GS_KERNEL_STACK_TOP, 16
    .set    GS_USER_RSP_SCRATCH, 24
    .set    USER_DATA_RPL3, 0x1b
    .set    USER_CODE_RPL3, 0x23
    .set    SYSCALL_FRAME_VECTOR, 0x80

    .globl nolx_syscall_entry
    .type nolx_syscall_entry, @function
nolx_syscall_entry:
    swapgs                              /* GS = kernel per-CPU block */
    movq    %rsp, %gs:GS_USER_RSP_SCRATCH
    movq    %gs:GS_KERNEL_STACK_TOP, %rsp

    /*
     * Synthesise the processor frame (ss, rsp, rflags, rip, cs) plus the vector and
     * normalised error code, then the general registers in the InterruptFrame order.
     * The %rcx and %r11 slots hold the user values SYSCALL clobbered: the ABI says they
     * are destroyed, and sysret overwrites them from the frame's rip and rflags fields.
     */
    pushq   $USER_DATA_RPL3
    pushq   %gs:GS_USER_RSP_SCRATCH
    pushq   %r11                        /* user rflags */
    pushq   $USER_CODE_RPL3             /* frame.cs: the hardware's push order, ss..rip */
    pushq   %rcx                        /* user rip */
    pushq   $0                          /* error code */
    pushq   $SYSCALL_FRAME_VECTOR
    pushq   %rax                        /* syscall number in, result out */
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

    movq    %rsp, %rdi                  /* the frame handed to Rust */
    movq    %rsp, %r12                  /* ... remembered for the fast-path test */
    andq    $-16, %rsp
    call    nolx_syscall_dispatch

    cmpq    %r12, %rax                  /* resumed the same frame: take sysretq */
    jne     nolx_frame_restore

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
    popq    %rax                        /* the syscall result */
    addq    $16, %rsp                   /* vector + error code */
    movq    0(%rsp), %rcx               /* rip -> rcx, where sysretq wants it */
    movq    16(%rsp), %r11              /* rflags -> r11 */
    movq    24(%rsp), %rsp              /* back to the user stack */
    swapgs                              /* GS = user */
    sysretq

    .section .note.GNU-stack, "", @progbits
