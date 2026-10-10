//! Long-mode GDT/TSS and 256-entry IDT installation.
//!
//! The boot trampoline loads a minimal GDT and a small fault-only IDT before entering Rust.
//! This module installs the TSS descriptor into the existing GDT, loads the task register,
//! then replaces the early IDT with a complete table of Rust-dispatched stubs.

use core::arch::asm;
use core::mem::size_of;
use core::ptr;

use nolx_hal_core::gdt::tss_descriptor;
use nolx_hal_core::idt::{IdtGate, IDT_ENTRIES};
use nolx_hal_core::vectors::exception_has_error_code;

use crate::sync::SyncCell;

// Keep the Rust exception-frame model in lockstep with the GAS stub list below.
const _: () = {
    assert!(exception_has_error_code(8));
    assert!(exception_has_error_code(10));
    assert!(exception_has_error_code(11));
    assert!(exception_has_error_code(12));
    assert!(exception_has_error_code(13));
    assert!(exception_has_error_code(14));
    assert!(exception_has_error_code(17));
    assert!(exception_has_error_code(21));
    assert!(exception_has_error_code(29));
    assert!(exception_has_error_code(30));
    assert!(!exception_has_error_code(9));
    assert!(!exception_has_error_code(15));
};

/// Kernel code selector, defined by `boot/boot.S`'s GDT.
const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// TSS selector, occupying GDT entries 5 and 6.
const TSS_SELECTOR: u16 = 0x28;
/// Stack size assigned to each fatal/critical exception IST.
const IST_STACK_SIZE: usize = 16 * 1024;

/// Long-mode TSS layout, as required by the processor.
#[repr(C, packed)]
struct TaskStateSegment {
    reserved0: u32,
    rsp0: u64,
    rsp1: u64,
    rsp2: u64,
    reserved1: u64,
    ist1: u64,
    ist2: u64,
    ist3: u64,
    ist4: u64,
    ist5: u64,
    ist6: u64,
    ist7: u64,
    reserved2: u64,
    reserved3: u16,
    iomap_base: u16,
}

impl TaskStateSegment {
    const fn empty() -> Self {
        Self {
            reserved0: 0,
            rsp0: 0,
            rsp1: 0,
            rsp2: 0,
            reserved1: 0,
            ist1: 0,
            ist2: 0,
            ist3: 0,
            ist4: 0,
            ist5: 0,
            ist6: 0,
            ist7: 0,
            reserved2: 0,
            reserved3: 0,
            iomap_base: size_of::<Self>() as u16,
        }
    }
}

const _: () = assert!(size_of::<TaskStateSegment>() == 104);

/// An aligned TSS wrapper. The processor writes its busy bit into the GDT, not this body.
#[repr(C, align(16))]
struct AlignedTss(TaskStateSegment);

/// A writable IST stack with a 16-byte-aligned top.
#[repr(C, align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

impl IstStack {
    const fn empty() -> Self {
        Self([0; IST_STACK_SIZE])
    }
}

/// The live IDT, aligned for descriptor-table loads.
#[repr(C, align(16))]
struct AlignedIdt([IdtGate; IDT_ENTRIES]);

/// The 10-byte `LGDT`/`LIDT` descriptor used in 64-bit mode.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

const _: () = assert!(size_of::<DescriptorTablePointer>() == 10);

static TSS: SyncCell<AlignedTss> = SyncCell::new(AlignedTss(TaskStateSegment::empty()));
static IDT: SyncCell<AlignedIdt> = SyncCell::new(AlignedIdt([IdtGate::MISSING; IDT_ENTRIES]));
static IST_NMI: SyncCell<IstStack> = SyncCell::new(IstStack::empty());
static IST_DOUBLE_FAULT: SyncCell<IstStack> = SyncCell::new(IstStack::empty());
static IST_MACHINE_CHECK: SyncCell<IstStack> = SyncCell::new(IstStack::empty());

extern "C" {
    /// Start of the boot GDT in `boot/boot.S`.
    #[link_name = "gdt64"]
    static mut BOOT_GDT: u64;
    /// Top of the boot stack, used as the initial ring-0 stack in the TSS.
    #[link_name = "boot_stack_top"]
    static BOOT_STACK_TOP: u8;
    /// 256 assembly entry points, in vector order.
    #[link_name = "nolx_isr_stub_table"]
    static ISR_STUB_TABLE: [usize; IDT_ENTRIES];
}

/// Install the TSS and the complete IDT.
///
/// Must run once on the boot processor before external interrupts are enabled. The boot
/// trampoline entered Rust with interrupts disabled and no other processor running.
pub fn init() {
    init_tss();
    init_idt();
}

fn init_tss() {
    // SAFETY: boot-time, single-CPU initialization; the TSS is only written here until
    // interrupt delivery and later CPU bring-up are implemented.
    let tss = unsafe { ptr::addr_of_mut!((*TSS.get()).0) };

    // SAFETY: these raw, unaligned writes follow the packed x86 TSS offsets. The TSS
    // remains exclusively owned by this initializer until `ltr` publishes it to the CPU.
    unsafe {
        ptr::addr_of_mut!((*tss).rsp0).write_unaligned(ptr::addr_of!(BOOT_STACK_TOP) as u64);
        ptr::addr_of_mut!((*tss).ist1).write_unaligned(stack_top(&IST_NMI));
        ptr::addr_of_mut!((*tss).ist2).write_unaligned(stack_top(&IST_DOUBLE_FAULT));
        ptr::addr_of_mut!((*tss).ist3).write_unaligned(stack_top(&IST_MACHINE_CHECK));
        ptr::addr_of_mut!((*tss).iomap_base).write_unaligned(size_of::<TaskStateSegment>() as u16);
    }

    let [low, high] = tss_descriptor(tss as u64, (size_of::<TaskStateSegment>() - 1) as u32);

    // SAFETY: the boot GDT reserves slots 5 and 6 for the 16-byte TSS descriptor. The
    // identity mapping is writable during M1/M2, and interrupts are still disabled.
    unsafe {
        let gdt = ptr::addr_of_mut!(BOOT_GDT);
        ptr::write_volatile(gdt.add(5), low);
        ptr::write_volatile(gdt.add(6), high);
    }

    // SAFETY: `TSS_SELECTOR` refers to the descriptor just installed in the boot GDT.
    // `ltr` reads the descriptor and records its selector in the task register.
    unsafe {
        asm!("ltr ax", in("ax") TSS_SELECTOR, options(nostack, preserves_flags));
    }
}

fn stack_top(stack: &SyncCell<IstStack>) -> u64 {
    // SAFETY: each IST region is exclusively assigned to a CPU stack. The pointer is used
    // only to compute its one-past-end stack top; Rust never creates a slice over it.
    unsafe { stack.get().cast::<u8>().add(size_of::<IstStack>()) as u64 }
}

fn init_idt() {
    // SAFETY: the IDT is written once before LIDT while interrupts remain disabled.
    let idt = unsafe { ptr::addr_of_mut!((*IDT.get()).0).cast::<IdtGate>() };

    // The assembly table is a read-only array of exactly 256 addresses, in vector order,
    // linked into this kernel image alongside its stubs. Taking its address does not read
    // from the external static; each indexed read below is explicitly unsafe.
    let stubs = ptr::addr_of!(ISR_STUB_TABLE).cast::<usize>();
    for vector in 0..IDT_ENTRIES {
        // SAFETY: `vector` is bounded by `IDT_ENTRIES`, the exact length of the assembly
        // table, and the entry is initialized before the IDT is loaded.
        let handler = unsafe { stubs.add(vector).read() } as u64;
        let ist = match vector {
            2 => 1,  // NMI: dedicated IST1
            8 => 2,  // double fault: dedicated IST2
            18 => 3, // machine check: dedicated IST3
            _ => 0,
        };
        let gate = IdtGate::interrupt(handler, KERNEL_CODE_SELECTOR, ist, 0);

        // SAFETY: `vector` is a valid index into the 256-entry aligned IDT.
        unsafe { idt.add(vector).write(gate) };
    }

    let descriptor = DescriptorTablePointer {
        limit: (size_of::<AlignedIdt>() - 1) as u16,
        base: idt as u64,
    };

    // SAFETY: descriptor points to the fully populated IDT and remains live for the
    // lifetime of the kernel. LIDT reads the packed 10-byte pseudo-descriptor.
    unsafe {
        asm!(
            "lidt [{}]",
            in(reg) &descriptor,
            options(nostack, preserves_flags),
        );
    }
}
