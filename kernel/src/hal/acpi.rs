//! Early physical-memory discovery for ACPI root, MADT, and HPET tables.

use core::ptr;

use nolx_hal_core::acpi::{HpetInfo, Madt, Rsdp, Sdt};

const IDENTITY_MAP_LIMIT: u64 = 0x8000_0000;
const MAX_SDT_LENGTH: usize = 1024 * 1024;
const EBDA_POINTER: usize = 0x40E;
const EBDA_MIN: u64 = 0x400;
const EBDA_MAX: u64 = 0xA0000;
const BIOS_SCAN_START: u64 = 0xE0000;
const BIOS_SCAN_END: u64 = 0x100000;

/// Firmware tables needed by the local-APIC and HPET HAL backends.
pub struct PlatformTables {
    /// MADT processor and I/O APIC topology.
    pub madt: Madt,
    /// HPET register-block metadata, when firmware published an HPET table.
    pub hpet: Option<HpetInfo>,
    /// `true` when the platform root was an XSDT, otherwise an RSDT.
    pub used_xsdt: bool,
    /// ACPI revision reported by the RSDP.
    pub rsdp_revision: u8,
}

/// Find and validate the platform RSDP, root table, MADT, and optional HPET table.
///
/// Firmware tables outside the current identity-mapped physical window are ignored. The early
/// kernel has no page allocator yet, so later memory-manager work will replace this restriction
/// with explicit reserved-memory mappings.
#[must_use]
pub fn discover() -> Option<PlatformTables> {
    let rsdp = find_rsdp()?;
    let roots = [rsdp.xsdt_address, u64::from(rsdp.rsdt_address)];
    for (index, address) in roots.into_iter().enumerate() {
        if address == 0 {
            continue;
        }
        let Some(root) = read_sdt_at(address) else {
            continue;
        };
        let expected = if index == 0 { *b"XSDT" } else { *b"RSDT" };
        if root.signature() != expected {
            continue;
        }
        let Ok(entries) = root.root_entries() else {
            continue;
        };

        let mut madt = None;
        let mut hpet = None;
        for physical_address in entries {
            let Some(table) = read_sdt_at(physical_address) else {
                continue;
            };
            match table.signature() {
                signature if signature == *b"APIC" && madt.is_none() => {
                    madt = Madt::parse(table.bytes()).ok();
                }
                signature if signature == *b"HPET" && hpet.is_none() => {
                    hpet = HpetInfo::parse(table.bytes()).ok();
                }
                _ => {}
            }
        }
        if let Some(madt) = madt {
            return Some(PlatformTables {
                madt,
                hpet,
                used_xsdt: index == 0,
                rsdp_revision: rsdp.revision,
            });
        }
    }
    None
}

fn find_rsdp() -> Option<Rsdp> {
    // SAFETY: the BIOS Data Area is identity-mapped by the boot page tables and the word at
    // 0x40E is the architecturally conventional segment address of the EBDA.
    let ebda_segment = unsafe { ptr::read_volatile(EBDA_POINTER as *const u16) };
    let ebda_start = u64::from(ebda_segment) << 4;
    if (EBDA_MIN..EBDA_MAX).contains(&ebda_start) {
        let ebda_end = (ebda_start + 1024).min(EBDA_MAX);
        if let Some(rsdp) = find_rsdp_in(ebda_start, ebda_end) {
            return Some(rsdp);
        }
    }
    find_rsdp_in(BIOS_SCAN_START, BIOS_SCAN_END)
}

fn find_rsdp_in(start: u64, end: u64) -> Option<Rsdp> {
    let mut address = start.checked_add(15)? & !15;
    while address.checked_add(20)? <= end {
        let available = usize::try_from(end - address).ok()?;
        // SAFETY: the scan range is a BIOS/EBDA region below 1 MiB, identity-mapped by the
        // boot trampoline; `available` is bounded by the containing scan range.
        let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, available) };
        if bytes.get(..8) == Some(&b"RSD PTR "[..]) {
            if let Ok(rsdp) = Rsdp::parse(bytes) {
                return Some(rsdp);
            }
        }
        address = address.checked_add(16)?;
    }
    None
}

fn read_sdt_at(address: u64) -> Option<Sdt<'static>> {
    let header_end = address.checked_add(36)?;
    if address < 0x1000 || header_end > IDENTITY_MAP_LIMIT {
        return None;
    }
    // SAFETY: the boot page tables identity-map physical addresses below 2 GiB, and the
    // checked address includes the complete fixed-width SDT header.
    let header = unsafe { core::slice::from_raw_parts(address as *const u8, 36) };
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if !(36..=MAX_SDT_LENGTH).contains(&length) {
        return None;
    }
    let table_end = address.checked_add(length as u64)?;
    if table_end > IDENTITY_MAP_LIMIT {
        return None;
    }
    // SAFETY: the entire declared table was bounded by the identity-mapped physical window;
    // firmware-owned ACPI tables remain readable for the kernel's lifetime.
    let bytes = unsafe { core::slice::from_raw_parts(address as *const u8, length) };
    Sdt::parse(bytes, None).ok()
}
