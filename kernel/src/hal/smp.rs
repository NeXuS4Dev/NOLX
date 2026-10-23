//! Application-processor startup, cross-processor proof, and parking across a sleep.
//!
//! The kernel boots on the bootstrap processor and, once its own descriptor tables, local
//! APIC and page tables are live, starts every other enabled processor the MADT describes.
//! An application processor is started the way the architecture requires: an `INIT` IPI puts
//! it in the wait-for-startup state, and a startup IPI names a page below one megabyte it
//! begins executing in real mode. That page holds a trampoline which switches the processor
//! to long mode on the kernel's own page tables and jumps to [`ap_entry`].
//!
//! # What an application processor does
//!
//! It enables its local APIC, installs its own descriptor tables and per-processor block,
//! publishes itself online, and halts. It runs no scheduler thread and owns no device: this
//! kernel's interrupt dispatch, console and locks are still bootstrap-processor code paths.
//! What it *does* prove is that the processor is real -- it answers an inter-processor
//! interrupt through the shared IDT, and the bootstrap processor requires that answer before
//! it reports success.
//!
//! # Parking and resume
//!
//! A sleep requires every processor to be quiet, so each application processor is sent a park
//! interrupt, which it acknowledges and then leaves with interrupts disabled. A wake does not
//! bring them back: the platform resets them, and restarting them is the operating system's
//! job. [`resume`] replays the startup sequence, and each processor re-installs its own state
//! from scratch -- a fresh per-processor block, a fresh descriptor set, a freshly enabled
//! local APIC -- then proves itself again with the same interrupt round trip.

use core::arch::{asm, global_asm};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nolx_hal_core::apic::IpiDelivery;
use nolx_hal_core::console::HalConsole;

use crate::sync::SyncCell;

use super::interrupts::InterruptFrame;

/// How many logical processors this kernel can run, the bootstrap processor included.
pub const MAX_CPUS: usize = super::cpu::MAX_CPUS;

/// Physical page the startup trampoline is copied to.
///
/// It must be below one megabyte, 4 KiB aligned, and is addressed by the startup IPI as
/// `vector << 12`. The first megabyte is reserved in full by the frame allocator, so nothing
/// else in the kernel can be handed this page. `0x8000`-`0xbfff` belongs to the S3 hand-off,
/// so this sits between it and the EBDA.
const TRAMPOLINE: u64 = 0xf000;
/// The startup IPI vector naming [`TRAMPOLINE`].
const TRAMPOLINE_VECTOR: u8 = (TRAMPOLINE >> 12) as u8;

/// Offsets inside the trampoline page.
const SLOT_CR4: u64 = 0x200;
const SLOT_CR3: u64 = 0x204;
const SLOT_EFER_LOW: u64 = 0x208;
const SLOT_EFER_HIGH: u64 = 0x20c;
const SLOT_CR0: u64 = 0x210;
const SLOT_STACK: u64 = 0x218;
const SLOT_ENTRY: u64 = 0x220;

/// Size of the kernel stack each application processor runs on.
const AP_STACK_SIZE: usize = 16 * 1024;

/// Park interrupt: the processor acknowledges it, then stops with interrupts disabled.
pub const PARK_VECTOR: u8 = 0xf0;
/// Proof interrupt: the processor counts it and returns.
pub const PING_VECTOR: u8 = 0xf1;

const OFFLINE: u32 = 0;
const STARTING: u32 = 1;
const ONLINE: u32 = 2;
const PARKED: u32 = 3;

// Bounded waits, in microseconds of wall time. A processor that does not answer is reported,
// never assumed.
const ONLINE_WAIT_MICROS: u64 = 250_000;
const PING_WAIT_MICROS: u64 = 20_000;

// The trampoline. It is copied to `TRAMPOLINE` and entered in real mode with the segment
// registers cleared, so every address in the first two stages is absolute and below 64 KiB:
// the page it runs from, its own stack inside that page, the GDT and its descriptor at fixed
// offsets. The third stage runs in long mode at the same low address, which the kernel's own
// page tables map for the duration of startup, and jumps to `ap_entry` through a data slot.
//
// A processor that is reset to real mode starts with `%cs = vector << 8` (`0x0f00` here) and
// `%eip = 0`, so the first instruction is the first byte of this blob.
global_asm!(
    r#"
    .section .text.ap_boot,"ax",@progbits
    .global ap_boot_start
    .global ap_boot_end

ap_boot_start:
    .code16
    cli
    cld
    xorw %ax, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $0x0f800, %esp
    lgdt 0x0f140
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $0x0f040

    .org 0x40
    .code32
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $0x0f800, %esp
    movl 0x0f200, %eax
    movl %eax, %cr4
    movl 0x0f204, %eax
    movl %eax, %cr3
    movl $0xc0000080, %ecx
    rdmsr
    orl $0x100, %eax
    wrmsr
    movl 0x0f210, %eax
    movl %eax, %cr0
    ljmpl $0x18, $0x0f080

    .org 0x80
    .code64
    movabs $0x000000000000f218, %rax
    mov (%rax), %rsp
    movabs $0x000000000000f220, %rax
    jmp *(%rax)

    .org 0x100
    .quad 0x0000000000000000
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .org 0x140
    .short 0x0027
    .long 0x0000f100
    .org 0x200
ap_boot_end:
    "#,
    options(att_syntax)
);

extern "C" {
    static ap_boot_start: u8;
    static ap_boot_end: u8;
}

#[repr(C, align(16))]
struct ApStack([u8; AP_STACK_SIZE]);

impl ApStack {
    const fn empty() -> Self {
        Self([0; AP_STACK_SIZE])
    }
}

/// One application processor's state.
struct Slot {
    /// Local APIC identifier firmware assigned it.
    apic_id: AtomicU32,
    /// One of `OFFLINE`/`STARTING`/`ONLINE`/`PARKED`.
    state: AtomicU32,
    /// Interrupts it has answered since it last started.
    pings: AtomicU64,
    /// Interrupts that reached it without an owner. Any value but zero is a defect.
    unexpected: AtomicU64,
    stack: ApStack,
}

impl Slot {
    const fn empty() -> Self {
        Self {
            apic_id: AtomicU32::new(0),
            state: AtomicU32::new(OFFLINE),
            pings: AtomicU64::new(0),
            unexpected: AtomicU64::new(0),
            stack: ApStack::empty(),
        }
    }
}

static SLOTS: SyncCell<[Slot; MAX_CPUS]> = SyncCell::new([const { Slot::empty() }; MAX_CPUS]);
/// Processors this kernel has started at least once.
static RECORDED: AtomicU32 = AtomicU32::new(0);

/// What one startup attempt achieved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Startup {
    /// No application processor is described, so there is nothing to start.
    NoneDescribed,
    /// The trampoline page could not be mapped or addressed.
    Trampoline,
    /// Every described processor is online.
    Started {
        /// How many application processors answered.
        processors: usize,
    },
    /// Some processors did not come online within the bounded wait.
    Partial {
        /// How many came online.
        started: usize,
        /// How many did not.
        refused: usize,
    },
}

fn pages() -> &'static [Slot; MAX_CPUS] {
    // SAFETY: the array is a static that lives for the kernel's lifetime and each slot is
    // claimed by exactly one processor.
    unsafe { &*SLOTS.get() }
}

/// How many application processors are online.
#[must_use]
pub fn online() -> usize {
    pages()
        .iter()
        .skip(1)
        .filter(|slot| slot.state.load(Ordering::Acquire) == ONLINE)
        .count()
}

/// Interrupts an application processor could not account for.
#[must_use]
pub fn unexpected_interrupts() -> u64 {
    pages()
        .iter()
        .map(|slot| slot.unexpected.load(Ordering::Acquire))
        .sum()
}

fn stack_top(index: usize) -> u64 {
    // SAFETY: `index` is a valid slot index at every call site.
    unsafe { pages()[index].stack.0.as_ptr().add(AP_STACK_SIZE) as u64 }
}

/// Start every application processor the firmware describes.
///
/// Called once, after the memory manager and the MMIO window exist, because the startup
/// trampoline is mapped into the kernel's own address space for the duration. Nothing is
/// assumed about a processor that does not answer: it is counted as refused and stays offline.
pub fn start(console: &mut dyn HalConsole) -> Startup {
    if !super::apic::is_enabled() {
        let _ = writeln!(
            console,
            "smp: no local APIC, application processors stay offline"
        );
        console.flush();
        return Startup::NoneDescribed;
    }
    let bsp = super::apic::current_apic_id();
    let described = match super::acpi::discover() {
        Some(platform) => platform.madt.local_apics().to_vec(),
        None => alloc::vec::Vec::new(),
    };
    let mut processors = 0usize;
    let mut skipped = 0usize;
    for processor in described {
        if processor.apic_id == bsp || !processor.enabled() {
            continue;
        }
        if processors + 1 >= MAX_CPUS {
            skipped += 1;
            continue;
        }
        processors += 1;
        let slot = &pages()[processors];
        slot.apic_id.store(processor.apic_id, Ordering::Release);
    }
    RECORDED.store(processors as u32, Ordering::Release);
    let _ = writeln!(
        console,
        "smp: bootstrap processor id {bsp}; {processors} application processor(s) described by firmware{}",
        if skipped > 0 {
            ", extra processors beyond the kernel's capacity stay offline"
        } else {
            ""
        }
    );
    if processors == 0 {
        console.flush();
        return Startup::NoneDescribed;
    }
    if !map_trampoline() {
        let _ = writeln!(
            console,
            "smp: the startup trampoline could not be mapped; application processors stay offline"
        );
        console.flush();
        return Startup::Trampoline;
    }
    write_trampoline();
    let mut started = 0usize;
    for index in 1..=processors {
        if start_one(index) {
            started += 1;
        }
    }
    let acknowledged = ping_all();
    let _ = writeln!(
        console,
        "smp: {started} of {processors} application processor(s) online, {acknowledged} answered an inter-processor interrupt"
    );
    unmap_trampoline();
    console.flush();
    if started == processors && acknowledged == processors {
        Startup::Started { processors }
    } else {
        Startup::Partial {
            started,
            refused: processors - started,
        }
    }
}

/// Park every online application processor before a sleep.
///
/// Returns the number parked. A processor that does not acknowledge is counted as refused and
/// the caller must not sleep: a processor still running is a processor whose view of memory the
/// sleep would freeze mid-flight.
pub fn park(console: &mut dyn HalConsole) -> Result<usize, usize> {
    let mut parked_count = 0usize;
    let mut refused = 0usize;
    for slot in pages().iter().skip(1) {
        if slot.state.load(Ordering::Acquire) != ONLINE {
            continue;
        }
        let id = slot.apic_id.load(Ordering::Acquire);
        if super::apic::send_ipi(id, IpiDelivery::Fixed, PARK_VECTOR).is_err()
            || !wait_for(slot, PARKED, ONLINE_WAIT_MICROS)
        {
            refused += 1;
            continue;
        }
        parked_count += 1;
    }
    if parked_count > 0 || refused > 0 {
        let _ = writeln!(
            console,
            "smp: parked {parked_count} application processor(s) for the sleep, {refused} refused"
        );
    }
    if refused != 0 {
        console.flush();
        return Err(refused);
    }
    // A parked processor is halted, not held: a wake that resets the platform can return it to
    // the firmware entry point, and several processors running firmware bring-up -- or the S3
    // hand-off itself -- alongside the processor that is restoring the machine is a race no
    // lock in this kernel can arbitrate. `INIT` puts each one in the wait-for-startup state,
    // which is the state ACPI requires a non-booting processor to be in for a sleep, and it
    // takes no cooperation from the processor to work.
    let held = hold(console);
    let _ = writeln!(
        console,
        "smp: {held} application processor(s) held in INIT for the sleep"
    );
    console.flush();
    Ok(parked_count)
}

/// Put every processor this kernel started into the wait-for-startup state.
///
/// Returns how many accepted the interrupt. This is not a request the processor can refuse or
/// ignore: `INIT` is delivered to the local APIC, and the processor stops executing whatever it
/// was running. It is safe to repeat -- before a sleep, immediately after a wake, and again
/// before each startup IPI.
pub fn hold(console: &mut dyn HalConsole) -> usize {
    let recorded = RECORDED.load(Ordering::Acquire) as usize;
    let mut held = 0usize;
    for index in 1..=recorded {
        let slot = &pages()[index];
        let id = slot.apic_id.load(Ordering::Acquire);
        if id == 0 {
            continue;
        }
        slot.state.store(OFFLINE, Ordering::Release);
        if super::apic::send_ipi(id, IpiDelivery::Init, 0).is_ok() {
            held += 1;
        }
    }
    let _ = console;
    held
}

/// Restart every processor that was parked, and prove each one is taking interrupts again.
///
/// This is the resume half of [`park`]: the wake reset leaves application processors in their
/// reset state, so they are started again from the trampoline and re-install every piece of
/// per-processor state they had before the sleep.
pub fn resume(console: &mut dyn HalConsole) -> (usize, usize) {
    let recorded = RECORDED.load(Ordering::Acquire) as usize;
    if recorded == 0 {
        return (0, 0);
    }
    if !map_trampoline() {
        let _ = writeln!(
            console,
            "smp: application processors cannot be restarted: the startup trampoline could not be mapped"
        );
        console.flush();
        return (0, recorded);
    }
    write_trampoline();
    let mut restarted = 0usize;
    for index in 1..=recorded {
        if start_one(index) {
            restarted += 1;
        }
    }
    let acknowledged = ping_all();
    let _ = writeln!(
        console,
        "smp: resumed {restarted} of {recorded} application processor(s) after the wake, {acknowledged} answered an inter-processor interrupt, {} online, {} unexpected interrupt(s)",
        online(),
        unexpected_interrupts()
    );
    unmap_trampoline();
    console.flush();
    (restarted, recorded - restarted)
}

/// Send one proof interrupt to every online processor and require each to answer.
fn ping_all() -> usize {
    let mut acknowledged = 0usize;
    for slot in pages().iter().skip(1) {
        if slot.state.load(Ordering::Acquire) != ONLINE {
            continue;
        }
        let before = slot.pings.load(Ordering::Acquire);
        let id = slot.apic_id.load(Ordering::Acquire);
        if super::apic::send_ipi(id, IpiDelivery::Fixed, PING_VECTOR).is_err() {
            continue;
        }
        if wait_until(PING_WAIT_MICROS, || {
            slot.pings.load(Ordering::Acquire) > before
        }) {
            acknowledged += 1;
        }
    }
    acknowledged
}

fn start_one(index: usize) -> bool {
    let slot = &pages()[index];
    let id = slot.apic_id.load(Ordering::Acquire);
    slot.state.store(STARTING, Ordering::Release);
    // A jump, not a call: the trampoline enters the entry point with an aligned stack, so the
    // slot holds the value a call would have left (top - 8) and the entry point sees the
    // usual `%rsp % 16 == 8` frame.
    write_slot64(SLOT_STACK, stack_top(index) - 8);
    // The address the trampoline jumps to. A function pointer, not a data reference: the
    // trampoline enters the entry point in the same state a `call` would have left.
    let entry_point: extern "C" fn() -> ! = ap_entry;
    let entry = entry_point as usize as u64;
    write_slot64(SLOT_ENTRY, entry);

    // INIT puts the processor in the wait-for-startup state; the startup IPI then names the
    // trampoline page. The delays are architectural minimums, not tuning: the processor needs
    // time to leave the INIT state before it can accept the vector.
    if super::apic::send_ipi(id, IpiDelivery::Init, 0).is_err() {
        return false;
    }
    delay_micros(10_000);
    if super::apic::send_ipi(id, IpiDelivery::Startup, TRAMPOLINE_VECTOR).is_err() {
        return false;
    }
    delay_micros(200);
    if wait_for(slot, ONLINE, ONLINE_WAIT_MICROS) {
        return true;
    }
    // One retry with a second startup IPI: a processor that missed the first vector because it
    // was still leaving the INIT state is common enough in emulation to be worth the second try.
    let _ = super::apic::send_ipi(id, IpiDelivery::Startup, TRAMPOLINE_VECTOR);
    delay_micros(200);
    wait_for(slot, ONLINE, ONLINE_WAIT_MICROS)
}

fn wait_for(slot: &Slot, state: u32, micros: u64) -> bool {
    wait_until(micros, || slot.state.load(Ordering::Acquire) == state)
}

/// Poll `done` for at most `micros` microseconds.
///
/// The bound is wall time, not a spin count: a processor that never answers has to be reported
/// in the same amount of time on a fast host and a slow one, and an emulator's instruction cost
/// has nothing to do with either. Where there is no counter to measure with, a spin count
/// derived from the same budget is the fallback.
fn wait_until(micros: u64, mut done: impl FnMut() -> bool) -> bool {
    let budget = fallback_spins(micros);
    if let Some(limit) = timestamp_ticks(micros) {
        if let Some(start) = super::apic::timestamp() {
            let mut remaining = budget;
            while remaining != 0 {
                if done() {
                    return true;
                }
                if super::apic::timestamp().is_none_or(|now| now.wrapping_sub(start) >= limit) {
                    return done();
                }
                remaining -= 1;
                core::hint::spin_loop();
            }
        }
    }
    if let Some(limit) = counter_ticks(micros) {
        if let Some(start) = super::hpet::counter() {
            let mut previous = start;
            let mut elapsed = 0u64;
            let mut remaining = budget;
            while remaining != 0 {
                if done() {
                    return true;
                }
                let Some(current) = super::hpet::counter() else {
                    break;
                };
                elapsed = elapsed.saturating_add(super::hpet::elapsed_ticks(previous, current));
                if elapsed >= limit {
                    return done();
                }
                previous = current;
                remaining -= 1;
                core::hint::spin_loop();
            }
        }
    }
    for _ in 0..budget {
        if done() {
            return true;
        }
        core::hint::spin_loop();
    }
    done()
}

fn delay_micros(micros: u64) {
    if let Some(limit) = timestamp_ticks(micros) {
        if let Some(start) = super::apic::timestamp() {
            while super::apic::timestamp().is_some_and(|now| now.wrapping_sub(start) < limit) {
                core::hint::spin_loop();
            }
            return;
        }
    }
    if let Some(ticks) = counter_ticks(micros) {
        if let Some(start) = super::hpet::counter() {
            let _ = super::hpet::wait_until_elapsed(start, ticks);
            return;
        }
    }
    // Neither counter is usable: fall back to a bounded spin. It is deliberately capped so a
    // platform that lost both counters reports a processor that never answers instead of
    // taking minutes to say so.
    for _ in 0..fallback_spins(micros) {
        core::hint::spin_loop();
    }
}

/// Timestamp-counter ticks `micros` is worth, when the TSC was calibrated.
fn timestamp_ticks(micros: u64) -> Option<u64> {
    let frequency = super::apic::timestamp_frequency_hz()?;
    Some((frequency.saturating_mul(micros) / 1_000_000).max(1))
}

/// How many spins stand in for `micros` when no counter is usable at all.
fn fallback_spins(micros: u64) -> u64 {
    micros.saturating_mul(1_000).min(40_000_000)
}

/// How many counter ticks `micros` is worth, if this platform's counter can be trusted.
///
/// A wake reset can leave the HPET stopped while the mapping and the frequency the boot path
/// measured are still in the kernel's statics. Every delay and every bounded wait here goes
/// through this check, because trusting a frozen counter turns a ten-millisecond architectural
/// delay into the whole spin budget -- and a processor restart into something that never
/// finishes. The check is sampled, not assumed: the counter has to move.
fn counter_ticks(micros: u64) -> Option<u64> {
    let frequency = super::hpet::frequency_hz()?;
    let start = super::hpet::counter()?;
    let mut moved = false;
    for _ in 0..10_000 {
        if super::hpet::counter()? != start {
            moved = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !moved {
        return None;
    }
    Some((frequency.saturating_mul(micros) / 1_000_000).max(1))
}

fn write_slot64(offset: u64, value: u64) {
    // SAFETY: the trampoline page is mapped identity for the duration of startup, is inside the
    // first megabyte the frame allocator reserves, and the offset is inside that page.
    unsafe {
        let slot = crate::mm::phys::direct(TRAMPOLINE + offset) as *mut u64;
        slot.write_unaligned(value);
    }
}

fn write_trampoline() {
    // SAFETY: as `write_slot64`; the blob is copied into the mapped page and every slot lies
    // inside it.
    unsafe {
        let start = core::ptr::addr_of!(ap_boot_start);
        let end = core::ptr::addr_of!(ap_boot_end);
        let length = end as usize - start as usize;
        assert!(length <= 0x200, "the startup trampoline outgrew its page");
        let low = crate::mm::phys::direct(TRAMPOLINE) as *mut u8;
        core::ptr::copy_nonoverlapping(start, low, length);

        let cr0: u64;
        let cr3: u64;
        let cr4: u64;
        let efer_low: u32;
        let efer_high: u32;
        asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack, preserves_flags));
        asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack, preserves_flags));
        asm!(
            "rdmsr",
            in("ecx") 0xc000_0080u32,
            out("eax") efer_low,
            out("edx") efer_high,
            options(nomem, nostack, preserves_flags)
        );
        (low.add(SLOT_CR4 as usize) as *mut u32).write_unaligned(cr4 as u32);
        (low.add(SLOT_CR3 as usize) as *mut u32).write_unaligned(cr3 as u32);
        (low.add(SLOT_CR0 as usize) as *mut u32).write_unaligned(cr0 as u32);
        (low.add(SLOT_EFER_LOW as usize) as *mut u32).write_unaligned(efer_low);
        (low.add(SLOT_EFER_HIGH as usize) as *mut u32).write_unaligned(efer_high);
    }
}

/// Map the trampoline page identity in the kernel's address space.
///
/// A processor entering real mode addresses memory physically, so the page it starts on must
/// answer at its own address; and the long-mode stage that follows must be able to keep
/// fetching from it after `CR3` is loaded. The mapping exists only while processors are being
/// started: nothing in the kernel may depend on low memory being addressable.
fn map_trampoline() -> bool {
    use nolx_mm_core::paging::{EntryFlags, PageTables, Protection};
    let mut store = crate::mm::tables::KernelTableStore::new();
    let mut tables = PageTables::new(crate::mm::space::kernel_root());
    let flags = EntryFlags::for_kernel(Protection::from_bits(
        Protection::READ | Protection::WRITE | Protection::EXECUTE,
    ));
    tables
        .map_range(
            &mut store,
            TRAMPOLINE,
            TRAMPOLINE,
            crate::mm::PAGE_SIZE as u64,
            flags,
        )
        .is_ok()
}

fn unmap_trampoline() {
    use nolx_mm_core::paging::PageTables;
    let mut store = crate::mm::tables::KernelTableStore::new();
    let mut tables = PageTables::new(crate::mm::space::kernel_root());
    let _ = tables.unmap_shared(&mut store, TRAMPOLINE);
    // SAFETY: the trampoline page is no longer mapped anywhere, so no translation for it can
    // remain in this processor's TLB. Application processors are already past it.
    unsafe {
        asm!("invlpg [{}]", in(reg) TRAMPOLINE, options(nostack, preserves_flags));
    }
}

/// The first code an application processor runs after the trampoline.
///
/// It runs with the kernel's page tables, a private stack, and interrupts still disabled: it
/// installs the state every interrupt path depends on before allowing one to arrive.
#[no_mangle]
pub extern "C" fn ap_entry() -> ! {
    if super::apic::init_ap().is_err() {
        idle_forever();
    }
    let id = super::apic::current_apic_id();
    let Some(index) = slot_for(id) else {
        idle_forever();
    };
    let top = stack_top(index);
    // SAFETY: the slot belongs to this processor (its local APIC identifier is unique among the
    // processors the kernel started), and this is the processor that owns it.
    unsafe { super::cpu::init_ap(index, super::cpu::features(), top) };
    super::tables::init_ap(index, top);
    pages()[index].state.store(ONLINE, Ordering::Release);
    // SAFETY: the IDT, GDT, TSS and per-processor block are installed, so an interrupt taken
    // from here on finds a complete execution environment.
    unsafe { asm!("sti", options(nostack)) };
    loop {
        // The processor has no work of its own; inter-processor interrupts are the only thing
        // that wakes it, and dropping to a halt keeps the host from spinning a core.
        // SAFETY: halting with interrupts enabled is woken by any pending interrupt.
        unsafe { asm!("hlt", options(nomem, nostack)) }
    }
}

fn slot_for(apic_id: u32) -> Option<usize> {
    pages()
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, slot)| slot.apic_id.load(Ordering::Acquire) == apic_id)
        .map(|(index, _)| index)
}

fn idle_forever() -> ! {
    // A processor that cannot install its own state must not take interrupts: it stops with
    // them disabled and stays out of the kernel's way.
    loop {
        // SAFETY: interrupts are disabled and the processor has nothing left to do.
        unsafe { asm!("cli", "hlt", options(nomem, nostack)) }
    }
}

/// Dispatch an interrupt that arrived on an application processor.
///
/// An application processor has no device, no console and no timekeeping. Only the two
/// inter-processor vectors are its own; anything else is counted and acknowledged so that a
/// stray delivery cannot wedge the processor, and the count is what the bootstrap processor
/// checks before it calls the startup a success.
pub fn on_ap_interrupt(frame: *mut InterruptFrame, vector: u8) -> *mut InterruptFrame {
    let index = slot_for(super::apic::current_apic_id());
    if vector >= 32 {
        super::apic::end_of_interrupt();
    }
    let Some(index) = index else {
        return frame;
    };
    let slot = &pages()[index];
    match vector {
        PING_VECTOR => {
            slot.pings.fetch_add(1, Ordering::AcqRel);
        }
        PARK_VECTOR => {
            slot.state.store(PARKED, Ordering::Release);
            // The sleep is the next thing the bootstrap processor does. This processor must not
            // take another interrupt -- its local APIC and everything behind it are about to be
            // reset -- so it stops here with interrupts disabled. A wake restarts it from the
            // trampoline; it never returns through the interrupt frame.
            idle_forever();
        }
        _ => {
            slot.unexpected.fetch_add(1, Ordering::AcqRel);
        }
    }
    frame
}
