//! The NOLX kernel.
//!
//! # Module map
//!
//! The layout follows the kernel model in `docs/architecture/01-kernel-model.md`:
//!
//! * [`hal`] -- hardware abstraction: console, CPU, firmware hand-off.
//! * [`mm`] -- physical frames, the kernel heap, and the kernel address space.
//! * [`obj`] -- the Object Manager: object headers, types, and the namespace.
//! * [`exec`] -- process and thread objects and the dispatcher.
//! * [`psx`] -- the POSIX subsystem, where the standard's semantics live.
//! * [`io`] -- the I/O Manager: driver objects, device stacks, and the IORB.
//!
//! # The `unsafe` budget
//!
//! This crate contains `unsafe` code, all of it in [`hal`] and [`mm`], and every block
//! carries a `// SAFETY:` comment naming the invariant it depends on. `make lint` enforces
//! that. Elsewhere, safe Rust is a hard requirement, not a preference.

#![no_std]
#![no_main]
#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

// The kernel has a heap of its own ([`mm::heap`]) and no `std` above it: `alloc` gives the
// collections the executive's tables are made of, without pulling in a filesystem, a thread
// runtime, or a global `stdout` that would have to be dealt with before the console exists.
extern crate alloc;

pub mod exec;
pub mod hal;
pub mod io;
pub mod mm;
pub mod obj;
pub mod psx;
mod sync;

use core::fmt::Write;
use core::panic::PanicInfo;

use hal::console::Console;
use nolx_hal_core::console::HalConsole;

/// The Multiboot magic value a compliant loader passes in `eax`.
pub const MULTIBOOT_BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

/// A no-op Rust function called by the boot trampoline before the full entry point.
#[no_mangle]
extern "C" fn nolx_probe0() {}

/// Prove that the trampoline can call a Rust function that writes to COM1.
#[no_mangle]
extern "C" fn nolx_probe() {
    hal::console::raw_put_str("NOLX: nolx_probe reached (Rust is callable)\n");
}

#[cfg(all(feature = "hal-fault-test-pf", feature = "hal-fault-test-gp"))]
compile_error!("enable at most one HAL fault-test feature at a time");

/// The kernel entry point, called from `boot/boot.S` in 64-bit long mode.
///
/// `mb_info_physical` is the *physical* address of the loader's information structure, not a
/// pointer: the trampoline ran in a window where the two were the same number, and this side
/// of the switch they are not. Everything the kernel reads from firmware goes through the
/// direct map, and what it hands back is an address in the kernel's own address space.
///
/// # Safety
///
/// Called exactly once, with interrupts disabled, on the kernel's own stack, running in the
/// image window. If `magic` is [`MULTIBOOT_BOOTLOADER_MAGIC`], `mb_info_physical` must point
/// to a valid Multiboot information structure inside the direct map, and must stay there for
/// the kernel's initial parse.
#[no_mangle]
extern "C" fn nolx_kernel_main(mb_info_physical: u64, magic: u32) -> ! {
    // The very first thing the kernel does is prove it is running. This goes straight to
    // COM1, before any static is read and before the framebuffer is assumed to be mapped.
    hal::console::raw_put_str("NOLX: nolx_kernel_main entered\n");

    let mut console = Console::new();
    hal::console::raw_put_str("NOLX: console constructed\n");

    let _ = writeln!(console, "NOLX {} starting", nolx_types::version_string());
    hal::console::raw_put_str("NOLX: first writeln! completed\n");

    // Install the full exception/interrupt path before parsing firmware structures or
    // initializing other subsystems. The PIC stays masked until the final idle path.
    hal::init(&mut console);
    let _ = writeln!(console, "NOLX: hal online");

    // The kernel's own GDT, IDT, and stack are live now, so nothing needs a low address to be
    // a valid address: drop the boot identity map and confirm what survived. A failed check
    // is not something to boot through -- continuing would mean running with an address space
    // whose contents nobody has verified.
    let switch = mm::tables::drop_identity_map();
    let _ = writeln!(
        console,
        "mm: higher-half switch: image window at {:#x}, dropped {} identity-map PML4 \
         entry/entries; identity map gone={}, direct map reachable={}, image window \
         reachable={}",
        mm::KERNEL_IMAGE_BASE,
        switch.dropped_entries,
        !switch.identity_still_reachable,
        switch.direct_map_reachable,
        switch.image_reachable
    );
    if switch.dropped_entries == 0 || switch.identity_still_reachable {
        panic!("mm: the boot identity map was not dropped");
    }
    if !switch.direct_map_reachable || !switch.image_reachable {
        panic!("mm: the higher-half switch left the kernel unreachable");
    }

    #[cfg(any(feature = "hal-fault-test-pf", feature = "hal-fault-test-gp"))]
    let _ = (mb_info_physical, magic);

    #[cfg(feature = "hal-fault-test-pf")]
    hal::interrupts::trigger_page_fault_test();
    #[cfg(feature = "hal-fault-test-gp")]
    hal::interrupts::trigger_general_protection_test();

    #[cfg(not(any(feature = "hal-fault-test-pf", feature = "hal-fault-test-gp")))]
    {
        if magic == MULTIBOOT_BOOTLOADER_MAGIC {
            // The loader's structure is a physical address inside the direct map; the same
            // goes for every address it contains, which `BootInfo` hands over unchanged and
            // its readers convert at the point of use.
            let structure = mm::phys::direct_ptr::<u8>(mb_info_physical);
            // SAFETY: the boot contract guarantees the structure is valid when the magic
            // matches. `BootInfo::parse` bounds-checks every field it reads.
            let info = unsafe { hal::multiboot::BootInfo::parse(structure) };
            let _ = writeln!(console, "multiboot: {info}");
            mm::init(&mut console, &info);
        } else {
            let _ = writeln!(
                console,
                "multiboot: magic {magic:#010x} is not {MULTIBOOT_BOOTLOADER_MAGIC:#010x}; \
                 continuing without firmware information"
            );
            mm::init_without_firmware(&mut console);
        }

        // Device register windows. The table for the MMIO window is created now, before any
        // address space exists, because spaces share the kernel's top-level entries. The
        // self check maps the local APIC through the window and compares it with the direct map.
        if let Err(error) = mm::mmio::init() {
            panic!("mmio: window init failed: {error:?}");
        }
        mm::mmio::selfcheck(&mut console);
        hal::acpi::evaluate_devices(&mut console);
        hal::pci::select_transport(&mut console);
        hal::pci::prepare_resources(&mut console);
        // The virtio block device: bring it up and read sector 0, polled. Failure is reported
        // and boot goes on.
        hal::virtio_blk::probe(&mut console);
        // The AHCI controller: the same sector-0 read through a different driver, polled.
        hal::ahci::probe(&mut console);
        hal::nvme::probe(&mut console);
        hal::xhci::probe(&mut console);
        hal::e1000::probe(&mut console);
        hal::virtio_net::probe(&mut console);
        hal::blockdev::report(&mut console);

        obj::init(&mut console);
        let _ = writeln!(console, "NOLX: objmgr online");
        // The executive's view of the pools the memory manager charges. Printing it is what
        // proves the charging sites -- page tables, heap regions -- reached the accounting.
        obj::pool::report(&mut console);

        if let Err(error) = obj::boot::init(&mut console) {
            panic!("obj: namespace init failed: {error:?}");
        }
        // The object manager checks its own reference rules before anything depends on them.
        // A failure is fatal for the same reason the memory self test's is.
        if let Err(error) = obj::boot::selftest(&mut console) {
            let _ = writeln!(console, "NOLX: obj selftest: FAILED: {error:?}");
            panic!("obj selftest: {error:?}");
        }

        exec::init(&mut console);
        psx::init(&mut console);
        io::init(&mut console);
        // Device discovery: enumerate PCI configuration space before the timer starts, so the
        // paired address/data port accesses have no competing interrupt-time users.
        hal::pci::init(&mut console);
        hal::ps2::init(&mut console);

        hal::blockdev::retained_selftest(&mut console);
        hal::driver_start::report(&mut console);
        hal::init_wall_clock(&mut console);
        let _ = writeln!(
            console,
            "NOLX: init sequence complete, halting the boot processor"
        );

        // Start the selected periodic source after every one-time initializer has finished.
        // The interrupt path only increments a counter; output stays in this idle context.
        hal::start_timer();
        let _ = writeln!(
            console,
            "hal: active clock source={} nominal {} Hz",
            hal::timer::backend_name(),
            hal::timer::frequency_hz(),
        );
        // SAFETY: the complete IDT and selected timer route are live. Do not let the compiler
        // move initialization across STI.
        unsafe { core::arch::asm!("sti", options(nostack)) };
        // The BSP starts three independent kernel stacks and lets the hardware clock preempt
        // them through the common interrupt-frame switch path. The test announces its output
        // with interrupts masked so no worker can print ahead of the prefix.
        exec::begin_round_robin(&mut console);

        let mut timer_marker_reported = false;
        let mut memory_selftest_done = false;
        #[cfg(feature = "hal-driver-lifecycle-test")]
        let mut lifecycle_test_done = false;
        let mut round_robin_reported = false;
        let mut user_test_started = false;
        let mut user_test_reported = false;
        #[cfg(feature = "hal-power-test-s5")]
        let mut power_test_started = false;
        loop {
            hal::xhci::poll_events(&mut console);
            hal::input::report_events(&mut console);
            #[cfg(feature = "hal-xhci-lifecycle-test")]
            if memory_selftest_done {
                hal::xhci::lifecycle_selftest(&mut console);
            }
            #[cfg(feature = "hal-input-object-test")]
            hal::input::hardware_selftest(&mut console);
            if !round_robin_reported && exec::round_robin_complete() {
                round_robin_reported = true;
                if !exec::report_round_robin(&mut console) {
                    panic!("round-robin context-switch test produced an unexpected trace");
                }
            }

            // The M6 ring-3 phase runs right after the kernel-thread round-robin, while the
            // clock is live: the same boot run carries both T2 tests. It must finish before
            // the memory-manager selftests below, which activate their own address spaces
            // and so cannot interleave with live user threads.
            if round_robin_reported && !user_test_started {
                user_test_started = true;
                exec::begin_user_test(&mut console);
            }
            if user_test_started && !user_test_reported && exec::user_test_complete() {
                user_test_reported = true;
                if !exec::report_user_test(&mut console) {
                    panic!("user mode T2 test produced an unexpected evidence chain");
                }
            }

            let ticks = hal::timer::ticks();
            if !timer_marker_reported && ticks >= 100 {
                let elapsed_ns = hal::timer::monotonic_ns();
                let realtime_ns = hal::rtc::unix_time_ns();
                let _ = writeln!(
                    console,
                    "NOLX: timer reached 100 ticks; nominal {} Hz, monotonic {} ns, realtime={:?} ns",
                    hal::timer::frequency_hz(),
                    elapsed_ns,
                    realtime_ns,
                );
                timer_marker_reported = true;
            }

            // Exercise the frame allocator once the clock is live, so the stress runs while
            // timer interrupts keep arriving instead of in the quiet window before they
            // start. A failure is fatal: a memory manager that did not survive its own self
            // test must not go on to serve allocations.
            if timer_marker_reported && user_test_reported && !memory_selftest_done {
                memory_selftest_done = true;
                if let Err(message) = mm::selftest::run(&mut console) {
                    let _ = writeln!(console, "NOLX: mm selftest: FAILED: {message}");
                    panic!("mm selftest: {message}");
                }
                // The heap is exercised next, through the same allocator the executive uses:
                // vectors, tree maps, and one request large enough to force a new region.
                if let Err(message) = mm::heap::selftest(&mut console) {
                    let _ = writeln!(console, "NOLX: kernel heap selftest: FAILED: {message}");
                    panic!("kernel heap selftest: {message}");
                }
                // And finally the address spaces: page tables built out of frames, the four
                // fault causes, and the proof that two spaces that share pages give every
                // frame and every table back.
                if let Err(message) = mm::space::selftest::run(&mut console) {
                    let _ = writeln!(console, "NOLX: address space selftest: FAILED: {message}");
                    panic!("address space selftest: {message}");
                }
            }

            #[cfg(feature = "hal-driver-lifecycle-test")]
            if memory_selftest_done && !lifecycle_test_done {
                lifecycle_test_done = true;
                hal::driver_start::lifecycle_selftest(&mut console);
            }

            #[cfg(feature = "hal-power-test-s5")]
            if memory_selftest_done && !power_test_started {
                power_test_started = true;
                // The dedicated power test refuses any active PCI bus master before calling
                // the primitive. Production suspend/shutdown must supply a driver quiesce policy.
                if hal::power::test_vm_has_active_busmaster() {
                    let _ = writeln!(console, "power: S5 refused: active PCI bus master");
                } else {
                    // SAFETY: this feature is run only by the single-CPU QEMU test with no
                    // DMA devices; all PCI bus masters were checked inactive above.
                    if let Err(error) = unsafe { hal::power::request_s5_quiesced(&mut console) } {
                        panic!("power: S5 request failed: {error:?}");
                    }
                }
            }

            // SAFETY: this boot processor has no runnable work. The selected periodic timer
            // interrupt wakes HLT; NMI remains available to a debugger or platform monitor.
            unsafe { core::arch::asm!("hlt", options(nomem, nostack)) };
        }
    }
}

/// The kernel's panic handler.
///
/// A kernel panic is fatal: there is no supervisor to return to. The handler prints what it
/// can and stops, because continuing with a broken invariant would corrupt data.
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    let mut console = Console::new();
    let _ = writeln!(console, "\r\nPANIC: {info}");
    let _ = writeln!(
        console,
        "NOLX has stopped. The filesystem is consistent up to the last checkpoint; \
         reboot to continue."
    );
    console.flush();

    // Disable interrupts so a timer cannot fire into a half-torn-down kernel.
    // SAFETY: `cli` only clears the interrupt flag; no memory is touched.
    unsafe { core::arch::asm!("cli") };
    loop {
        // SAFETY: as above; this is the terminal idle state.
        unsafe { core::arch::asm!("hlt") };
    }
}
