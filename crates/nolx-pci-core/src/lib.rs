//! PCI configuration-space logic, tested on an ordinary host.
//!
//! Milestone 9 starts here: every device driver NOLX will have (AHCI, NVMe, virtio, xHCI,
//! e1000) is found through PCI configuration space, and the decisions about what that space
//! means -- how a bus/device/function triple becomes an address, how a function header is
//! read, how a BAR is decoded and sized, how the capability list is walked, and how a bus
//! tree is enumerated -- are pure logic. They live here with no port access and
//! `forbid(unsafe_code)`.
//!
//! The kernel supplies the one thing this crate cannot have: a [`ConfigSpace`] that reads and
//! writes dwords through the legacy mechanism (`0xCF8` address, `0xCFC` data). Everything else
//! is written against that trait, so a test can model a whole machine in a few lines.
//!
//! Scope: legacy configuration access, MSI/MSI-X rules in [`msi`], and validated PCIe
//! ECAM address calculations in [`ecam`]. The kernel still uses legacy configuration
//! ports for drivers, with a read-only ECAM boot probe using MCFG discovery.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod ecam;
pub mod msi;

/// I/O port holding the configuration address register.
pub const CONFIG_ADDRESS_PORT: u16 = 0xCF8;
/// I/O port holding the configuration data register.
pub const CONFIG_DATA_PORT: u16 = 0xCFC;

/// The vendor ID a configuration read returns when no function responds.
pub const VENDOR_NONE: u16 = 0xFFFF;

/// The most functions an [`Inventory`] can record.
pub const MAX_FUNCTIONS: usize = 256;
/// The number of BAR slots in a type-0 header.
pub const BAR_SLOTS: usize = 6;
/// The most capabilities [`capabilities`] will walk before it reports the list as truncated.
pub const MAX_CAPABILITIES: usize = 48;

/// Offset of the vendor/device ID dword.
const OFF_VENDOR: u8 = 0x00;
/// Offset of the command/status dword.
const OFF_COMMAND: u8 = 0x04;
/// Offset of the class/revision dword.
const OFF_CLASS: u8 = 0x08;
/// Offset of the header-type dword (cache line, latency, header type, BIST).
const OFF_HEADER: u8 = 0x0C;
/// Offset of the first BAR.
const OFF_BAR0: u8 = 0x10;
/// Offset of the capability-list pointer in a type-0 or type-1 header.
const OFF_CAP_POINTER: u8 = 0x34;
/// Offset of the secondary-bus number in a type-1 (PCI-to-PCI bridge) header.
const OFF_SECONDARY_BUS: u8 = 0x18;

/// Bit 4 of the status register: the capability list exists.
const STATUS_CAP_LIST: u32 = 1 << 20;
/// The command-register bits that let a function answer I/O (bit 0) and memory (bit 1) cycles.
const COMMAND_DECODE_MASK: u32 = 0x3;
/// Bit 7 of the header-type byte: the device has more than one function.
const HEADER_MULTIFUNCTION: u8 = 0x80;
/// Class code and subclass of a PCI-to-PCI bridge.
const CLASS_BRIDGE: u8 = 0x06;
const SUBCLASS_PCI_BRIDGE: u8 = 0x04;

/// Failure in the configuration-space logic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PciError {
    /// A device number was 32 or more.
    InvalidDevice,
    /// A function number was 8 or more.
    InvalidFunction,
    /// An [`Inventory`] already holds [`MAX_FUNCTIONS`] functions.
    Full,
}

/// A bus/device/function triple.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bdf {
    /// Bus number, 0 to 255.
    pub bus: u8,
    /// Device number, 0 to 31.
    pub device: u8,
    /// Function number, 0 to 7.
    pub function: u8,
}

impl Bdf {
    /// Build a triple, refusing device and function numbers the hardware cannot express.
    pub const fn new(bus: u8, device: u8, function: u8) -> Result<Self, PciError> {
        if device >= 32 {
            return Err(PciError::InvalidDevice);
        }
        if function >= 8 {
            return Err(PciError::InvalidFunction);
        }
        Ok(Self {
            bus,
            device,
            function,
        })
    }
}

/// The value written to `0xCF8` to select one dword of configuration space.
///
/// Bit 31 enables the mapping, bits 23:16 are the bus, 15:11 the device, 10:8 the function,
/// and bits 7:2 the dword index. The low two bits of `offset` are discarded: the mechanism
/// only addresses whole dwords, and the data port supplies the byte lanes.
#[must_use]
pub const fn config_address(bdf: Bdf, offset: u8) -> u32 {
    0x8000_0000
        | ((bdf.bus as u32) << 16)
        | ((bdf.device as u32 & 0x1F) << 11)
        | ((bdf.function as u32 & 0x07) << 8)
        | (offset as u32 & 0xFC)
}

/// Access to one dword of configuration space at a time.
///
/// `offset` is a byte offset within the 256-byte legacy space. Implementations address the
/// dword containing it; callers pass dword-aligned offsets.
pub trait ConfigSpace {
    /// Read the dword at `offset` of function `bdf`.
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32;
    /// Write the dword at `offset` of function `bdf`.
    fn write_u32(&mut self, bdf: Bdf, offset: u8, value: u32);
}

/// The identity and class of one function, read from its header.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FunctionInfo {
    /// Where the function sits.
    pub bdf: Bdf,
    /// Vendor ID, never [`VENDOR_NONE`] in a recorded function.
    pub vendor_id: u16,
    /// Device ID as the vendor assigned it.
    pub device_id: u16,
    /// Base class code.
    pub class: u8,
    /// Subclass code.
    pub subclass: u8,
    /// Programming interface.
    pub prog_if: u8,
    /// Revision ID.
    pub revision: u8,
    /// Header type, with the multifunction bit masked off (0 = endpoint, 1 = bridge).
    pub header_type: u8,
    /// Whether the device advertises more functions at the same device number.
    pub multifunction: bool,
}

/// Read the identity of the function at `bdf`, or `None` if nothing responds there.
pub fn read_function<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> Option<FunctionInfo> {
    let vendor_dword = cfg.read_u32(bdf, OFF_VENDOR);
    let vendor_id = (vendor_dword & 0xFFFF) as u16;
    if vendor_id == VENDOR_NONE || vendor_id == 0 {
        return None;
    }
    let device_id = (vendor_dword >> 16) as u16;
    let class_dword = cfg.read_u32(bdf, OFF_CLASS);
    let header_dword = cfg.read_u32(bdf, OFF_HEADER);
    let header_byte = (header_dword >> 16) as u8;
    Some(FunctionInfo {
        bdf,
        vendor_id,
        device_id,
        revision: class_dword as u8,
        prog_if: (class_dword >> 8) as u8,
        subclass: (class_dword >> 16) as u8,
        class: (class_dword >> 24) as u8,
        header_type: header_byte & !HEADER_MULTIFUNCTION,
        multifunction: header_byte & HEADER_MULTIFUNCTION != 0,
    })
}

/// One decoded base address register.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bar {
    /// The slot is not implemented, or its value is zero.
    #[default]
    Unused,
    /// A 32-bit memory window.
    Memory32 {
        /// Physical base, aligned to the window size.
        base: u32,
        /// Whether reads have no side effects and writes may be combined.
        prefetchable: bool,
    },
    /// A 64-bit memory window, held in two consecutive slots.
    Memory64 {
        /// Physical base, aligned to the window size.
        base: u64,
        /// Whether reads have no side effects and writes may be combined.
        prefetchable: bool,
    },
    /// An I/O port window.
    Io {
        /// First port of the window.
        port: u32,
    },
    /// A reserved memory type. Recorded so that the slot is not misread as a 64-bit pair.
    Reserved,
}

/// How many slots a BAR occupies after decoding: 2 for a 64-bit memory BAR, otherwise 1.
#[must_use]
pub const fn bar_slots(bar: &Bar) -> usize {
    match bar {
        Bar::Memory64 { .. } => 2,
        _ => 1,
    }
}

/// Decode a BAR from its raw low dword, and the high dword when the type calls for one.
///
/// The high dword is consulted only for 64-bit memory BARs, so passing any value for it is
/// harmless otherwise.
#[must_use]
pub const fn decode_bar(low: u32, high: u32) -> Bar {
    if low & 0x1 == 0x1 {
        return if low & !0x3 == 0 {
            Bar::Unused
        } else {
            Bar::Io { port: low & !0x3 }
        };
    }
    let prefetchable = low & 0x8 != 0;
    match (low >> 1) & 0x3 {
        0x0 => {
            if low & !0xF == 0 {
                Bar::Unused
            } else {
                Bar::Memory32 {
                    base: low & !0xF,
                    prefetchable,
                }
            }
        }
        0x2 => {
            let base = ((high as u64) << 32) | (low & !0xF) as u64;
            if base == 0 {
                Bar::Unused
            } else {
                Bar::Memory64 { base, prefetchable }
            }
        }
        _ => Bar::Reserved,
    }
}

/// The size of a BAR window, from the value read back after writing all ones.
///
/// `probe_low` and `probe_high` are the dwords read back after the device was written with
/// `0xFFFF_FFFF` in each slot. Returns `None` when the device implements no bits, which is
/// how a BAR says "this slot is not used". The window size is always a power of two.
#[must_use]
pub const fn bar_size(bar: &Bar, probe_low: u32, probe_high: u32) -> Option<u64> {
    match bar {
        Bar::Unused | Bar::Reserved => None,
        Bar::Io { .. } => {
            let mask = (probe_low & !0x3) as u64;
            size_from_mask(mask, 32)
        }
        Bar::Memory32 { .. } => {
            let mask = (probe_low & !0xF) as u64;
            size_from_mask(mask, 32)
        }
        Bar::Memory64 { .. } => {
            let mask = ((probe_high as u64) << 32) | (probe_low & !0xF) as u64;
            size_from_mask(mask, 64)
        }
    }
}

/// `2^k` where `k` is the number of trailing zeros of the writable address bits, given the
/// mask of bits the device let us set. A zero mask means no bits are implemented.
const fn size_from_mask(mask: u64, width: u32) -> Option<u64> {
    if mask == 0 {
        return None;
    }
    let size = (!mask).wrapping_add(1);
    // `wrapping_add` on a 32-bit window wraps at 2^64, so keep only the width that exists.
    let size = if width == 32 {
        size & 0xFFFF_FFFF
    } else {
        size
    };
    if size == 0 {
        None
    } else {
        Some(size)
    }
}

/// The decoded BARs of a type-0 function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BarSet {
    /// One entry per slot. The upper half of a 64-bit BAR is [`Bar::Unused`].
    pub slots: [Bar; BAR_SLOTS],
    /// Window size per slot, `None` where the slot is unused or reserved.
    pub sizes: [Option<u64>; BAR_SLOTS],
}

/// Read, size, and decode the BARs of a type-0 function.
///
/// Sizing follows the usual protocol: save the register, write all ones, read the mask the
/// device kept, and restore the original value. The restore happens even when the probe finds
/// nothing, so a function is never left with a half-sized window.
///
/// While the probe runs, the function's I/O and memory decoding are switched off in the command
/// register, as the PCI specification recommends: a device answering at the bogus addresses a
/// probe writes would otherwise claim memory it does not own. The command register is then put
/// back exactly as it was. Only its low half is written, so the status bits (some of which are
/// write-one-to-clear) are never touched.
pub fn read_bars<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> BarSet {
    let mut set = BarSet::default();
    let command = cfg.read_u32(bdf, OFF_COMMAND) & 0xFFFF;
    cfg.write_u32(bdf, OFF_COMMAND, command & !COMMAND_DECODE_MASK);
    let mut slot = 0usize;
    while slot < BAR_SLOTS {
        let offset = OFF_BAR0 + (slot as u8) * 4;
        let original_low = cfg.read_u32(bdf, offset);
        // A 64-bit window in the last slot has no room for its upper half: it is reserved,
        // and it is never probed, because writing the slot would touch a register that is not
        // ours to size.
        let is_64 = original_low & 0x1 == 0 && (original_low >> 1) & 0x3 == 0x2;
        if is_64 && slot + 1 >= BAR_SLOTS {
            set.slots[slot] = Bar::Reserved;
            slot += 1;
            continue;
        }
        let original_high = if is_64 {
            cfg.read_u32(bdf, offset + 4)
        } else {
            0
        };
        let bar = decode_bar(original_low, original_high);
        let width = bar_slots(&bar);

        // Probe. Writing all ones and reading back reveals the implemented address bits.
        cfg.write_u32(bdf, offset, 0xFFFF_FFFF);
        let probed_low = cfg.read_u32(bdf, offset);
        let mut probed_high = 0;
        if width == 2 {
            cfg.write_u32(bdf, offset + 4, 0xFFFF_FFFF);
            probed_high = cfg.read_u32(bdf, offset + 4);
            cfg.write_u32(bdf, offset + 4, original_high);
        }
        cfg.write_u32(bdf, offset, original_low);

        set.slots[slot] = bar;
        set.sizes[slot] = bar_size(&bar, probed_low, probed_high);
        if width == 2 {
            set.slots[slot + 1] = Bar::Unused;
        }
        slot += width;
    }
    cfg.write_u32(bdf, OFF_COMMAND, command);
    set
}

/// One entry of the capability list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Capability {
    /// Byte offset of the capability header within configuration space.
    pub offset: u8,
    /// Capability ID (for example 0x05 is MSI, 0x11 is MSI-X, 0x10 is PCI Express).
    pub id: u8,
}

/// The capabilities of one function, in list order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapabilityList {
    /// The entries found, in the order the list links them.
    pub entries: [Capability; MAX_CAPABILITIES],
    /// How many entries are valid.
    pub len: usize,
    /// True if the walk stopped at [`MAX_CAPABILITIES`] with the list still linked. A list
    /// that long is either broken or looped; the caller must not treat it as complete.
    pub truncated: bool,
}

impl Default for CapabilityList {
    fn default() -> Self {
        Self {
            entries: [Capability::default(); MAX_CAPABILITIES],
            len: 0,
            truncated: false,
        }
    }
}

impl CapabilityList {
    /// The valid entries.
    #[must_use]
    pub fn as_slice(&self) -> &[Capability] {
        &self.entries[..self.len]
    }

    /// The first capability with `id`, if the function has one.
    #[must_use]
    pub fn find(&self, id: u8) -> Option<Capability> {
        self.as_slice().iter().copied().find(|cap| cap.id == id)
    }
}

/// Walk the capability list of `bdf`.
///
/// A function without the capability-list bit in its status register has no list. Pointers
/// below 0x40 are the header and are refused, and their low two bits are reserved and masked
/// off. The walk is bounded by [`MAX_CAPABILITIES`], so a looped list cannot hang boot.
pub fn capabilities<C: ConfigSpace>(cfg: &mut C, bdf: Bdf) -> CapabilityList {
    let mut list = CapabilityList::default();
    let status = cfg.read_u32(bdf, OFF_COMMAND);
    if status & STATUS_CAP_LIST == 0 {
        return list;
    }
    let mut pointer = (cfg.read_u32(bdf, OFF_CAP_POINTER) as u8) & !0x3;
    while pointer >= 0x40 {
        if list.len == MAX_CAPABILITIES {
            list.truncated = true;
            break;
        }
        let header = cfg.read_u32(bdf, pointer);
        let id = header as u8;
        let next = ((header >> 8) as u8) & !0x3;
        list.entries[list.len] = Capability {
            offset: pointer,
            id,
        };
        list.len += 1;
        pointer = next;
    }
    list
}

/// Every function found by [`enumerate`], in discovery order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Inventory {
    /// The recorded functions. Entries past `len` are default and meaningless.
    pub functions: [FunctionInfo; MAX_FUNCTIONS],
    /// How many functions were recorded.
    pub len: usize,
    /// True if a function was found after the inventory was full, so the list is incomplete.
    pub overflowed: bool,
    /// Buses reached through a bridge whose secondary number was not above its own bus.
    /// Firmware that assigns bus numbers this way is broken; such a bridge is not followed.
    pub refused_bridges: usize,
}

impl Default for Inventory {
    fn default() -> Self {
        Self {
            functions: [FunctionInfo::default(); MAX_FUNCTIONS],
            len: 0,
            overflowed: false,
            refused_bridges: 0,
        }
    }
}

impl Inventory {
    /// The recorded functions.
    #[must_use]
    pub fn as_slice(&self) -> &[FunctionInfo] {
        &self.functions[..self.len]
    }

    /// The first function whose class and subclass match.
    #[must_use]
    pub fn find_class(&self, class: u8, subclass: u8) -> Option<FunctionInfo> {
        self.as_slice()
            .iter()
            .copied()
            .find(|info| info.class == class && info.subclass == subclass)
    }

    /// Record one function.
    pub fn push(&mut self, info: FunctionInfo) -> Result<(), PciError> {
        if self.len == MAX_FUNCTIONS {
            self.overflowed = true;
            return Err(PciError::Full);
        }
        self.functions[self.len] = info;
        self.len += 1;
        Ok(())
    }
}

/// Enumerate every function reachable from bus 0.
///
/// Each bus is scanned once. Bus 0 is always reached; any other bus is reached only through a
/// PCI-to-PCI bridge (header type 1) that the scan has already seen. A bridge's secondary bus
/// must be above the bridge's own bus, which is how firmware numbers them in depth-first order;
/// a bridge that breaks that rule is counted in [`Inventory::refused_bridges`] and not followed,
/// which also makes the scan terminate on any input.
pub fn enumerate<C: ConfigSpace>(cfg: &mut C) -> Inventory {
    let mut inventory = Inventory::default();
    let mut reached = [false; 256];
    reached[0] = true;
    let mut bus = 0usize;
    while bus < 256 {
        if reached[bus] {
            scan_bus(cfg, bus as u8, &mut inventory, &mut reached);
        }
        bus += 1;
    }
    inventory
}

fn scan_bus<C: ConfigSpace>(
    cfg: &mut C,
    bus: u8,
    inventory: &mut Inventory,
    reached: &mut [bool; 256],
) {
    let mut device = 0u8;
    while device < 32 {
        // A device number is valid by construction; `Bdf::new` cannot fail here.
        if let Ok(first) = Bdf::new(bus, device, 0) {
            if let Some(info) = read_function(cfg, first) {
                let functions = if info.multifunction { 8 } else { 1 };
                record(cfg, info, inventory, reached, bus);
                let mut function = 1u8;
                while function < functions {
                    if let Ok(bdf) = Bdf::new(bus, device, function) {
                        if let Some(extra) = read_function(cfg, bdf) {
                            record(cfg, extra, inventory, reached, bus);
                        }
                    }
                    function += 1;
                }
            }
        }
        device += 1;
    }
}

fn record<C: ConfigSpace>(
    cfg: &mut C,
    info: FunctionInfo,
    inventory: &mut Inventory,
    reached: &mut [bool; 256],
    bus: u8,
) {
    let _ = inventory.push(info);
    if info.class == CLASS_BRIDGE && info.subclass == SUBCLASS_PCI_BRIDGE && info.header_type == 1 {
        let secondary = (cfg.read_u32(info.bdf, OFF_SECONDARY_BUS) >> 8) as u8;
        if secondary > bus && !reached[secondary as usize] {
            reached[secondary as usize] = true;
        } else if secondary <= bus {
            inventory.refused_bridges += 1;
        }
    }
}

#[cfg(test)]
mod tests;
