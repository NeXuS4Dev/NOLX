//! Checked PCI exclusion rules: the command-register bits that stop a function from
//! mastering the bus, and the readback filter that says a device may no longer reach
//! memory or raise an interrupt.
//!
//! A driver that is about to release a page a device could reach must prove three
//! separate things: the function may no longer master the bus, its legacy interrupt
//! line is blocked, and its message capability delivers nothing. These are decisions
//! over register values, so they live here and are host-tested; the kernel performs the
//! writes and the readbacks.

/// PCI command register: memory space enable.
pub const COMMAND_MEMORY_SPACE: u16 = 1 << 1;
/// PCI command register: bus master enable. A function may reach memory only while this
/// bit is set.
pub const COMMAND_BUS_MASTER: u16 = 1 << 2;
/// PCI command register: disable legacy INTx interrupts.
pub const COMMAND_INTX_DISABLE: u16 = 1 << 10;

/// Whether the command register proves the function can no longer master the bus.
#[must_use]
pub fn dma_disabled(command: u16) -> bool {
    command & COMMAND_BUS_MASTER == 0
}

/// Whether the command register proves the function's legacy interrupt line is blocked.
#[must_use]
pub fn intx_blocked(command: u16) -> bool {
    command & COMMAND_INTX_DISABLE != 0
}

/// Whether the command register and the message state prove the function is excluded:
/// it can neither master the bus nor raise an interrupt.
#[must_use]
pub fn excluded(command: u16, messages_disabled: bool) -> bool {
    dma_disabled(command) && intx_blocked(command) && messages_disabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bus_mastering_must_be_off_before_a_page_may_be_released() {
        assert!(dma_disabled(0));
        assert!(dma_disabled(COMMAND_MEMORY_SPACE));
        assert!(!dma_disabled(COMMAND_BUS_MASTER));
        assert!(!dma_disabled(COMMAND_BUS_MASTER | COMMAND_INTX_DISABLE));
    }

    #[test]
    fn the_legacy_interrupt_line_must_be_blocked() {
        assert!(intx_blocked(COMMAND_INTX_DISABLE));
        assert!(!intx_blocked(0));
        assert!(!intx_blocked(COMMAND_BUS_MASTER));
    }

    #[test]
    fn every_missing_part_of_the_exclusion_refuses() {
        for command in 0..=u16::MAX {
            for messages in [false, true] {
                assert_eq!(
                    excluded(command, messages),
                    command & COMMAND_BUS_MASTER == 0
                        && command & COMMAND_INTX_DISABLE != 0
                        && messages
                );
            }
        }
    }

    #[test]
    fn unrelated_command_bits_do_not_authorize_dma_or_interrupt_delivery() {
        assert!(excluded(COMMAND_MEMORY_SPACE | COMMAND_INTX_DISABLE, true));
        assert!(!excluded(
            COMMAND_MEMORY_SPACE | COMMAND_INTX_DISABLE,
            false
        ));
        assert!(!excluded(COMMAND_MEMORY_SPACE, true));
    }
}
