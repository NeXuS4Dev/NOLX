//! AHCI (SATA host controller) rules, as far as they can be decided without touching a device.
//!
//! An AHCI controller exposes its registers through the memory BAR named by PCI class 01:06:01.
//! Which register is which, which bits mean "running", how a SATA disk is recognised from its
//! port signature, and what a command looks like in memory (the host-to-device FIS, the command
//! header, the physical region descriptor) are rules over bytes. They live here, host-tested. The
//! kernel maps the register window, allocates the memory the controller reads, and polls.
//!
//! Scope: one command slot in use, read commands through the 48-bit DMA command, and no
//! interrupts. Hot plug, port multipliers, and ATAPI are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

/// Host capabilities.
pub const HBA_CAP: u64 = 0x00;
/// Global host control: the AHCI enable bit lives here.
pub const HBA_GHC: u64 = 0x04;
/// Interrupt status, one bit per port.
pub const HBA_IS: u64 = 0x08;
/// Ports implemented: one bit per port.
pub const HBA_PI: u64 = 0x0C;
/// Offset of the first port's register block from the start of the HBA registers.
pub const PORT_BASE: u64 = 0x100;
/// Bytes per port register block.
pub const PORT_STRIDE: u64 = 0x80;
/// The most ports an HBA can implement.
pub const PORTS_MAX: u8 = 32;
/// Bytes of HBA register space that cover every port.
pub const HBA_REGION_BYTES: u64 = PORT_BASE + PORT_STRIDE * PORTS_MAX as u64;

/// Port command list base, low and high dword.
pub const PORT_CLB: u64 = 0x00;
/// Port command list base, high dword.
pub const PORT_CLBU: u64 = 0x04;
/// Port received-FIS base, low dword.
pub const PORT_FB: u64 = 0x08;
/// Port received-FIS base, high dword.
pub const PORT_FBU: u64 = 0x0C;
/// Port interrupt status.
pub const PORT_IS: u64 = 0x10;
/// Port interrupt enable: the per-port interrupt mask.
pub const PORT_IE: u64 = 0x14;
/// Port command and status.
pub const PORT_CMD: u64 = 0x18;
/// Port task file data: the device's status and error.
pub const PORT_TFD: u64 = 0x20;
/// Port signature: identifies the attached device.
pub const PORT_SIG: u64 = 0x24;
/// Port SATA status: link detection in the low nibble.
pub const PORT_SSTS: u64 = 0x28;
/// Port SATA error, cleared by writing ones.
pub const PORT_SERR: u64 = 0x30;
/// Port command issue: one bit per command slot.
pub const PORT_CI: u64 = 0x38;

/// Global host control: AHCI mode is enabled.
pub const GHC_AE: u32 = 1 << 31;
/// Global host control: the MSI enable bit. While it is set the HBA may signal with messages.
pub const GHC_MSI: u32 = 1 << 1;
/// Port command: start processing the command list.
pub const CMD_ST: u32 = 1 << 0;
/// Port command: the received-FIS area is enabled.
pub const CMD_FRE: u32 = 1 << 4;
/// Port command: the received-FIS engine is running.
pub const CMD_FR: u32 = 1 << 14;
/// Port command: the command-list engine is running.
pub const CMD_CR: u32 = 1 << 15;
/// Task file: the device reports an error.
pub const TFD_ERR: u32 = 1 << 0;
/// Task file: the device is requesting data.
pub const TFD_DRQ: u32 = 1 << 3;
/// Task file: the device is busy.
pub const TFD_BSY: u32 = 1 << 7;
/// Port interrupt status: a task file error stopped a command.
pub const IS_TFES: u32 = 1 << 30;
/// The port signature of a SATA disk.
pub const SIG_SATA: u32 = 0x0000_0101;
/// The SATA status detection value for a device present with an established link.
pub const DET_PRESENT: u32 = 3;

/// The FIS type of a host-to-device register FIS.
pub const FIS_H2D: u8 = 0x27;
/// The ATA command READ DMA EXT: 48-bit LBA, DMA.
pub const ATA_READ_DMA_EXT: u8 = 0x25;
/// The ATA command that writes sectors with a 48-bit address (WRITE DMA EXT).
pub const ATA_WRITE_DMA_EXT: u8 = 0x35;
/// The device register bit that selects LBA addressing.
pub const ATA_DEVICE_LBA: u8 = 0x40;
/// Bytes in a host-to-device register FIS.
pub const FIS_LEN: usize = 20;
/// Bytes in a command header.
pub const COMMAND_HEADER_LEN: usize = 32;
/// Bytes in one physical region descriptor.
pub const PRDT_ENTRY_LEN: usize = 16;
/// Offset of the physical region descriptor table in a command table, after the 128-byte
/// command FIS and ATAPI area.
pub const PRDT_OFFSET: usize = 0x80;
/// The most bytes one descriptor can describe: 4 MiB.
pub const PRDT_MAX_BYTES: u32 = 4 << 20;
/// Alignment the controller requires of the command list base.
pub const COMMAND_LIST_ALIGN: u64 = 1024;
/// Alignment the controller requires of the received-FIS base.
pub const RECEIVED_FIS_ALIGN: u64 = 256;
/// Alignment the controller requires of each command table base.
pub const COMMAND_TABLE_ALIGN: u64 = 128;

/// The register offset of `register` in port `port`, or `None` for a port the HBA cannot have.
#[must_use]
pub fn port_register(port: u8, register: u64) -> Option<u64> {
    if port < PORTS_MAX {
        Some(PORT_BASE + u64::from(port) * PORT_STRIDE + register)
    } else {
        None
    }
}

/// The number of command slots the HBA implements. The capability field is zero-based.
#[must_use]
pub fn command_slots(cap: u32) -> u8 {
    ((cap >> 8) & 0x1F) as u8 + 1
}

/// Whether the ports-implemented register says `port` exists.
#[must_use]
pub fn port_implemented(pi: u32, port: u8) -> bool {
    port < PORTS_MAX && pi & (1 << port) != 0
}

/// The device detection field of a SATA status register.
#[must_use]
pub fn det(ssts: u32) -> u32 {
    ssts & 0xF
}

/// Whether a port holds a SATA disk: the link is up and the signature is a SATA one.
#[must_use]
pub fn is_sata_disk(ssts: u32, signature: u32) -> bool {
    det(ssts) == DET_PRESENT && signature == SIG_SATA
}

/// Whether a task file reports an error.
#[must_use]
pub fn tfd_error(tfd: u32) -> bool {
    tfd & TFD_ERR != 0
}

/// Whether a task file says the device is still busy or waiting for data.
#[must_use]
pub fn tfd_busy(tfd: u32) -> bool {
    tfd & (TFD_BSY | TFD_DRQ) != 0
}

/// A host-to-device register FIS that reads `count` sectors starting at `lba` with READ DMA EXT.
/// `count` is the sector count; the device reads 65536 sectors for a count of zero, which this
/// crate never asks for.
#[must_use]
pub fn h2d_read_dma_ext(lba: u64, count: u16) -> [u8; FIS_LEN] {
    h2d_dma_ext(ATA_READ_DMA_EXT, lba, count)
}

/// A host-to-device register FIS that writes `count` sectors starting at `lba` with WRITE DMA EXT.
#[must_use]
pub fn h2d_write_dma_ext(lba: u64, count: u16) -> [u8; FIS_LEN] {
    h2d_dma_ext(ATA_WRITE_DMA_EXT, lba, count)
}

/// A host-to-device register FIS for a 48-bit DMA command: the address and count are split across
/// the bytes the specification names.
#[must_use]
fn h2d_dma_ext(command: u8, lba: u64, count: u16) -> [u8; FIS_LEN] {
    let mut fis = [0u8; FIS_LEN];
    fis[0] = FIS_H2D;
    // Bit 7 set: this FIS carries a command, not a control update.
    fis[1] = 0x80;
    fis[2] = command;
    fis[4] = lba as u8;
    fis[5] = (lba >> 8) as u8;
    fis[6] = (lba >> 16) as u8;
    fis[7] = ATA_DEVICE_LBA;
    fis[8] = (lba >> 24) as u8;
    fis[9] = (lba >> 32) as u8;
    fis[10] = (lba >> 40) as u8;
    fis[12] = count as u8;
    fis[13] = (count >> 8) as u8;
    fis
}

/// What a command header says about the command it points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandHeader {
    /// The FIS length in dwords, 2 to 16.
    pub fis_dwords: u8,
    /// Whether the data flows from host to device.
    pub write: bool,
    /// How many physical region descriptors the table holds.
    pub prdt_entries: u16,
    /// The physical address of the command table, which must be 128-byte aligned.
    pub table: u64,
}

/// Encode a command header. Returns `None` for a FIS length outside 2 to 16 dwords, or a table
/// address the controller cannot use.
#[must_use]
pub fn encode_command_header(header: &CommandHeader) -> Option<[u8; COMMAND_HEADER_LEN]> {
    if !(2..=16).contains(&header.fis_dwords) || !header.table.is_multiple_of(COMMAND_TABLE_ALIGN) {
        return None;
    }
    let mut dw0 = u32::from(header.fis_dwords) | (u32::from(header.prdt_entries) << 16);
    if header.write {
        dw0 |= 1 << 6;
    }
    let mut out = [0u8; COMMAND_HEADER_LEN];
    out[0..4].copy_from_slice(&dw0.to_le_bytes());
    // Bytes 4..8 are the transferred byte count, which the controller writes.
    out[8..12].copy_from_slice(&(header.table as u32).to_le_bytes());
    out[12..16].copy_from_slice(&((header.table >> 32) as u32).to_le_bytes());
    Some(out)
}

/// Encode one physical region descriptor covering `bytes` bytes at `address`. Returns `None` for
/// an empty or odd-length region, an address that is not word aligned, or a region over 4 MiB.
#[must_use]
pub fn prdt_entry(address: u64, bytes: u32) -> Option<[u8; PRDT_ENTRY_LEN]> {
    if bytes == 0
        || bytes > PRDT_MAX_BYTES
        || !bytes.is_multiple_of(2)
        || !address.is_multiple_of(2)
    {
        return None;
    }
    let mut out = [0u8; PRDT_ENTRY_LEN];
    out[0..4].copy_from_slice(&(address as u32).to_le_bytes());
    out[4..8].copy_from_slice(&((address >> 32) as u32).to_le_bytes());
    // The count is stored one less than the byte count, so an even count is stored odd.
    out[12..16].copy_from_slice(&(bytes - 1).to_le_bytes());
    Some(out)
}

#[cfg(test)]
mod tests;

/// Encode ATA IDENTIFY DEVICE as a host-to-device register FIS (512 bytes of data-in).
#[must_use]
pub fn h2d_identify() -> [u8; FIS_LEN] {
    let mut fis = h2d_dma_ext(0xec, 0, 0);
    fis[7] = 0;
    fis
}

/// Encode ATA FLUSH CACHE EXT, a command with no data PRDT.
#[must_use]
pub fn h2d_flush_cache() -> [u8; FIS_LEN] {
    let mut fis = h2d_dma_ext(0xea, 0, 0);
    fis[7] = 0;
    fis
}

/// Read the 48-bit ATA sector capacity from words 100 through 103 of IDENTIFY DEVICE.
/// Refuse short data, missing LBA48 capability, empty disks and out-of-range capacities.
#[must_use]
pub fn identify_sectors(bytes: &[u8]) -> Option<u64> {
    if bytes.len() < 512 || bytes[167] & 4 == 0 {
        return None;
    }
    let sectors = u64::from_le_bytes(bytes[200..208].try_into().ok()?);
    (sectors > 0 && sectors <= (1 << 48)).then_some(sectors)
}

/// The readback of a stopped AHCI controller: one port's registers and the HBA's global
/// control. These are decisions over register values, so they can be checked on a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopState {
    /// The port command register: the engine enable and running bits.
    pub command: u32,
    /// The port's command list base address, both dwords.
    pub list_base: u64,
    /// The port's received-FIS base address, both dwords.
    pub fis_base: u64,
    /// The port's interrupt enable register.
    pub interrupt_enable: u32,
    /// The HBA's global host control.
    pub global: u32,
}

impl StopState {
    /// Whether both of the port's engines are stopped: the command list engine and the
    /// received-FIS engine are neither enabled nor running.
    #[must_use]
    pub fn engines_stopped(&self) -> bool {
        self.command & (CMD_ST | CMD_FRE | CMD_CR | CMD_FR) == 0
    }

    /// Whether the port names no command list and no received-FIS area, so it cannot reach
    /// a page the driver is about to release.
    #[must_use]
    pub fn unmapped(&self) -> bool {
        self.list_base == 0 && self.fis_base == 0
    }

    /// Whether the controller no longer signals: the port's interrupts are masked and the
    /// HBA's MSI enable is clear.
    #[must_use]
    pub fn signalling_disabled(&self) -> bool {
        self.interrupt_enable == 0 && self.global & GHC_MSI == 0
    }

    /// Whether the controller is quiesced: the engines are stopped, the port names no
    /// buffer, and the controller no longer signals.
    #[must_use]
    pub fn quiesced(&self) -> bool {
        self.engines_stopped() && self.unmapped() && self.signalling_disabled()
    }
}

/// The final readback filter, not a hardware operation: the port must be quiesced and the
/// function must be excluded from bus mastering, legacy interrupts and message delivery
/// before an exposed page may be freed.
#[must_use]
pub fn stop_proven(state: StopState, command: u16, messages_disabled: bool) -> bool {
    state.quiesced() && nolx_pci_core::exclusion::excluded(command, messages_disabled)
}

#[cfg(test)]
mod cache_tests {
    use super::*;
    #[test]
    fn identify_and_flush_have_no_lba_or_count_fields() {
        for (fis, opcode) in [(h2d_identify(), 0xec), (h2d_flush_cache(), 0xea)] {
            assert_eq!(fis[0], 0x27);
            assert_eq!(fis[1], 0x80);
            assert_eq!(fis[2], opcode);
            assert!(fis[3..].iter().all(|&b| b == 0));
        }
    }
    #[test]
    fn identify_capacity_is_bounded_and_requires_lba48() {
        let mut bytes = [0; 512];
        assert_eq!(identify_sectors(&bytes), None);
        bytes[167] = 4;
        bytes[200..208].copy_from_slice(&2048u64.to_le_bytes());
        assert_eq!(identify_sectors(&bytes), Some(2048));
        assert_eq!(identify_sectors(&bytes[..511]), None);
        bytes[200..208].copy_from_slice(&u64::MAX.to_le_bytes());
        assert_eq!(identify_sectors(&bytes), None);
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    fn stopped() -> StopState {
        StopState {
            command: 0,
            list_base: 0,
            fis_base: 0,
            interrupt_enable: 0,
            global: 0,
        }
    }

    #[test]
    fn a_stopped_port_has_no_engine_enabled_or_running() {
        let mut state = stopped();
        assert!(state.engines_stopped());
        for bit in [CMD_ST, CMD_FRE, CMD_CR, CMD_FR] {
            state.command = bit;
            assert!(
                !state.engines_stopped(),
                "engine bit {bit:#x} still running"
            );
        }
    }

    #[test]
    fn a_stopped_port_names_no_buffer() {
        let mut state = stopped();
        assert!(state.unmapped());
        state.list_base = 0x1000;
        assert!(!state.unmapped());
        state.list_base = 0;
        state.fis_base = 0x2000;
        assert!(!state.unmapped());
    }

    #[test]
    fn a_quiet_controller_masks_its_port_and_clears_the_msi_enable() {
        let mut state = stopped();
        assert!(state.signalling_disabled());
        state.interrupt_enable = 1;
        assert!(!state.signalling_disabled());
        state.interrupt_enable = 0;
        state.global = GHC_MSI;
        assert!(!state.signalling_disabled());
    }

    #[test]
    fn the_stop_proof_needs_the_port_and_the_pci_exclusion_together() {
        let state = stopped();
        assert!(stop_proven(state, 1 << 10, true));
        assert!(!stop_proven(state, 1 << 10, false));
        assert!(!stop_proven(state, 1 << 10 | 4, true));
        assert!(!stop_proven(state, 0, true));
        let mut running = state;
        running.command = CMD_ST;
        assert!(!stop_proven(running, 1 << 10, true));
        let mut mapped = state;
        mapped.fis_base = 0x4000;
        assert!(!stop_proven(mapped, 1 << 10, true));
        let mut signalling = state;
        signalling.global = GHC_MSI;
        assert!(!stop_proven(signalling, 1 << 10, true));
    }
}
