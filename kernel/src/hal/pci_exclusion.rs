//! Checked PCI DMA and message exclusion, shared by the storage and network transports.
//!
//! A driver that is about to release a page a device could reach must prove, by reading
//! the device back, that the device can neither master the bus nor raise an interrupt.
//! The rules live in `nolx-pci-core`, where they are host-tested; this module performs
//! the writes and the readbacks against the real configuration space.
//!
//! Three facts are needed, and each is checked separately:
//!
//! * the command register's bus-master bit is clear and its INTx-disable bit is set, so
//!   the function reaches no memory and raises no legacy interrupt;
//! * the message capability's enable bit is clear -- and for MSI-X the function mask is
//!   set as well, so a device that still writes its table delivers nothing;
//! * the readback of those registers says so, not the write that was attempted.

use super::pci::ConfigAccess;
use nolx_pci_core::{msi, Bdf, ConfigSpace};

/// Turn the function's memory decoding and bus mastering on or off, keeping every other
/// command bit as it was, and block its legacy interrupt line. The readback decides: a
/// function that does not keep the bits it was given is not usable.
pub(super) fn pci_access(bdf: Bdf, dma: bool) -> bool {
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    let wanted = (command | 2 | (1 << 10)) & !4 | if dma { 4 } else { 0 };
    ConfigAccess.write_u32(bdf, 4, wanted);
    if ConfigAccess.read_u32(bdf, 4) & (6 | (1 << 10)) != wanted & (6 | (1 << 10)) {
        return false;
    }
    true
}

/// Stop the function from delivering messages: clear the MSI enable bit, and for MSI-X
/// clear the enable bit and set the function mask, so no vector of the function can
/// deliver. Every capability is read back before the function is called excluded.
pub(super) fn disable_messages(bdf: Bdf) -> bool {
    let caps = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    if caps.truncated {
        return false;
    }
    for cap in caps.as_slice() {
        let (clear, set) = match cap.id {
            msi::CAP_MSI => (1u32 << 16, 0),
            msi::CAP_MSI_X => (1u32 << 31, 1u32 << 30),
            _ => continue,
        };
        let old = ConfigAccess.read_u32(bdf, cap.offset);
        ConfigAccess.write_u32(bdf, cap.offset, (old & !clear) | set);
        if ConfigAccess.read_u32(bdf, cap.offset) & (clear | set) != set {
            return false;
        }
    }
    true
}

/// Whether every message capability of the function reads back as disabled: MSI with its
/// enable bit clear, MSI-X with its enable bit clear and its function mask set.
pub(super) fn messages_disabled(bdf: Bdf) -> bool {
    let caps = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    !caps.truncated
        && caps.as_slice().iter().all(|cap| {
            let control = ConfigAccess.read_u32(bdf, cap.offset) >> 16;
            match cap.id {
                msi::CAP_MSI => msi::msi_disabled(control as u16),
                msi::CAP_MSI_X => msi::msix_disabled(control as u16),
                _ => true,
            }
        })
}
