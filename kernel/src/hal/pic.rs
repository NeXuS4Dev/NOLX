//! Legacy dual-8259 PIC programming and end-of-interrupt handling.
//!
//! The PIC is used as the initial interrupt controller so the PIT can prove the new IDT
//! and dispatcher path before local-APIC support is introduced. All lines remain masked
//! until `unmask_irq` is explicitly called.

use nolx_hal_core::pic::{irq_for_vector, MASTER_VECTOR_OFFSET, SLAVE_VECTOR_OFFSET};

use super::{inb, outb};

const MASTER_COMMAND: u16 = 0x20;
const MASTER_DATA: u16 = 0x21;
const SLAVE_COMMAND: u16 = 0xA0;
const SLAVE_DATA: u16 = 0xA1;
const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const ICW4_8086: u8 = 0x01;
const EOI: u8 = 0x20;

/// Remap both PICs away from the CPU exception vectors and mask every IRQ line.
pub fn init() {
    // SAFETY: the PIC command/data ports are the standard 8259-compatible PC ports. The
    // kernel is still in early single-CPU boot with maskable interrupts disabled.
    unsafe {
        outb(MASTER_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();
        outb(SLAVE_COMMAND, ICW1_INIT | ICW1_ICW4);
        io_wait();
        outb(MASTER_DATA, MASTER_VECTOR_OFFSET);
        io_wait();
        outb(SLAVE_DATA, SLAVE_VECTOR_OFFSET);
        io_wait();
        outb(MASTER_DATA, 1 << 2); // slave is connected to master IRQ2
        io_wait();
        outb(SLAVE_DATA, 2); // slave's cascade identity is 2
        io_wait();
        outb(MASTER_DATA, ICW4_8086);
        io_wait();
        outb(SLAVE_DATA, ICW4_8086);
        io_wait();
        outb(MASTER_DATA, 0xFF);
        outb(SLAVE_DATA, 0xFF);
    }
}

/// Unmask one IRQ line. Unmasking a slave line also unmasks the master's cascade input.
pub fn unmask_irq(irq: u8) {
    if irq >= 16 {
        return;
    }

    // SAFETY: data-port reads return the current mask. The PIC was remapped by `init`, and
    // the caller only requests a line in the controller's documented 0..16 range.
    unsafe {
        if irq < 8 {
            let mask = inb(MASTER_DATA) & !(1 << irq);
            outb(MASTER_DATA, mask);
        } else {
            let slave_mask = inb(SLAVE_DATA) & !(1 << (irq - 8));
            outb(SLAVE_DATA, slave_mask);
            let master_mask = inb(MASTER_DATA) & !(1 << 2);
            outb(MASTER_DATA, master_mask);
        }
    }
}

/// Mask one IRQ line. Masking the cascade line also blocks every slave IRQ.
pub fn mask_irq(irq: u8) {
    if irq >= 16 {
        return;
    }

    // SAFETY: the PIC was remapped by `init`; the mask ports are readable and writable.
    unsafe {
        if irq < 8 {
            let mask = inb(MASTER_DATA) | (1 << irq);
            outb(MASTER_DATA, mask);
        } else {
            let mask = inb(SLAVE_DATA) | (1 << (irq - 8));
            outb(SLAVE_DATA, mask);
        }
    }
}

/// Send the appropriate EOI after servicing a PIC interrupt vector.
pub fn end_of_interrupt(vector: u8) {
    let Some(irq) = irq_for_vector(vector) else {
        return;
    };

    // SAFETY: this function is called only after dispatching a vector in the remapped PIC
    // ranges. A slave IRQ must be acknowledged at both controllers, slave first.
    unsafe {
        if irq >= 8 {
            outb(SLAVE_COMMAND, EOI);
        }
        outb(MASTER_COMMAND, EOI);
    }
}

/// The vector's IRQ number, if it belongs to either PIC.
#[must_use]
pub const fn irq_for(vector: u8) -> Option<u8> {
    irq_for_vector(vector)
}

fn io_wait() {
    // SAFETY: port 0x80 is the conventional PC I/O delay port. It is used only to separate
    // the slow PIC initialization writes, not to communicate with an active device.
    unsafe { outb(0x80, 0) }
}
