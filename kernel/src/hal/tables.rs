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

use crate::mm::phys;
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

/// The kernel's own copy of the GDT, aligned for `LGDT`.
///
/// The trampoline's GDT lives in the boot layer, at a low address that stops being mapped
/// when the kernel drops the identity map; a descriptor table the processor reads on every
/// interrupt has to be somewhere the kernel owns. The descriptors themselves are copied from
/// the trampoline's table -- one definition of the code and data selectors, checked against
/// the linked image by `scripts/verify-multiboot.py` -- and the two TSS slots are filled here.
#[repr(C, align(16))]
struct AlignedGdt([u64; GDT_ENTRIES]);

/// Descriptors in the GDT: null, kernel code, kernel data, user data, user code, TSS low,
/// TSS high. The trampoline defines the same layout; the user pair is data-below-code for
/// `sysret` (see `boot/boot.S`).
const GDT_ENTRIES: usize = 7;

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

static GDT: SyncCell<AlignedGdt> = SyncCell::new(AlignedGdt([0; GDT_ENTRIES]));
static TSS: SyncCell<AlignedTss> = SyncCell::new(AlignedTss(TaskStateSegment::empty()));
static IDT: SyncCell<AlignedIdt> = SyncCell::new(AlignedIdt([IdtGate::MISSING; IDT_ENTRIES]));
static IST_NMI: SyncCell<IstStack> = SyncCell::new(IstStack::empty());
static IST_DOUBLE_FAULT: SyncCell<IstStack> = SyncCell::new(IstStack::empty());
static IST_MACHINE_CHECK: SyncCell<IstStack> = SyncCell::new(IstStack::empty());

extern "C" {
    /// Start of the trampoline's GDT in `boot/boot.S`, at its low (link-time) address.
    #[link_name = "gdt64"]
    static BOOT_GDT: u64;
    /// Top of the kernel's own stack, defined by `kernel/linker.ld`. Ring 0 interrupts find
    /// their stack here; the trampoline's stack is only used to reach this code.
    #[link_name = "_kernel_stack_top"]
    static KERNEL_STACK_TOP: u8;
    /// 256 assembly entry points, in vector order.
    #[link_name = "nolx_isr_stub_table"]
    static ISR_STUB_TABLE: [usize; IDT_ENTRIES];
}

/// Install the TSS and the complete IDT.
///
/// Must run once on the boot processor before external interrupts are enabled. The boot
/// trampoline entered Rust with interrupts disabled and no other processor running.
pub fn init() {
    init_gdt();
    init_tss();
    init_idt();
}

/// Copy the trampoline's descriptors into the kernel's own GDT and load it.
///
/// `mov cr3`-style tricks aside, this is the last step that needs the low mapping: the
/// trampoline's `gdt64` is read *through the image window*, which maps the boot layer's
/// frames at the kernel's image offset. After `lgdt` the processor reads descriptors from
/// kernel memory, and the kernel can drop the identity map.
fn init_gdt() {
    let boot_gdt = phys::KERNEL_IMAGE_BASE + ptr::addr_of!(BOOT_GDT) as u64;
    // SAFETY: `GDT` is written here and by `init_tss`, both during single-processor boot
    // with interrupts disabled; the pointer is used only to copy descriptors into the table
    // this function then loads.
    let gdt = unsafe { ptr::addr_of_mut!((*GDT.get()).0).cast::<u64>() };
    for index in 0..GDT_ENTRIES {
        // SAFETY: `gdt64` is a 7-entry table linked into the boot layer, which the image
        // window maps read-only by way of the kernel's own page tables. The destination is
        // the kernel's aligned copy.
        let descriptor = unsafe { ptr::read_volatile((boot_gdt as *const u64).add(index)) };
        unsafe { gdt.add(index).write(descriptor) };
    }

    let descriptor = DescriptorTablePointer {
        limit: (size_of::<AlignedGdt>() - 1) as u16,
        base: gdt as u64,
    };
    // SAFETY: the table is populated and stays live for the lifetime of the kernel. The
    // selectors already in CS/DS/SS keep their meaning: the new table's first five entries
    // are byte-for-byte the ones the trampoline loaded.
    unsafe {
        asm!(
            "lgdt [{}]",
            in(reg) &descriptor,
            options(nostack, preserves_flags),
        );
    }
}

fn init_tss() {
    // SAFETY: boot-time, single-CPU initialization; the TSS is only written here until
    // interrupt delivery and later CPU bring-up are implemented.
    let tss = unsafe { ptr::addr_of_mut!((*TSS.get()).0) };

    // SAFETY: these raw, unaligned writes follow the packed x86 TSS offsets. The TSS
    // remains exclusively owned by this initializer until `ltr` publishes it to the CPU.
    unsafe {
        ptr::addr_of_mut!((*tss).rsp0).write_unaligned(ptr::addr_of!(KERNEL_STACK_TOP) as u64);
        ptr::addr_of_mut!((*tss).ist1).write_unaligned(stack_top(&IST_NMI));
        ptr::addr_of_mut!((*tss).ist2).write_unaligned(stack_top(&IST_DOUBLE_FAULT));
        ptr::addr_of_mut!((*tss).ist3).write_unaligned(stack_top(&IST_MACHINE_CHECK));
        ptr::addr_of_mut!((*tss).iomap_base).write_unaligned(size_of::<TaskStateSegment>() as u16);
    }

    let [low, high] = tss_descriptor(tss as u64, (size_of::<TaskStateSegment>() - 1) as u32);

    // SAFETY: the kernel's GDT reserves slots 5 and 6 for the 16-byte TSS descriptor, and
    // it is the table currently loaded. Interrupts are still disabled.
    unsafe {
        let gdt = ptr::addr_of_mut!((*GDT.get()).0).cast::<u64>();
        ptr::write_volatile(gdt.add(5), low);
        ptr::write_volatile(gdt.add(6), high);
    }

    // SAFETY: `TSS_SELECTOR` refers to the descriptor just installed in the kernel's GDT.
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

/// Set `TSS.RSP0`: the stack every privilege-level-0 entry from ring 3 switches to.
///
/// The dispatcher calls this on every context switch, because RSP0 is per-*thread* state:
/// a `syscall`, an IRQ, or an exception raised in user mode must land on the kernel stack
/// of the thread that was interrupted, not of whichever thread ran last in the kernel.
pub fn set_rsp0(top: u64) {
    // SAFETY: boot-time structure stays live for the lifetime of the kernel; this write is
    // the documented per-thread update performed by the scheduler with interrupts disabled.
    unsafe {
        let tss = ptr::addr_of_mut!((*TSS.get()).0);
        ptr::addr_of_mut!((*tss).rsp0).write_unaligned(top);
    }
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
