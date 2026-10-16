//! MSI-X for the storage drivers. A driver's device signals a completion through one MSI-X table
//! entry, which names a message address (this processor's local APIC) and a vector. The driver's
//! interrupt handler counts the vector; this module programs the entry, and waits for the handler
//! to count.

use core::arch::asm;
use core::sync::atomic::{AtomicUsize, Ordering};

use nolx_mm_core::mmio::MmioError;
use nolx_pci_core::{msi, Bar, BarSet, Bdf, ConfigSpace};

use super::apic;
use super::cpu::interrupts_enabled;
use super::pci::ConfigAccess;
use crate::mm::mmio::{self, Region};
use crate::mm::PAGE_SHIFT;

/// The page size the table's window is rounded to.
const PAGE: u64 = 1 << PAGE_SHIFT;
/// The most polls a wait for a delivery makes before it gives up.
const WAIT_LIMIT: u32 = 1_000_000;

/// Why MSI-X could not be set up for a function.
#[derive(Debug)]
pub enum Error {
    /// The function has no MSI-X capability.
    NoCapability,
    /// MSI capability/layout validation failed.
    Msi(msi::MsiProgramError),
    /// The entry is past the table, or the table is not in a memory BAR.
    Layout,
    /// The table's MMIO window was refused.
    Window(mmio::Failure),
    /// A register access was refused.
    Access(MmioError),
    /// The processor's APIC ID or the vector does not fit the message.
    Encoding,
}

impl From<mmio::Failure> for Error {
    fn from(error: mmio::Failure) -> Self {
        Self::Window(error)
    }
}

impl From<MmioError> for Error {
    fn from(error: MmioError) -> Self {
        Self::Access(error)
    }
}

/// The physical base of a memory BAR, or `None` when the slot is not one.
fn bar_base(bars: &BarSet, slot: u8) -> Result<u64, Error> {
    match bars.slots.get(usize::from(slot)) {
        Some(Bar::Memory32 { base, .. }) => Ok(u64::from(*base)),
        Some(Bar::Memory64 { base, .. }) => Ok(*base),
        _ => Err(Error::Layout),
    }
}

/// Enable MSI-X on `bdf`, and program table entry `entry` to deliver `vector` to this processor's
/// local APIC. The entry is unmasked. The other entries keep the state the device reset them to,
/// which is masked.
///
/// # Errors
/// [`Error`] for a missing capability, a table outside a memory BAR or past its end, a refused
/// mapping or register access, or an encoding the message cannot carry.
pub fn enable(bdf: Bdf, bars: &BarSet, entry: u16, vector: u8) -> Result<(), Error> {
    let list = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    let cap = list.find(msi::CAP_MSI_X).ok_or(Error::NoCapability)?;
    let header = ConfigAccess.read_u32(bdf, cap.offset);
    let table = ConfigAccess.read_u32(bdf, cap.offset + 4);
    let pba = ConfigAccess.read_u32(bdf, cap.offset + 8);
    let control = (header >> 16) as u16;
    let msix = msi::parse_msix(control, table, pba);
    if entry >= msix.table_size {
        return Err(Error::Layout);
    }

    // The table, in physical memory, and the page-aligned window that covers it.
    let table_at = bar_base(bars, msix.table_bar)? + u64::from(msix.table_offset);
    let table_end = table_at + u64::from(msix.table_size) * msi::MSIX_ENTRY_BYTES;
    let page = table_at & !(PAGE - 1);
    let length = (table_end - page).div_ceil(PAGE) * PAGE;
    let region: Region = mmio::map(page, length)?;

    let apic_id = u8::try_from(apic::current_apic_id()).map_err(|_| Error::Encoding)?;
    let address = u64::from(msi::message_address(apic_id));
    let words = msi::msix_entry(address, vector, false).ok_or(Error::Encoding)?;
    let at = table_at - page + u64::from(entry) * msi::MSIX_ENTRY_BYTES;
    for (index, word) in words.chunks_exact(4).enumerate() {
        let value = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
        region.write32(at + 4 * index as u64, value)?;
    }

    // Enable MSI-X with the whole function unmasked, keeping the rest of the header dword.
    let enabled = msi::msix_enable(control);
    ConfigAccess.write_u32(
        bdf,
        cap.offset,
        (u32::from(enabled) << 16) | (header & 0xFFFF),
    );
    Ok(())
}

/// Program a single MSI for a controller without an MSI-X table.
pub fn enable_msi(bdf: Bdf, vector: u8) -> Result<(), Error> {
    if !apic::is_enabled() {
        return Err(Error::Encoding);
    }
    let destination = u8::try_from(apic::current_apic_id()).map_err(|_| Error::Encoding)?;
    msi::program_msi(&mut ConfigAccess, bdf, destination, vector).map_err(Error::Msi)
}

/// Let interrupts through until `counter` reaches `expected`, or the bounded wait runs out, and
/// return the counter's value. The vector stays pending while IF is clear, and the local APIC keeps
/// one pending bit per vector, so a driver takes each completion before it submits the next
/// request: deliveries that queue up unserviced merge into one. IF is restored afterwards.
pub fn wait_for_interrupts(counter: &AtomicUsize, expected: usize) -> usize {
    let was_enabled = interrupts_enabled();
    // SAFETY: `sti` sets IF and changes nothing else. The IDT, the local APIC, and the driver's
    // vector are installed before the drivers run.
    unsafe { asm!("sti", options(nomem, nostack)) };
    let mut spins = 0u32;
    while counter.load(Ordering::Acquire) < expected && spins < WAIT_LIMIT {
        core::hint::spin_loop();
        spins += 1;
    }
    if !was_enabled {
        // SAFETY: `cli` clears IF, which it was clear before this call.
        unsafe { asm!("cli", options(nomem, nostack)) };
    }
    counter.load(Ordering::Acquire)
}
