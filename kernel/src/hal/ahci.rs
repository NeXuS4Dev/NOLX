//! The AHCI driver's probe: find the controller, find the first port with a SATA disk, start
//! that port, read sector 0 through command slot 0, then write a pattern to sectors 1600 to 1603,
//! read them back, and compare CRC-32 checksums. Everything is polled.
//!
//! The rules -- the register offsets, the port state bits, the FIS and command-header layouts,
//! the physical region descriptors -- live in `nolx-ahci-core`, where they are host-tested. This
//! module supplies register access through an [`mmio`] window, the command memory from [`Dma`],
//! and bounded polling. The probe is synchronous. A port that never answers is a timeout, not a
//! hang.
//!
//! Command memory is one DMA page: the command list at 0, the received FIS at 1024, and the
//! command table at 2048, with its FIS at 0 and its descriptor at 0x80. The data buffer is a
//! second page, from offset 0. CI runs QEMU with a copy of the disk image attached to an AHCI
//! port, so the bytes read come from the disk, and CI checks the written sectors in that copy.

use nolx_ahci_core::{self as ahci, CommandHeader};
use super::blockdev;
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{Bar, Bdf};
use nolx_virtio_core::crc32;

use super::dma::Dma;
use super::pci::LegacyPorts;
use super::virtio_blk::fill_pattern;
use crate::mm::mmio::{self, Region};
use core::sync::atomic::{fence, Ordering};

/// The base address register that holds the HBA's registers. AHCI puts them in BAR 5.
const ABAR_SLOT: usize = 5;
/// The AHCI class code: mass storage, SATA, AHCI interface.
const AHCI_CLASS: u8 = 0x01;
const AHCI_SUBCLASS: u8 = 0x06;
const AHCI_PROG_IF: u8 = 0x01;
/// How many times a bounded wait looks before it gives up.
const POLL_LIMIT: u32 = 1_000_000;
/// Where each piece of command memory sits in the command page.
const COMMAND_LIST_AT: u64 = 0;
const RECEIVED_FIS_AT: u64 = 1024;
const COMMAND_TABLE_AT: u64 = 2048;
/// One sector, and the bytes of it that the probe checks against the disk image.
const SECTOR_BYTES: u32 = 512;
const SECTOR_PATTERN: &[u8] = b"NOLX-BLK-SECTOR0";
/// The round trip: its first sector, how many sectors, and their size in bytes. The sectors sit in
/// the gap after the last partition the CI image defines, as the virtio round trip's do.
const ROUND_TRIP_SECTOR: u64 = 1600;
const ROUND_TRIP_SECTORS: u16 = 4;
const ROUND_TRIP_BYTES: usize = ROUND_TRIP_SECTORS as usize * SECTOR_BYTES as usize;

/// Why the probe stopped.
#[derive(Debug)]
pub enum Failure {
    /// The block queue refused the request, or did not hand it back.
    Queue(QueueError),
    /// No AHCI controller is on the PCI bus.
    NoController,
    /// BAR 5 is not a memory BAR, so the HBA registers cannot be reached.
    BarNotMemory,
    /// No implemented port holds a SATA disk.
    NoDisk,
    /// The AHCI rules refused a header, FIS, or descriptor.
    Encoding,
    /// No frame was available for the command memory.
    Memory,
    /// The MMIO window refused a mapping.
    Window(mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// A bounded wait ran out, with the step named.
    Timeout(&'static str),
    /// The port reported a task-file error.
    Device {
        /// The task-file register as the port last reported it.
        tfd: u32,
    },
    /// The data read back is not the pattern the disk image holds.
    Pattern,
    /// The round trip read back different bytes from the ones written.
    RoundTrip,
}

impl From<MmioError> for Failure {
    fn from(error: MmioError) -> Self {
        Self::Access(error)
    }
}

impl From<mmio::Failure> for Failure {
    fn from(error: mmio::Failure) -> Self {
        Self::Window(error)
    }
}

/// Probe for an AHCI controller, start its first SATA port, and read sector 0. Failure is
/// reported on the console and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    if let Err(failure) = bring_up(console) {
        let _ = writeln!(console, "ahci: not brought up: {failure:?}");
    }
}

fn bring_up(console: &mut dyn HalConsole) -> Result<(), Failure> {
    let bdf = find_controller().ok_or(Failure::NoController)?;
    let bars = nolx_pci_core::read_bars(&mut LegacyPorts, bdf);
    let abar = match bars.slots[ABAR_SLOT] {
        Bar::Memory32 { base, .. } => u64::from(base),
        Bar::Memory64 { base, .. } => base,
        _ => return Err(Failure::BarNotMemory),
    };
    let hba = mmio::map(abar, ahci::HBA_REGION_BYTES)?;
    let ports = hba.read32(ahci::HBA_PI)?;
    // Put the controller in AHCI mode. The port registers are only meaningful in that mode.
    let ghc = hba.read32(ahci::HBA_GHC)?;
    hba.write32(ahci::HBA_GHC, ghc | ahci::GHC_AE)?;

    let number = find_sata_disk(&hba, ports)?;
    let _ = writeln!(
        console,
        "ahci: {:02x}:{:02x}.{} abar {abar:#x}, ports {ports:#x}, port {number} is a SATA disk",
        bdf.bus, bdf.device, bdf.function,
    );

    let page = Dma::new().ok_or(Failure::Memory)?;
    let data = Dma::new().ok_or(Failure::Memory)?;
    let port = Port { hba: &hba, number };
    port.stop()?;
    port.write64(ahci::PORT_CLB, page.address(COMMAND_LIST_AT))?;
    port.write64(ahci::PORT_FB, page.address(RECEIVED_FIS_AT))?;
    port.write(ahci::PORT_SERR, u32::MAX)?;
    port.write(ahci::PORT_IS, u32::MAX)?;
    port.start()?;

    // Sector 0 first: its first bytes are the pattern CI wrote into the image.
    data.write(0, &[0u8; SECTOR_BYTES as usize]);
    transfer(
        &port,
        &page,
        &data,
        Command {
            write: false,
            lba: 0,
            sectors: 1,
        },
    )?;
    let mut sector = [0u8; SECTOR_BYTES as usize];
    data.read(0, &mut sector);
    if sector[..SECTOR_PATTERN.len()] != *SECTOR_PATTERN {
        return Err(Failure::Pattern);
    }
    let _ = writeln!(
        console,
        "ahci: port {number} sector 0 read, status ok, first {} bytes match the disk image",
        SECTOR_PATTERN.len(),
    );

    // The round trip: write a pattern, clear the buffer, read the sectors back, and compare.
    let mut written = [0u8; ROUND_TRIP_BYTES];
    fill_pattern(&mut written);
    data.write(0, &written);
    transfer(
        &port,
        &page,
        &data,
        Command {
            write: true,
            lba: ROUND_TRIP_SECTOR,
            sectors: ROUND_TRIP_SECTORS,
        },
    )?;
    data.write(0, &[0u8; ROUND_TRIP_BYTES]);
    transfer(
        &port,
        &page,
        &data,
        Command {
            write: false,
            lba: ROUND_TRIP_SECTOR,
            sectors: ROUND_TRIP_SECTORS,
        },
    )?;
    let mut back = [0u8; ROUND_TRIP_BYTES];
    data.read(0, &mut back);
    if back != written {
        return Err(Failure::RoundTrip);
    }
    let checksum = crc32(&back);
    let _ = writeln!(
        console,
        "ahci: wrote {ROUND_TRIP_SECTORS} sectors at {ROUND_TRIP_SECTOR}, read back, \
         crc32 {checksum:#010x} matches the written data"
    );
    Ok(())
}

/// The first AHCI controller on the bus, if there is one.
fn find_controller() -> Option<Bdf> {
    let inventory = nolx_pci_core::enumerate(&mut LegacyPorts);
    inventory
        .as_slice()
        .iter()
        .find(|info| {
            info.class == AHCI_CLASS
                && info.subclass == AHCI_SUBCLASS
                && info.prog_if == AHCI_PROG_IF
        })
        .map(|info| info.bdf)
}

/// The first implemented port whose link is up and whose device is a SATA disk.
fn find_sata_disk(hba: &Region, ports: u32) -> Result<u8, Failure> {
    for number in 0..ahci::PORTS_MAX {
        if !ahci::port_implemented(ports, number) {
            continue;
        }
        let port = Port { hba, number };
        let ssts = port.read(ahci::PORT_SSTS)?;
        let signature = port.read(ahci::PORT_SIG)?;
        if ahci::is_sata_disk(ssts, signature) {
            return Ok(number);
        }
    }
    Err(Failure::NoDisk)
}

/// One command in slot 0: a read or a write of `sectors` sectors starting at `lba`.
struct Command {
    write: bool,
    lba: u64,
    sectors: u16,
}

/// Issue `command` in slot 0 with its data at offset 0 of the data page, wait for the port, and
/// check its task file. For a write the caller has already put the data in the page.
fn transfer(port: &Port, page: &Dma, data: &Dma, command: Command) -> Result<(), Failure> {
    let request = Request {
        direction: if command.write {
            Direction::Write
        } else {
            Direction::Read
        },
        lba: command.lba,
        sectors: u32::from(command.sectors),
    };
    blockdev::run(blockdev::AHCI, request, || {
        issue(port, page, data, command)
    })
}

/// Issue the command in the port's slot 0 and wait for it. Called through [`transfer`].
fn issue(port: &Port, page: &Dma, data: &Dma, command: Command) -> Result<(), Failure> {
    let bytes = u32::from(command.sectors) * SECTOR_BYTES;
    let header = ahci::encode_command_header(&CommandHeader {
        fis_dwords: 5,
        write: command.write,
        prdt_entries: 1,
        table: page.address(COMMAND_TABLE_AT),
    })
    .ok_or(Failure::Encoding)?;
    let prdt = ahci::prdt_entry(data.address(0), bytes).ok_or(Failure::Encoding)?;
    let fis = if command.write {
        ahci::h2d_write_dma_ext(command.lba, command.sectors)
    } else {
        ahci::h2d_read_dma_ext(command.lba, command.sectors)
    };
    page.write(COMMAND_LIST_AT, &header);
    page.write(COMMAND_TABLE_AT, &fis);
    page.write(COMMAND_TABLE_AT + ahci::PRDT_OFFSET as u64, &prdt);
    // The device reads these bytes over the bus, so they must be written before the port is told.
    fence(Ordering::SeqCst);

    port.write(ahci::PORT_CI, 1)?;
    poll(
        || {
            let tfd = port.read(ahci::PORT_TFD)?;
            if ahci::tfd_error(tfd) || (port.read(ahci::PORT_IS)? & ahci::IS_TFES) != 0 {
                return Err(Failure::Device { tfd });
            }
            Ok((port.read(ahci::PORT_CI)? & 1) == 0)
        },
        "command issue",
    )?;
    fence(Ordering::SeqCst);
    Ok(())
}

/// Poll `ready` until it returns true, at most [`POLL_LIMIT`] times.
fn poll(
    mut ready: impl FnMut() -> Result<bool, Failure>,
    step: &'static str,
) -> Result<(), Failure> {
    for _ in 0..POLL_LIMIT {
        if ready()? {
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout(step))
}

/// One implemented port's registers, reached through the HBA window.
struct Port<'a> {
    hba: &'a Region,
    number: u8,
}

impl Port<'_> {
    fn read(&self, register: u64) -> Result<u32, Failure> {
        let offset = ahci::port_register(self.number, register).ok_or(Failure::NoDisk)?;
        self.hba.read32(offset).map_err(Failure::from)
    }

    fn write(&self, register: u64, value: u32) -> Result<(), Failure> {
        let offset = ahci::port_register(self.number, register).ok_or(Failure::NoDisk)?;
        self.hba.write32(offset, value).map_err(Failure::from)
    }

    /// A 64-bit register, written as its low word and then its high word, which sits 4 bytes
    /// above.
    fn write64(&self, low: u64, value: u64) -> Result<(), Failure> {
        self.write(low, value as u32)?;
        self.write(low + 4, (value >> 32) as u32)
    }

    /// Stop the port: clear ST and wait for the command list to stop, then clear FRE and wait
    /// for the received FIS area to stop. The port's FIS area and command list may only change
    /// while both are stopped.
    fn stop(&self) -> Result<(), Failure> {
        self.write(ahci::PORT_CMD, self.read(ahci::PORT_CMD)? & !ahci::CMD_ST)?;
        poll(
            || Ok((self.read(ahci::PORT_CMD)? & ahci::CMD_CR) == 0),
            "command list stop",
        )?;
        self.write(ahci::PORT_CMD, self.read(ahci::PORT_CMD)? & !ahci::CMD_FRE)?;
        poll(
            || Ok((self.read(ahci::PORT_CMD)? & ahci::CMD_FR) == 0),
            "received FIS stop",
        )
    }

    /// Start the port: wait for the list engine to be idle, then set FRE and ST.
    fn start(&self) -> Result<(), Failure> {
        poll(
            || Ok((self.read(ahci::PORT_CMD)? & ahci::CMD_CR) == 0),
            "command list idle",
        )?;
        self.write(ahci::PORT_CMD, self.read(ahci::PORT_CMD)? | ahci::CMD_FRE)?;
        self.write(ahci::PORT_CMD, self.read(ahci::PORT_CMD)? | ahci::CMD_ST)
    }
}

impl From<QueueError> for Failure {
    fn from(error: QueueError) -> Self {
        Self::Queue(error)
    }
}
