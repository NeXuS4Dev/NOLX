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

/// The Multiboot magic value a compliant loader passes in `eax`.
pub const MULTIBOOT_BOOTLOADER_MAGIC: u32 = 0x2BAD_B002;

/// The kernel entry point, called from `boot/boot.S` in 64-bit long mode.
///
/// # Safety
///
/// Called exactly once, with interrupts disabled, on the boot stack, with `mb_info`
/// pointing at a Multiboot information structure that is valid for as long as the kernel
/// runs, or null if `magic` is not [`MULTIBOOT_BOOTLOADER_MAGIC`].
/// A trivial Rust entry point, called from `boot/boot.S` before the real one.
///
/// It exists to answer one question: can the trampoline call into Rust at all? It has no
/// frame worth speaking of and touches nothing, so if this prints and `nolx_kernel_main`
/// does not, the fault is in the kernel entry point rather than in the handoff. It is
/// cheap to keep, and it is the first thing to look at when a future change to the entry
/// path breaks booting.
/// An empty Rust function, called from `boot/boot.S` before [`nolx_probe`].
///
/// It answers one question and nothing else: can the trampoline enter Rust and come back?
/// If this returns and `nolx_probe` does not, the handoff is fine and whatever is wrong is
/// in what `nolx_probe` touches -- one port read and one port write.
#[no_mangle]
extern "C" fn nolx_probe0() {}

#[no_mangle]
extern "C" fn nolx_probe() {
    hal::console::raw_put_str("NOLX: nolx_probe reached (Rust is callable)\n");
}

// Deliberately not `pub`: this is called from `boot/boot.S` and from nowhere in Rust, so it
// is not an API. Keeping it private is also what makes clippy's
// `not_unsafe_ptr_arg_deref` rule inapplicable -- the rule protects Rust callers, and there
// are none. Its contract is the one stated above, and the assembly upholds it.
#[no_mangle]
extern "C" fn nolx_kernel_main(mb_info: *const u8, magic: u32) -> ! {
    // The very first thing the kernel does is prove it is running. This goes straight to
    // COM1, before any static is read and before the framebuffer is assumed to be mapped,
    // so if this line does not appear the failure is in the handoff and not in the kernel.
    hal::console::raw_put_str("NOLX: nolx_kernel_main entered\n");

    let mut console = Console::new();
    hal::console::raw_put_str("NOLX: console constructed\n");

    // The assembly already logged that long mode was entered. Everything from here on has
    // a real console, so this is the first line that can carry detail.
    let _ = writeln!(console, "NOLX {} starting", nolx_types::version_string());
    hal::console::raw_put_str("NOLX: first writeln! completed\n");

    if magic == MULTIBOOT_BOOTLOADER_MAGIC {
        // SAFETY: the contract above guarantees `mb_info` is a valid Multiboot info
        // structure whenever the magic matches. `BootInfo::parse` bounds-checks every
        // field it reads against the structure's own length.
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

    let _ = writeln!(console, "NOLX: hal online");

    obj::init(&mut console);
    let _ = writeln!(console, "NOLX: objmgr online");

    exec::init(&mut console);
    psx::init(&mut console);
    io::init(&mut console);

    let _ = writeln!(
        console,
        "NOLX: init sequence complete, halting the boot processor"
    );

    // There is no scheduler to hand off to yet (M5). Halt with interrupts enabled so an
    // NMI or a debugger can still get in.
    loop {
        // SAFETY: `hlt` on the boot processor with no runnable work is the intended idle
        // state; it does not modify memory and returns on any interrupt.
        unsafe { core::arch::asm!("hlt") };
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

    // Disable interrupts so a timer cannot fire into a half-torn-down kernel.
    // SAFETY: `cli` only clears the interrupt flag; no memory is touched.
    unsafe { core::arch::asm!("cli") };
    loop {
        // SAFETY: as above; this is the terminal idle state.
        unsafe { core::arch::asm!("hlt") };
    }
}
