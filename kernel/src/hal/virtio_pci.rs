//! Checked PCI DMA and message exclusion shared by modern virtio transports.
use super::pci::ConfigAccess;
use nolx_pci_core::{Bdf, ConfigSpace};

pub(super) fn pci_access(bdf: Bdf, dma: bool) -> bool {
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    let wanted = (command | 2 | (1 << 10)) & !4 | if dma { 4 } else { 0 };
    ConfigAccess.write_u32(bdf, 4, wanted);
    if ConfigAccess.read_u32(bdf, 4) & (6 | (1 << 10)) != wanted & (6 | (1 << 10)) {
        return false;
    }
    true
}
pub(super) fn disable_messages(bdf: Bdf) -> bool {
    use nolx_pci_core::msi;
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
pub(super) fn messages_disabled(bdf: Bdf) -> bool {
    use nolx_pci_core::msi;
    let caps = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    !caps.truncated
        && caps.as_slice().iter().all(|cap| {
            let control = ConfigAccess.read_u32(bdf, cap.offset) >> 16;
            match cap.id {
                msi::CAP_MSI => control & 1 == 0,
                msi::CAP_MSI_X => control & 0xc000 == 0x4000,
                _ => true,
            }
        })
}
