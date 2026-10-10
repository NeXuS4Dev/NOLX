//! The backend-independent kernel-console contract.
//!
//! The concrete kernel driver may write to a serial port, a framebuffer, or both. Code
//! above the HAL depends only on this object-safe formatting and flush interface.

use core::fmt::Write;

/// A kernel console that accepts formatted text and can flush pending output.
///
/// Implementations must be object-safe so boot and diagnostic code can use one backend
/// without knowing its concrete device type. The serial/VGA bring-up console is synchronous,
/// so its `flush` implementation is a no-op; buffered consoles should drain their queue.
pub trait HalConsole: Write {
    /// Make all previously written output visible to the device or user.
    fn flush(&mut self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::fmt;

    #[derive(Default)]
    struct TestConsole {
        output: String,
        flushes: usize,
    }

    impl Write for TestConsole {
        fn write_str(&mut self, text: &str) -> fmt::Result {
            self.output.push_str(text);
            Ok(())
        }
    }

    impl HalConsole for TestConsole {
        fn flush(&mut self) {
            self.flushes += 1;
        }
    }

    fn write_diagnostic(console: &mut dyn HalConsole) {
        writeln!(console, "HAL ready: {}", 42).expect("test console write succeeds");
        console.flush();
    }

    #[test]
    fn hal_console_is_object_safe_for_formatted_output_and_flush() {
        let mut console = TestConsole::default();
        write_diagnostic(&mut console);

        assert_eq!(console.output, "HAL ready: 42\n");
        assert_eq!(console.flushes, 1);
    }
}
