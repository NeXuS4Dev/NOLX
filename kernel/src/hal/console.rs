//! The kernel console.
//!
//! One device from the first instruction that can print: COM1 for the log (which is what
//! QEMU's `-serial stdio` and the CI smoke test read) and the VGA text buffer for a human
//! sitting in front of the machine. Both are written unconditionally; neither is allowed to
//! fail, because a console that can report its own failure is not a console.

use core::fmt;

use super::{inb, outb};

/// COM1's base I/O port.
const COM1: u16 = 0x3F8;
/// Offset of the line status register from the base port.
const COM1_LSR: u16 = 3;
/// The "transmit holding register empty" bit.
const LSR_THRE: u8 = 1 << 5;

/// The VGA text-mode framebuffer: 80x25 cells of (character, attribute) at `0xB8000`.
const VGA_BUFFER: usize = 0x000B_8000;
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
/// single boot processor writes here, and a `static mut` with disciplined access is honest
/// about the assumption.
static mut VGA_ROW: usize = 0;
static mut VGA_COL: usize = 0;

impl Console {
    /// A handle to the console.
    #[must_use]
    pub fn new() -> Self {
        // SAFETY: reading two `usize` statics that only this module writes, on the single
        // boot processor, cannot tear. See the note on [`VGA_ROW`].
        let (row, col) = unsafe { (VGA_ROW, VGA_COL) };
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
            VGA_ROW = self.row;
            VGA_COL = self.col;
        }
    }

    fn vga_write_cell(&self, row: usize, col: usize, byte: u8) {
        if row >= VGA_HEIGHT || col >= VGA_WIDTH {
            return;
        }
        let offset = (row * VGA_WIDTH + col) * 2;
        // SAFETY: `VGA_BUFFER` is the text-mode framebuffer, identity mapped by the boot
        // layer, and `offset` is bounded by the checks above to stay inside the 80x25
        // region. Volatile writes are required because the device reads this memory.
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
