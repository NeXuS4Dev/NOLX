//! The AHCI driver's probe: find the controller, find the first port with a SATA disk, start
//! that port, read sector 0 through command slot 0, then write a pattern to sectors 1600 to 1603,
//! read them back, and compare CRC-32 checksums. Commands verify MSI delivery when a local
//! APIC is available, with a polled completion fallback otherwise.
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

use super::blockdev;
use super::driver_start::{self, Log};
use nolx_ahci_core::{self as ahci, CommandHeader};
use nolx_block_core::devices::QueueError;
use nolx_block_core::queue::{Direction, Request};
use nolx_df_core::{DeviceId, Dispatch, Driver, Match};
use nolx_hal_core::console::HalConsole;
use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{Bar, Bdf, ConfigSpace};
use nolx_virtio_core::crc32;

use super::dma::Dma;
use super::pci::ConfigAccess;
use super::virtio_blk::fill_pattern;
use crate::mm::mmio::{self, Region};
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

/// Dedicated AHCI MSI completion vector.
pub const COMPLETION_VECTOR: u8 = 0x52;
static COMPLETIONS: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static MSI_ACTIVE: [AtomicBool; driver_start::DEVICES] =
    [const { AtomicBool::new(false) }; driver_start::DEVICES];
static INTERRUPT_REGISTERS: [IrqMutex<Option<(Region, u8)>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];

/// Keep the compatibility vector for the first HBA; reserve one for each further controller.
fn completion_vector(dev: usize) -> u8 {
    if blockdev::device(dev) == Some(blockdev::AHCI) {
        COMPLETION_VECTOR
    } else {
        0x90 + dev as u8
    }
}
/// Whether the vector belongs to this driver's bounded per-controller reservation.
pub fn handles_vector(vector: u8) -> bool {
    vector == COMPLETION_VECTOR || (0x90..0xa0).contains(&vector)
}
/// Attribute a vector to exactly one controller before acknowledging its completion.
pub fn on_completion_interrupt(vector: u8) {
    let dev = if vector == COMPLETION_VECTOR {
        blockdev::primary_handle(blockdev::AHCI)
    } else {
        Some(usize::from(vector - 0x90))
    };
    let Some(dev) = dev.filter(|&dev| dev < driver_start::DEVICES) else {
        return;
    };
    let registers = INTERRUPT_REGISTERS[dev].lock();
    if let Some((hba, number)) = *registers {
        let status_offset = ahci::port_register(number, ahci::PORT_IS).unwrap_or(0);
        let status = hba.read32(status_offset).unwrap_or(0);
        let global = hba.read32(ahci::HBA_IS).unwrap_or(0);
        let _ = hba.write32(status_offset, status);
        let _ = hba.write32(ahci::HBA_IS, global);
        if status & 1 != 0 {
            COMPLETIONS[dev].fetch_add(1, Ordering::Release);
        }
    }
}

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
    /// Filesystem verification on this controller failed.
    Filesystem(super::storage_volume::Failure),
    /// MSI programming failed.
    Msi(super::msix::Error),
    /// A command completed without its interrupt being delivered.
    Interrupt,
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
/// What the framework's start operation leaves for the probe: the bring-up's output and result.
struct State {
    log: Log,
    outcome: Option<Result<(), Failure>>,
    disk: Option<Disk>,
    bdf: Option<Bdf>,
}

static STATE: [IrqMutex<State>; driver_start::DEVICES] = [const {
    IrqMutex::new(State {
        log: Log::new(),
        outcome: None,
        disk: None,
        bdf: None,
    })
}; driver_start::DEVICES];

/// The match rule: the controller's class, subclass, and programming interface.
static MATCHES: &[Match] = &[Match::by_class(
    AHCI_CLASS,
    AHCI_SUBCLASS,
    Some(AHCI_PROG_IF),
)];

/// The driver's dispatch table. Probe accepts any device the match rule selects.
const DISPATCH: Dispatch = Dispatch {
    probe: accept,
    start: start_device,
    stop: stop_device,
};

/// Probe for the controller, register the driver with the framework, and bind and start it. The
/// bring-up's output is replayed on the console. Failure is reported and does not stop boot.
pub fn probe(console: &mut dyn HalConsole) {
    let inventory = nolx_pci_core::enumerate(&mut ConfigAccess);
    let mut found = false;
    for info in inventory.as_slice().iter().filter(|info| {
        info.class == AHCI_CLASS && info.subclass == AHCI_SUBCLASS && info.prog_if == AHCI_PROG_IF
    }) {
        found = true;
        if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
            let _ = writeln!(
                console,
                "ahci: PCI {:02x}:{:02x}.{} already registered, duplicate attach refused",
                info.bdf.bus, info.bdf.device, info.bdf.function
            );
            continue;
        }
        let id = DeviceId {
            vendor: info.vendor_id,
            device: info.device_id,
            class: info.class,
            subclass: info.subclass,
            prog_if: info.prog_if,
        };
        let driver = Driver {
            name: "ahci",
            matches: MATCHES,
            ops: DISPATCH,
        };
        let attached = driver_start::attach_prepared(driver, id, |dev| {
            // Recheck under framework exclusion: the outer enumeration check is only a fast path.
            if STATE.iter().any(|state| state.lock().bdf == Some(info.bdf)) {
                return Err(nolx_df_core::Error::Busy);
            }
            STATE[dev].lock().bdf = Some(info.bdf);
            blockdev::bind_storage(dev, blockdev::AHCI).map_err(|_| nolx_df_core::Error::Full)?;
            Ok(())
        });
        match attached {
            Ok(dev) => {
                let state = STATE[dev].lock();
                state.log.replay(console);
                let _ = writeln!(console, "ahci: bound and started through nolx-df");
                let _ = writeln!(
                    console,
                    "ahci: instance {dev}, block {}, vector {:#x}, PCI {:02x}:{:02x}.{}",
                    blockdev::device(dev).unwrap(),
                    completion_vector(dev),
                    info.bdf.bus,
                    info.bdf.device,
                    info.bdf.function
                );
            }
            Err(error) => {
                let _ = writeln!(console, "ahci: framework refused the device: {error:?}");
                // Rejected starts retain diagnostic state and pinned resources. Identify by BDF.
                for state in &STATE {
                    let state = state.lock();
                    if state.bdf == Some(info.bdf) {
                        state.log.replay(console);
                        if let Some(Err(failure)) = &state.outcome {
                            let _ = writeln!(console, "ahci: not brought up: {failure:?}");
                        }
                    }
                }
            }
        }
    }
    if !found {
        let _ = writeln!(console, "ahci: not brought up: {:?}", Failure::NoController);
    }
}

fn accept(_: &DeviceId) -> bool {
    true
}

/// Start the selected controller; BDF, diagnostics and transport ownership stay per handle.
fn start_device(dev: usize) -> bool {
    let mut log = Log::new();
    let result = bring_up(dev, &mut log);
    let started = result.is_ok();
    let mut state = STATE[dev].lock();
    state.log = log;
    state.outcome = Some(result.map(|disk| {
        state.disk = Some(disk);
    }));
    started
}

/// Flush and quiesce hardware before returning owned DMA pages. No IRQ lock spans the wait.
fn stop_device(dev: usize) -> bool {
    let Some(mut disk) = STATE[dev].lock().disk.take() else {
        return false;
    };
    if disk.quiesce().is_err() {
        STATE[dev].lock().disk = Some(disk);
        return false;
    }
    disk.release();
    true
}

pub(crate) fn with_disk<T>(
    dev: usize,
    op: impl FnOnce(&mut dyn nolxfs::sector::SectorDevice) -> Result<T, nolxfs::DevError>,
) -> Result<T, nolxfs::DevError> {
    let mut disk = STATE[dev]
        .lock()
        .disk
        .take()
        .ok_or(nolxfs::DevError::Io(19))?;
    let result = op(&mut disk);
    STATE[dev].lock().disk = Some(disk);
    result
}

fn bring_up(dev: usize, console: &mut dyn HalConsole) -> Result<Disk, Failure> {
    let bdf = STATE[dev].lock().bdf.ok_or(Failure::NoController)?;
    let vector = completion_vector(dev);
    let initial_completions = COMPLETIONS[dev].load(Ordering::Acquire);
    let command = ConfigAccess.read_u32(bdf, 4) & 0xffff;
    ConfigAccess.write_u32(bdf, 4, command | 6);
    let bars = nolx_pci_core::read_bars(&mut ConfigAccess, bdf);
    let abar = match bars.slots[ABAR_SLOT] {
        Bar::Memory32 { base, .. } => u64::from(base),
        Bar::Memory64 { base, .. } => base,
        _ => return Err(Failure::BarNotMemory),
    };
    let hba = mmio::map_owned(dev, abar, ahci::HBA_REGION_BYTES)?;
    let ports = hba.read32(ahci::HBA_PI)?;
    // Put the controller in AHCI mode. The port registers are only meaningful in that mode.
    let ghc = hba.read32(ahci::HBA_GHC)?;
    hba.write32(ahci::HBA_GHC, ghc | ahci::GHC_AE)?;

    let number = find_sata_disk(dev, &hba, ports)?;
    let _ = writeln!(
        console,
        "ahci: {:02x}:{:02x}.{} abar {abar:#x}, ports {ports:#x}, port {number} is a SATA disk",
        bdf.bus, bdf.device, bdf.function,
    );

    let page = Dma::new().ok_or(Failure::Memory)?;
    let data = Dma::new().ok_or(Failure::Memory)?;
    let port = Port {
        owner: dev,
        hba: &hba,
        number,
    };
    port.stop()?;
    port.write64(ahci::PORT_CLB, page.address(COMMAND_LIST_AT))?;
    port.write64(ahci::PORT_FB, page.address(RECEIVED_FIS_AT))?;
    port.write(ahci::PORT_SERR, u32::MAX)?;
    port.write(ahci::PORT_IS, u32::MAX)?;
    port.start()?;
    if super::apic::is_enabled() {
        *INTERRUPT_REGISTERS[dev].lock() = Some((hba, number));
        super::msix::enable_msi(bdf, vector).map_err(Failure::Msi)?;
        // Enable only D2H register-FIS interrupts, one per completed DMA EXT command.
        port.write(0x14, 1)?;
        MSI_ACTIVE[dev].store(true, Ordering::Release);
        hba.write32(ahci::HBA_GHC, hba.read32(ahci::HBA_GHC)? | 2)?;
    } else {
        // Preserve the existing polled disk path on machines without a local APIC.
        port.write(0x14, 0)?;
        MSI_ACTIVE[dev].store(false, Ordering::Release);
        hba.write32(ahci::HBA_GHC, hba.read32(ahci::HBA_GHC)? & !2)?;
    }

    // Sector 0 first: its first bytes are the pattern CI wrote into the image.
    data.write(0, &[0u8; SECTOR_BYTES as usize]);
    transfer(
        &port,
        &page,
        &data,
        Command {
            write: false,
            opcode: None,
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
            opcode: None,
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
            opcode: None,
            lba: ROUND_TRIP_SECTOR,
            sectors: ROUND_TRIP_SECTORS,
        },
    )?;
    let mut back = [0u8; ROUND_TRIP_BYTES];
    data.read(0, &mut back);
    if back != written {
        return Err(Failure::RoundTrip);
    }
    if MSI_ACTIVE[dev].load(Ordering::Acquire) {
        let _ = writeln!(
            console,
            "ahci: MSI completion interrupts: {} of 3 commands, vector {vector:#x}",
            COMPLETIONS[dev].load(Ordering::Acquire) - initial_completions
        );
    } else {
        let _ = writeln!(console, "ahci: polled completion fallback; no local APIC");
    }
    let checksum = crc32(&back);
    let _ = writeln!(
        console,
        "ahci: wrote {ROUND_TRIP_SECTORS} sectors at {ROUND_TRIP_SECTOR}, read back, \
         crc32 {checksum:#010x} matches the written data"
    );
    // Obtain the real disk geometry before trusting GPT addresses.
    transfer(
        &port,
        &page,
        &data,
        Command {
            write: false,
            opcode: Some(0xec),
            lba: 0,
            sectors: 1,
        },
    )?;
    let identify = data.read_array::<512>(0);
    let capacity = ahci::identify_sectors(&identify).ok_or(Failure::Encoding)?;
    let mut disk = Disk {
        owner: dev,
        capacity,
        bdf,
        hba,
        number,
        page,
        data,
    };
    let (first, sectors) =
        super::storage_volume::discover(&mut disk, capacity).map_err(Failure::Filesystem)?;
    super::storage_volume::run(&mut disk, first, sectors, console, "ahci")
        .map_err(Failure::Filesystem)?;
    Ok(disk)
}

/// The first implemented port whose link is up and whose device is a SATA disk.
fn find_sata_disk(owner: usize, hba: &Region, ports: u32) -> Result<u8, Failure> {
    for number in 0..ahci::PORTS_MAX {
        if !ahci::port_implemented(ports, number) {
            continue;
        }
        let port = Port { owner, hba, number };
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
    opcode: Option<u8>,
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
    blockdev::run(
        blockdev::device(port.owner).ok_or(QueueError::UnknownDevice)?,
        request,
        || issue(port, page, data, command),
    )
}

/// Issue the command in the port's slot 0 and wait for it. Called through [`transfer`].
fn issue(port: &Port, page: &Dma, data: &Dma, command: Command) -> Result<(), Failure> {
    let expected = COMPLETIONS[port.owner].load(Ordering::Acquire) + 1;
    let bytes = u32::from(command.sectors) * SECTOR_BYTES;
    let header = ahci::encode_command_header(&CommandHeader {
        fis_dwords: 5,
        write: command.write,
        prdt_entries: u16::from(command.sectors != 0),
        table: page.address(COMMAND_TABLE_AT),
    })
    .ok_or(Failure::Encoding)?;
    let prdt = if bytes != 0 {
        Some(ahci::prdt_entry(data.address(0), bytes).ok_or(Failure::Encoding)?)
    } else {
        None
    };
    let fis = if let Some(opcode) = command.opcode {
        match opcode {
            0xec => ahci::h2d_identify(),
            0xea => ahci::h2d_flush_cache(),
            _ => return Err(Failure::Encoding),
        }
    } else if command.write {
        ahci::h2d_write_dma_ext(command.lba, command.sectors)
    } else {
        ahci::h2d_read_dma_ext(command.lba, command.sectors)
    };
    page.write(COMMAND_LIST_AT, &header);
    page.write(COMMAND_TABLE_AT, &fis);
    if let Some(prdt) = prdt {
        page.write(COMMAND_TABLE_AT + ahci::PRDT_OFFSET as u64, &prdt);
    }
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
    if MSI_ACTIVE[port.owner].load(Ordering::Acquire)
        && super::msix::wait_for_interrupts(&COMPLETIONS[port.owner], expected) < expected
    {
        return Err(Failure::Interrupt);
    }
    fence(Ordering::SeqCst);
    Ok(())
}

/// Poll with a two-second HPET budget and hard iteration bound, or a bounded clockless fallback.
fn poll(
    mut ready: impl FnMut() -> Result<bool, Failure>,
    step: &'static str,
) -> Result<(), Failure> {
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    for iteration in 0..POLL_LIMIT * 100 {
        if ready()? {
            return Ok(());
        }
        match (clock, super::hpet::counter()) {
            (Some((start, frequency)), Some(now)) => {
                if super::hpet::elapsed_ticks(start, now) >= frequency.saturating_mul(2) {
                    break;
                }
            }
            _ if iteration >= POLL_LIMIT - 1 => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    Err(Failure::Timeout(step))
}

/// One implemented port's registers, reached through the HBA window.
struct Port<'a> {
    owner: usize,
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

/// Owned transport state for a SATA port, reused for every filesystem request.
struct Disk {
    owner: usize,
    capacity: u64,
    bdf: Bdf,
    hba: Region,
    number: u8,
    page: Dma,
    data: Dma,
}
impl super::storage_volume::SectorDisk for Disk {
    fn sector_count(&self) -> u64 {
        self.capacity
    }
    fn flush(&mut self) -> Result<(), nolxfs::DevError> {
        issue(
            &Port {
                owner: self.owner,
                hba: &self.hba,
                number: self.number,
            },
            &self.page,
            &self.data,
            Command {
                write: false,
                opcode: Some(0xea),
                lba: 0,
                sectors: 0,
            },
        )
        .map_err(|_| nolxfs::DevError::Io(5))
    }
    fn read_sectors(&mut self, first: u64, out: &mut [u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, out.len())?;
        for (i, chunk) in out.chunks_mut(4096).enumerate() {
            transfer(
                &Port {
                    owner: self.owner,
                    hba: &self.hba,
                    number: self.number,
                },
                &self.page,
                &self.data,
                Command {
                    write: false,
                    opcode: None,
                    lba: first + (i * 8) as u64,
                    sectors: (chunk.len() / 512) as u16,
                },
            )
            .map_err(|_| nolxfs::DevError::Io(5))?;
            self.data.read(0, chunk);
        }
        Ok(())
    }
    fn write_sectors(&mut self, first: u64, bytes: &[u8]) -> Result<(), nolxfs::DevError> {
        nolxfs::sector::check_sector_io(self.capacity, first, bytes.len())?;
        for (i, chunk) in bytes.chunks(4096).enumerate() {
            self.data.write(0, chunk);
            transfer(
                &Port {
                    owner: self.owner,
                    hba: &self.hba,
                    number: self.number,
                },
                &self.page,
                &self.data,
                Command {
                    write: true,
                    opcode: None,
                    lba: first + (i * 8) as u64,
                    sectors: (chunk.len() / 512) as u16,
                },
            )
            .map_err(|_| nolxfs::DevError::Io(5))?;
        }
        Ok(())
    }
}

impl Disk {
    fn quiesce(&mut self) -> Result<(), Failure> {
        nolxfs::sector::SectorDevice::flush(self).map_err(|_| Failure::Encoding)?;
        let port = Port {
            owner: self.owner,
            hba: &self.hba,
            number: self.number,
        };
        port.stop()?;
        port.write(0x14, 0)?;
        self.hba
            .write32(ahci::HBA_GHC, self.hba.read32(ahci::HBA_GHC)? & !2)?;
        *INTERRUPT_REGISTERS[self.owner].lock() = None;
        MSI_ACTIVE[self.owner].store(false, Ordering::Release);
        let command = ConfigAccess.read_u32(self.bdf, 4) & 0xffff;
        ConfigAccess.write_u32(self.bdf, 4, command & !4);
        blockdev::detach(blockdev::device(self.owner).ok_or(QueueError::UnknownDevice)?)?;
        Ok(())
    }
    fn release(self) {
        // SAFETY: both AHCI engines stopped, interrupts were detached and PCI DMA disabled.
        unsafe {
            self.page.release();
            self.data.release();
        }
    }
}
