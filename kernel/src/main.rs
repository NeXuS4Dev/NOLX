//! The NOLX kernel.
//!
//! # Module map
//!
//! The layout follows the kernel model in `docs/architecture/01-kernel-model.md`:
//!
//! * [`hal`] -- hardware abstraction: console, CPU, firmware hand-off.
//! * [`mm`] -- physical frames and the kernel address space.
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
/// # Safety
///
/// Called exactly once, with interrupts disabled, on the boot stack. If `magic` is
/// [`MULTIBOOT_BOOTLOADER_MAGIC`], `mb_info` must point to a valid Multiboot information
/// structure that remains mapped during the kernel's initial parse.
#[no_mangle]
extern "C" fn nolx_kernel_main(mb_info: *const u8, magic: u32) -> ! {
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

    #[cfg(any(feature = "hal-fault-test-pf", feature = "hal-fault-test-gp"))]
    let _ = (mb_info, magic);

    #[cfg(feature = "hal-fault-test-pf")]
    hal::interrupts::trigger_page_fault_test();
    #[cfg(feature = "hal-fault-test-gp")]
    hal::interrupts::trigger_general_protection_test();

    #[cfg(not(any(feature = "hal-fault-test-pf", feature = "hal-fault-test-gp")))]
    {
        if magic == MULTIBOOT_BOOTLOADER_MAGIC {
            // SAFETY: the boot contract guarantees `mb_info` is valid when the magic
            // matches. `BootInfo::parse` bounds-checks every field it reads.
            let info = unsafe { hal::multiboot::BootInfo::parse(mb_info) };
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

        obj::init(&mut console);
        let _ = writeln!(console, "NOLX: objmgr online");

        exec::init(&mut console);
        psx::init(&mut console);
        io::init(&mut console);

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

        let mut timer_marker_reported = false;
        loop {
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
