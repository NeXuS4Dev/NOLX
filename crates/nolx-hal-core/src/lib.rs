//! Hardware-independent x86-64 HAL primitives.
//!
//! The kernel-specific code that touches ports, control registers, and interrupt state is
//! kept in `kernel/src/hal`. This crate contains the interfaces, layouts, and calculations
//! that can be checked on an ordinary host: ACPI firmware-table decoding, the console
//! contract, GDT/IDT descriptors, PIC vector mapping, exception-frame facts, IPL values,
//! RTC decoding, and PIT clock conversion.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod acpi;
pub mod apic;
pub mod console;
pub mod gdt;
pub mod idt;
pub mod interrupt;
pub mod ipl;
pub mod irq_vectors;
pub mod pic;
pub mod rtc;
pub mod time;
pub mod vectors;
