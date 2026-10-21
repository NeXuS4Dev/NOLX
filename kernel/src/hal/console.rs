//! The kernel console.
//!
//! One device from the first instruction that can print: COM1 for the log (which is what
//! QEMU's `-serial stdio` and the CI smoke test read) and the VGA text buffer for a human
//! sitting in front of the machine. Both are written unconditionally; neither is allowed to
//! fail, because a console that can report its own failure is not a console.

use core::fmt;

use crate::sync::SyncCell;
use nolx_hal_core::console::HalConsole;

use super::{inb, outb};

/// COM1's base I/O port.
const COM1: u16 = 0x3F8;
/// Offset of the line status register from the base port.
///
/// Five. Offset 3 is the FIFO control register, which is write-only on a 16550A: reading
/// it reports nothing, so a transmit loop that waits on it waits forever. This is not a
/// hypothetical -- `COM1_LSR` was three, and the effect was that the kernel's very first
/// Rust function spun inside its first character write. No fault, no output, no way to
/// tell from a serial log that anything had gone wrong.
const COM1_LSR: u16 = 5;

/// `boot/boot.S` waits on the same register using the absolute port `0x3FD`. The two have
/// to agree, and this is checked while compiling rather than discovered while booting.
const _: () = assert!(
    COM1 + COM1_LSR == 0x3FD,
    "COM1's line status register is at 0x3FD"
);
/// The "transmit holding register empty" bit.
const LSR_THRE: u8 = 1 << 5;
/// The transmitter and holding register are both empty.
const LSR_TEMT: u8 = 1 << 6;

/// The VGA text-mode framebuffer: 80x25 cells of (character, attribute) at `0xB8000`.
/// The text-mode framebuffer's physical address.
const VGA_BUFFER_PHYS: u64 = 0x000B_8000;
/// Where the kernel reaches it: the framebuffer is inside the direct map, not at a low
/// address the kernel could take for a pointer.
const VGA_BUFFER: usize = crate::mm::phys::direct(VGA_BUFFER_PHYS);
/// Columns in text mode.
const VGA_WIDTH: usize = 80;
/// Rows in text mode.
const VGA_HEIGHT: usize = 25;
/// Light grey on black.
const VGA_ATTRIBUTE: u8 = 0x07;

/// The kernel console.
///
/// Stateless: the cursor positions live in the hardware and in the static below, so a
/// `Console` can be constructed anywhere, including from the panic handler.
#[derive(Clone, Copy, Debug)]
pub struct Console {
    /// Row the VGA cursor is on.
    row: usize,
    /// Column the VGA cursor is on.
    col: usize,
}

/// The VGA cursor, kept in a static so every `Console` instance agrees.
///
/// A spinlock would be the right answer once there is more than one CPU (M15). Until then a
/// single boot processor writes here, and [`SyncCell`] names that assumption in one place.
static VGA_ROW: SyncCell<usize> = SyncCell::new(0);
static VGA_COL: SyncCell<usize> = SyncCell::new(0);

/// Write a byte straight to COM1, waiting for the transmit register.
///
/// Exists separately from [`Console`] because the first thing the kernel needs to do is
/// prove that it is running at all, before any static has been touched and before it is
/// known whether the VGA framebuffer is even mapped. This is the most primitive output
/// path in the system, and it is deliberately dumb.
pub fn raw_put_byte(byte: u8) {
    // SAFETY: COM1's line status register reads without side effects, and writing the data
    // register transmits one byte, which is what this function is for.
    unsafe {
        while inb(COM1 + COM1_LSR) & LSR_THRE == 0 {
            core::hint::spin_loop();
        }
        outb(COM1, byte);
    }
}

/// Write a string straight to COM1.
pub fn raw_put_str(text: &str) {
    for byte in text.as_bytes() {
        if *byte == b'\n' {
            raw_put_byte(b'\r');
        }
        raw_put_byte(*byte);
    }
}

impl Console {
    /// A handle to the console.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: reading two `usize` statics that only this module writes, on the single
        // boot processor, cannot tear. See the note on [`VGA_ROW`].
        let (row, col) = unsafe { (*VGA_ROW.get(), *VGA_COL.get()) };
        Self { row, col }
    }

    /// Write one byte to both sinks.
    pub fn put_byte(&mut self, byte: u8) {
        self.serial_put(byte);
        self.vga_put(byte);
    }

    /// Write a string.
    pub fn put_str_raw(&mut self, text: &str) {
        for byte in text.as_bytes() {
            self.put_byte(*byte);
        }
    }

    fn serial_put(&self, byte: u8) {
        // Wait for the transmit holding register to drain. A real driver would use the
        // interrupt-driven path; the boot console must not depend on interrupts working.
        // SAFETY: COM1's line status register is readable at any time and has no side
        // effects on read.
        unsafe {
            while inb(COM1 + COM1_LSR) & LSR_THRE == 0 {
                core::hint::spin_loop();
            }
            // SAFETY: writing the data register transmits the byte, which is exactly what
            // we intend.
            outb(COM1, byte);
        }
    }

    fn vga_put(&mut self, byte: u8) {
        match byte {
            b'\n' => {
                self.col = 0;
                self.row += 1;
            }
            b'\r' => self.col = 0,
            8 => {
                // Backspace: move back one cell and blank it.
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = VGA_WIDTH - 1;
                }
                self.vga_write_cell(self.row, self.col, b' ');
            }
            0x20..=0x7E => {
                self.vga_write_cell(self.row, self.col, byte);
                self.col += 1;
                if self.col == VGA_WIDTH {
                    self.col = 0;
                    self.row += 1;
                }
            }
            // Everything else -- the upper half of the byte range, control characters -- is
            // dropped. A console that prints raw control bytes is a console that can be
            // used to forge its own output.
            _ => {}
        }

        if self.row >= VGA_HEIGHT {
            self.vga_scroll();
            self.row = VGA_HEIGHT - 1;
        }

        // SAFETY: single boot processor; see the note on [`VGA_ROW`].
        unsafe {
            *VGA_ROW.get() = self.row;
            *VGA_COL.get() = self.col;
        }
    }

    fn vga_write_cell(&self, row: usize, col: usize, byte: u8) {
        if row >= VGA_HEIGHT || col >= VGA_WIDTH {
            return;
        }
        let offset = (row * VGA_WIDTH + col) * 2;
        // SAFETY: `VGA_BUFFER` is the text-mode framebuffer reached through the direct
        // map, and `offset` is bounded by the checks above to stay inside the 80x25 region.
        // Volatile writes are required because the device reads this memory.
        unsafe {
            let base = VGA_BUFFER as *mut u8;
            core::ptr::write_volatile(base.add(offset), byte);
            core::ptr::write_volatile(base.add(offset + 1), VGA_ATTRIBUTE);
        }
    }

    fn vga_scroll(&self) {
        for row in 1..VGA_HEIGHT {
            for col in 0..VGA_WIDTH {
                let (character, attribute) = self.vga_read_cell(row, col);
                self.vga_write_cell(row - 1, col, character);
                let _ = attribute;
            }
        }
        for col in 0..VGA_WIDTH {
            self.vga_write_cell(VGA_HEIGHT - 1, col, b' ');
        }
    }

    fn vga_read_cell(&self, row: usize, col: usize) -> (u8, u8) {
        let offset = (row * VGA_WIDTH + col) * 2;
        // SAFETY: as [`Console::vga_write_cell`], reading rather than writing.
        unsafe {
            let base = VGA_BUFFER as *const u8;
            (
                core::ptr::read_volatile(base.add(offset)),
                core::ptr::read_volatile(base.add(offset + 1)),
            )
        }
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Write for Console {
    /// A whole formatted write is one line, so it runs with maskable interrupts disabled.
    ///
    /// Without this, a timer switch in the middle of a `writeln!` lets another thread's output
    /// land inside the line; the serial log then holds two fragments and a fixture that greps
    /// for the line fails on a boot that never misbehaved.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> fmt::Result {
        super::interrupts::without_interrupts(|| fmt::write(self, args))
    }

    fn write_str(&mut self, text: &str) -> fmt::Result {
        // `fmt::Write` callers expect `\n` alone to mean a new line, while the terminal
        // wants CR LF. Translate here so callers can just use `writeln!`.
        for byte in text.as_bytes() {
            if *byte == b'\n' {
                self.put_byte(b'\r');
            }
            self.put_byte(*byte);
        }
        Ok(())
    }
}

impl HalConsole for Console {
    fn flush(&mut self) {
        // VGA writes are volatile stores and are complete before `write_str` returns. For
        // COM1, THRE only says the holding register is empty; TEMT additionally waits for
        // the final byte to leave the shift register, which is required before a fatal halt.
        // SAFETY: the 16550-compatible line-status register is readable without side
        // effects; polling it until TEMT is the documented transmitter-drain operation.
        unsafe {
            while inb(COM1 + COM1_LSR) & LSR_TEMT == 0 {
                core::hint::spin_loop();
            }
        }
    }
}
