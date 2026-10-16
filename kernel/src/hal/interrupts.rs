//! Common assembly-to-Rust interrupt boundary, resolvable page faults, and fatal exception
//! reporting.
//!
//! There are exactly two things an exception can do here. A page fault may be *resolved*: the
//! address space the processor is running in committed the page the access wanted, so the
//! memory manager maps it and the faulting instruction is retried -- the return from this
//! function is the iret back to it. Anything else is fatal, and reporting is all that is left.
//!
//! A page fault is resolvable only when a user address space is active: the kernel's own
//! address space has nothing to demand-page, so a fault in it is a kernel bug, and the report
//! says so.

use core::arch::asm;

use crate::hal::console::Console;
use crate::hal::{apic, cpu, pic, timer};
use nolx_hal_core::console::HalConsole;
use nolx_hal_core::ipl::Ipl;

pub use nolx_hal_core::interrupt::InterruptFrame;

/// Entry called by `interrupts.S`. It is not a Rust API; the assembly stubs are its only
/// caller and provide a pointer to a live [`InterruptFrame`].
#[no_mangle]
extern "C" fn nolx_interrupt_dispatch(frame_pointer: *mut InterruptFrame) -> *mut InterruptFrame {
    // SAFETY: each assembly stub pushes the register, vector, error-code, and processor frame
    // immediately before this call. The pointer remains live until this function returns.
    let frame = unsafe { &mut *frame_pointer };
    let vector = frame.vector as u8;

    if apic::is_spurious_vector(vector) {
        return frame_pointer;
    }

    if vector == super::ahci::COMPLETION_VECTOR {
        return device_completion(frame_pointer, super::ahci::on_completion_interrupt);
    }
    if vector == super::nvme::COMPLETION_VECTOR {
        return device_completion(frame_pointer, super::nvme::on_completion_interrupt);
    }
    if vector == super::virtio_blk::COMPLETION_VECTOR {
        return device_completion(frame_pointer, super::virtio_blk::on_completion_interrupt);
    }

    if apic::is_enabled() && apic::handles_vector(vector) {
        // Interrupt gates clear IF. APIC TPR and the per-CPU software IPL are both raised
        // until after the local APIC EOI, then restored in that order.
        let mut clock_tick = false;
        if let Some(previous_ipl) = cpu::raise_ipl(Ipl::DEVICE_MIN) {
            if vector == 32 || vector == nolx_hal_core::apic::LAPIC_TIMER_VECTOR {
                timer::on_interrupt(vector);
                clock_tick = true;
            }
            apic::end_of_interrupt();
            cpu::restore_ipl(previous_ipl);
        } else {
            apic::end_of_interrupt();
        }
        return if clock_tick {
            crate::exec::on_timer_interrupt(frame_pointer)
        } else {
            frame_pointer
        };
    }

    if pic::irq_for(vector).is_some() {
        // The legacy path has no hardware priority register. Keep software IPL recorded until
        // the 8259 has received EOI, then restore the previous level.
        let mut clock_tick = false;
        if let Some(previous_ipl) = cpu::raise_ipl(Ipl::DEVICE_MIN) {
            if vector == 32 {
                timer::on_interrupt(vector);
                clock_tick = true;
            }
            pic::end_of_interrupt(vector);
            cpu::restore_ipl(previous_ipl);
        } else {
            pic::end_of_interrupt(vector);
        }
        return if clock_tick {
            crate::exec::on_timer_interrupt(frame_pointer)
        } else {
            frame_pointer
        };
    }

    if vector == 14 && crate::mm::space::handle(read_cr2(), frame.error_code) {
        // The page is mapped and the instruction can be retried: the address space promised
        // this memory, and the fault is how a promise becomes a page.
        return frame_pointer;
    }

    let mut console = Console::new();
    report_exception(frame, &mut console);
    halt_after_exception();
}

fn report_exception(frame: &InterruptFrame, console: &mut dyn HalConsole) {
    let cr2 = if frame.vector == 14 { read_cr2() } else { 0 };
    let _ = writeln!(
        console,
        "NOLX: CPU exception vector={} error={:#018x} rip={:#018x} cs={:#06x} rflags={:#018x} rsp={:#018x} ss={:#06x} cr2={:#018x}",
        frame.vector,
        frame.error_code,
        frame.instruction_pointer,
        frame.code_segment,
        frame.flags,
        frame.stack_pointer,
        frame.stack_segment,
        cr2,
    );
    let _ = writeln!(
        console,
        "NOLX: registers rax={:#018x} rbx={:#018x} rcx={:#018x} rdx={:#018x}",
        frame.rax, frame.rbx, frame.rcx, frame.rdx,
    );
    let _ = writeln!(
        console,
        "NOLX: registers rsi={:#018x} rdi={:#018x} rbp={:#018x} r8={:#018x}",
        frame.rsi, frame.rdi, frame.rbp, frame.r8,
    );
    let _ = writeln!(
        console,
        "NOLX: registers r9={:#018x} r10={:#018x} r11={:#018x} r12={:#018x}",
        frame.r9, frame.r10, frame.r11, frame.r12,
    );
    let _ = writeln!(
        console,
        "NOLX: registers r13={:#018x} r14={:#018x} r15={:#018x}",
        frame.r13, frame.r14, frame.r15,
    );
    let _ = writeln!(console, "NOLX: fatal exception; processor halted");
    console.flush();
}

fn read_cr2() -> u64 {
    let address: u64;
    // SAFETY: CR2 is defined on every x86-64 processor and this path is entered only from
    // the page-fault vector, where it contains the faulting linear address.
    unsafe {
        asm!(
            "mov {}, cr2",
            out(reg) address,
            options(nomem, nostack, preserves_flags),
        );
    }
    address
}

fn halt_after_exception() -> ! {
    // SAFETY: the current interrupt gate has IF clear; CLI also prevents any later maskable
    // interrupt from returning to a faulting context. HLT is the fatal kernel stop state.
    unsafe { asm!("cli", options(nomem, nostack)) };
    loop {
        // SAFETY: the kernel has deliberately stopped after reporting an unrecoverable CPU
        // exception. There is no scheduler or recovery path at this milestone.
        unsafe { asm!("hlt", options(nomem, nostack)) };
    }
}

/// Trigger the deliberate out-of-map read used by the T2 page-fault test build.
#[cfg(feature = "hal-fault-test-pf")]
pub fn trigger_page_fault_test() -> ! {
    // The bootstrap page tables map the first 2 GiB; this canonical address is just beyond
    // that range and has no valid translation.
    // SAFETY: this function exists only in the dedicated T2 fault-test image. The resulting
    // #PF must be caught by the installed IDT and terminate in `report_exception`.
    let _ = unsafe { core::ptr::read_volatile(0x8000_0000usize as *const u8) };
    halt_after_exception()
}

/// Trigger a #GP with an invalid data-segment selector in the T2 test build.
#[cfg(feature = "hal-fault-test-gp")]
pub fn trigger_general_protection_test() -> ! {
    // Selector 0x40 is GDT entry 8, one past the seven the table holds. The descriptor fetch
    // lands at `GDTR.base + 0x40`, which is inside the kernel image window -- and that matters:
    // the processor may perform the descriptor read before it checks the GDT limit, so a
    // selector whose fetch address is *not* mapped (an LDT selector, say, with no LDT loaded,
    // which fetches at LDTR.base 0) faults on the read and reports #PF instead. The address
    // here is read successfully and the limit check is what fails, which is the #GP this
    // test exists to prove the handler reports.
    // SAFETY: this function exists only in the dedicated T2 fault-test image. The resulting
    // #GP must be caught by the installed IDT and terminate in `report_exception`.
    unsafe {
        asm!(
            "mov ax, 0x40",
            "mov ds, ax",
            out("ax") _,
            options(nostack),
        );
    }
    halt_after_exception()
}

/// Take a device's completion on its own MSI-X vector: count it with `handler`, then send the local
/// APIC its EOI. The software IPL is raised for the duration, as for the other device vectors.
fn device_completion(frame_pointer: *mut InterruptFrame, handler: fn()) -> *mut InterruptFrame {
    let previous = cpu::raise_ipl(Ipl::DEVICE_MIN);
    handler();
    apic::end_of_interrupt();
    if let Some(previous_ipl) = previous {
        cpu::restore_ipl(previous_ipl);
    }
    frame_pointer
}
