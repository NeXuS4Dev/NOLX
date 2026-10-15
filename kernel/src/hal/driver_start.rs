//! Support for drivers whose bring-up runs as a framework start operation.
//!
//! The framework's start callback is a plain function with no console and no arguments. So a
//! driver's bring-up writes its output into a [`Log`], the start callback keeps the log and the
//! result in the driver's static state, and the probe replays the log on the real console once the
//! framework has returned. The queue lock is never held while the bring-up runs.

use core::fmt::{self, Write};

use nolx_df_core::{Driver, Error, Registry};
use nolx_hal_core::console::HalConsole;

/// Bytes kept for one bring-up's output. Output past this is dropped, and the replay says so.
const CAPACITY: usize = 4096;

/// One bring-up's output, kept until the probe prints it.
pub struct Log {
    buf: [u8; CAPACITY],
    len: usize,
    cut: bool,
}

impl Log {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [0; CAPACITY],
            len: 0,
            cut: false,
        }
    }

    /// Write the kept output to the console, then the cut notice if output was dropped.
    pub fn replay(&self, console: &mut dyn HalConsole) {
        // The buffer only ever ends on a character boundary, so the slice is valid UTF-8.
        if let Ok(text) = core::str::from_utf8(&self.buf[..self.len]) {
            let _ = console.write_str(text);
        }
        if self.cut {
            let _ = writeln!(console, "driver output cut at {CAPACITY} bytes");
        }
        console.flush();
    }
}

impl Default for Log {
    fn default() -> Self {
        Self::new()
    }
}

impl Write for Log {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let room = CAPACITY - self.len;
        // Keep whole characters only, so the kept text stays valid UTF-8.
        let take = (0..=room.min(text.len()))
            .rev()
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(0);
        self.buf[self.len..self.len + take].copy_from_slice(&text.as_bytes()[..take]);
        self.len += take;
        if take < text.len() {
            self.cut = true;
        }
        Ok(())
    }
}

impl HalConsole for Log {
    fn flush(&mut self) {}
}

/// Register the driver, add the device, bind it, and start it. Returns the device handle.
///
/// # Errors
/// The framework's error for the first step that failed, including [`Error::OpFailed`] when the
/// driver's start operation reports failure.
pub fn attach<const D: usize, const N: usize>(
    registry: &mut Registry<D, N>,
    driver: Driver,
    id: nolx_df_core::DeviceId,
) -> Result<usize, Error> {
    registry.add_driver(driver)?;
    let dev = registry.add_device(id)?;
    registry.bind(dev)?;
    registry.start(dev)?;
    Ok(dev)
}
