//! Early physical-memory discovery for ACPI root, MADT, HPET, MCFG, and power tables.

use core::ptr;

use crate::mm::phys;
use nolx_hal_core::acpi::{mcfg::Mcfg, HpetInfo, Madt, Rsdp, Sdt};
use nolx_power_core::{self as power, Fadt, SleepState};

/// How much physical memory the direct map covers. Tables outside it cannot be read, and a
/// firmware table above the line is reported as absent rather than followed into a fault.
const DIRECT_MAP_LIMIT: u64 = phys::DIRECT_MAP_BYTES;
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
    /// Validated PCIe ECAM allocations, when firmware publishes an MCFG table.
    pub mcfg: Option<Mcfg>,
    /// `true` when the platform root was an XSDT, otherwise an RSDT.
    pub used_xsdt: bool,
    /// ACPI revision reported by the RSDP.
    pub rsdp_revision: u8,
    /// Fixed ACPI description: the DSDT address and the PM1 control port, when a FADT was found.
    pub fadt: Option<Fadt>,
    /// The S5 (soft-off) sleep type named in the DSDT, when the DSDT was read and names one.
    pub s5_sleep_type: Option<u8>,
    /// The S3 (suspend-to-RAM) sleep type named in the DSDT, when the DSDT was read and names one.
    pub s3_sleep_type: Option<u8>,
}

/// Find and validate the platform RSDP, root table, MADT, and optional device tables.
///
/// Firmware tables outside the direct map are ignored. The early kernel has no allocator for
/// a dedicated mapping yet, so later memory-manager work will replace this restriction with
/// explicit reserved-memory mappings of their own.
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
        let mut fadt = None;
        let mut mcfg = None;
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
                signature if signature == power::FADT_SIGNATURE && fadt.is_none() => {
                    fadt = Fadt::parse(table.bytes()).ok();
                }
                signature if signature == *b"MCFG" && mcfg.is_none() => {
                    mcfg = Mcfg::parse(table.bytes()).ok();
                }
                _ => {}
            }
        }
        if let Some(madt) = madt {
            let (s5_sleep_type, s3_sleep_type) = match fadt {
                Some(fadt) => sleep_types(fadt.dsdt),
                None => (None, None),
            };
            return Some(PlatformTables {
                madt,
                hpet,
                mcfg,
                used_xsdt: index == 0,
                rsdp_revision: rsdp.revision,
                fadt,
                s5_sleep_type,
                s3_sleep_type,
            });
        }
    }
    None
}

fn find_rsdp() -> Option<Rsdp> {
    // SAFETY: the BIOS Data Area is inside the direct map, and the word at 0x40E is the
    // architecturally conventional segment address of the EBDA.
    let ebda_segment = unsafe { ptr::read_volatile(phys::direct_ptr::<u16>(EBDA_POINTER as u64)) };
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
        // SAFETY: the scan range is a BIOS/EBDA region below 1 MiB, which the direct map
        // covers; `available` is bounded by the containing scan range.
        let bytes =
            unsafe { core::slice::from_raw_parts(phys::direct_ptr::<u8>(address), available) };
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
    if address < 0x1000 || header_end > DIRECT_MAP_LIMIT {
        return None;
    }
    // SAFETY: the direct map covers physical addresses below 2 GiB, and the checked address
    // includes the complete fixed-width SDT header.
    let header = unsafe { core::slice::from_raw_parts(phys::direct_ptr::<u8>(address), 36) };
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    if !(36..=MAX_SDT_LENGTH).contains(&length) {
        return None;
    }
    let table_end = address.checked_add(length as u64)?;
    if table_end > DIRECT_MAP_LIMIT {
        return None;
    }
    // SAFETY: the entire declared table was bounded by the direct map's window; firmware-owned
    // ACPI tables remain readable through it for the kernel's lifetime.
    let bytes = unsafe { core::slice::from_raw_parts(phys::direct_ptr::<u8>(address), length) };
    Sdt::parse(bytes, None).ok()
}

/// The S5 and S3 sleep types the DSDT at `dsdt_address` names, read from its AML. A DSDT that
/// cannot be read, or does not name a type, gives `None` for that state.
fn sleep_types(dsdt_address: u64) -> (Option<u8>, Option<u8>) {
    let Some(dsdt) = read_sdt_at(dsdt_address) else {
        return (None, None);
    };
    if dsdt.signature() != *b"DSDT" {
        return (None, None);
    }
    let aml = dsdt.bytes();
    (
        power::sleep_type(aml, SleepState::S5.name()),
        power::sleep_type(aml, SleepState::S3.name()),
    )
}

/// One evaluated firmware device. Resource records are observations, not permission to
/// access hardware or allocate PCI apertures; native routing policy is a separate gate.
#[derive(Clone, Copy)]
pub struct FirmwareDevice {
    name: [u8; 160],
    length: usize,
    /// Evaluated/defaulted `_STA` value.
    pub status: u32,
    /// Fully validated `_CRS`, present only for usable devices with successful evaluation.
    pub resources: Option<power::Resources>,
}
impl FirmwareDevice {
    /// Absolute AML namespace path.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.length]).unwrap_or("invalid")
    }
}
impl core::fmt::Write for FirmwareDevice {
    fn write_str(&mut self, text: &str) -> core::fmt::Result {
        let end = self
            .length
            .checked_add(text.len())
            .ok_or(core::fmt::Error)?;
        let dst = self
            .name
            .get_mut(self.length..end)
            .ok_or(core::fmt::Error)?;
        dst.copy_from_slice(text.as_bytes());
        self.length = end;
        Ok(())
    }
}
const MAX_FIRMWARE_DEVICES: usize = 128;
static FIRMWARE_DEVICES: crate::sync::IrqMutex<[Option<FirmwareDevice>; MAX_FIRMWARE_DEVICES]> =
    crate::sync::IrqMutex::new([None; MAX_FIRMWARE_DEVICES]);

static FIRMWARE_CATALOG_COMPLETE: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);
/// Snapshot one evaluated firmware device without copying the whole catalog onto the stack.
pub fn firmware_device(index: usize) -> Option<FirmwareDevice> {
    FIRMWARE_DEVICES.lock().get(index).copied().flatten()
}
/// Whether all applicable namespace-device evaluations fit and completed without refusal.
/// Even true is not proof of a complete physical inventory or permission to route/map resources.
pub fn firmware_catalog_complete() -> bool {
    FIRMWARE_CATALOG_COMPLETE.load(core::sync::atomic::Ordering::Acquire)
}

/// Load checksummed DSDT/SSDT AML and evaluate device status/resources before driver startup.
/// Every unsupported operation is reported. Failed tables roll back atomically; failed status
/// is never defaulted to present, and failed/unsupported resources are never published.
#[inline(never)]
pub fn evaluate_devices(console: &mut dyn nolx_hal_core::console::HalConsole) {
    use core::fmt::Write;
    use power::aml::Namespace;
    FIRMWARE_DEVICES.lock().fill(None);
    FIRMWARE_CATALOG_COMPLETE.store(false, core::sync::atomic::Ordering::Release);
    let mut table_refusals = 0;
    let Some(platform) = discover() else {
        let _ = writeln!(console, "aml: no ACPI platform; namespace unavailable");
        return;
    };
    let mut loaded = 0;
    let dsdt = platform
        .fadt
        .and_then(|f| read_sdt_at(f.dsdt))
        .filter(|s| s.signature() == *b"DSDT");
    let Some(dsdt) = dsdt else {
        let _ = writeln!(
            console,
            "aml: DSDT unavailable or invalid; namespace incomplete"
        );
        return;
    };
    let mut namespace = Namespace::with_dsdt_revision(dsdt.bytes()[8]);
    match namespace.load(&dsdt.bytes()[36..]) {
        Ok(()) => {
            loaded += 1;
            let _ = writeln!(
                console,
                "aml: DSDT loaded atomically, {} objects",
                namespace.len()
            );
        }
        Err(error) => {
            table_refusals += 1;
            let _ = writeln!(console, "aml: DSDT refused atomically: {error:?}");
        }
    }
    if let Some(rsdp) = find_rsdp() {
        let address = if platform.used_xsdt {
            rsdp.xsdt_address
        } else {
            u64::from(rsdp.rsdt_address)
        };
        if let Some(root) = read_sdt_at(address) {
            if let Ok(entries) = root.root_entries() {
                for address in entries {
                    let Some(ssdt) = read_sdt_at(address) else {
                        table_refusals += 1;
                        let _ = writeln!(
                            console,
                            "aml: invalid root entry at {address:#x}; namespace incomplete"
                        );
                        continue;
                    };
                    if ssdt.signature() != *b"SSDT" {
                        continue;
                    }
                    match namespace.load(&ssdt.bytes()[36..]) {
                        Ok(()) => {
                            loaded += 1;
                            let _ = writeln!(
                                console,
                                "aml: SSDT loaded atomically, {} objects",
                                namespace.len()
                            );
                        }
                        Err(error) => {
                            table_refusals += 1;
                            let _ = writeln!(console, "aml: SSDT refused atomically: {error:?}");
                        }
                    }
                }
            } else {
                table_refusals += 1;
            }
        } else {
            table_refusals += 1;
        }
    } else {
        table_refusals += 1;
    }
    let mut retained = 0;
    let mut ready = 0;
    let mut refused = table_refusals;
    for path in namespace.devices() {
        let (status, defaulted) = match namespace.device_status(path) {
            Ok(value) => value,
            Err(error) => {
                refused += 1;
                let _ = writeln!(
                    console,
                    "aml: {path} _STA refused: {error:?}; device not admitted"
                );
                continue;
            }
        };
        let _ = writeln!(
            console,
            "aml: {path} _STA={status:#x}, defaulted={defaulted}"
        );
        let resources = if power::is_usable(status) {
            match namespace.device_resources(path) {
                Ok(Some(bytes)) => {
                    match power::parse_resources(&bytes) {
                        Ok(resources) => {
                            ready += 1;
                            let _ = writeln!(
                                console,
                                "aml: {path} _CRS evaluated and decoded: {} resources",
                                resources.len()
                            );
                            Some(resources)
                        }
                        Err(error) => {
                            refused += 1;
                            let _ = writeln!(console, "aml: {path} _CRS resource refusal: {error:?}; resources not published");
                            None
                        }
                    }
                }
                Ok(None) => None,
                Err(error) => {
                    refused += 1;
                    let _ = writeln!(
                        console,
                        "aml: {path} _CRS evaluation refused: {error:?}; resources not published"
                    );
                    None
                }
            }
        } else {
            let _ = writeln!(
                console,
                "aml: {path} _CRS skipped: device status is not usable"
            );
            None
        };
        if retained == MAX_FIRMWARE_DEVICES {
            refused += 1;
            let _ = writeln!(
                console,
                "aml: firmware catalog capacity refused; no silent truncation"
            );
            break;
        }
        let mut device = FirmwareDevice {
            name: [0; 160],
            length: 0,
            status,
            resources,
        };
        if write!(&mut device, "{path}").is_err() {
            refused += 1;
            continue;
        }
        FIRMWARE_DEVICES.lock()[retained] = Some(device);
        retained += 1;
    }
    FIRMWARE_CATALOG_COMPLETE.store(
        loaded > 0 && refused == 0,
        core::sync::atomic::Ordering::Release,
    );
    let _ = writeln!(console, "aml: namespace evaluation: {loaded} tables, {} objects, {retained} catalog devices, {ready} resource results, {refused} refusals", namespace.len());
    #[cfg(feature = "hal-aml-test")]
    {
        assert!(!firmware_catalog_complete());
        assert!(firmware_device(MAX_FIRMWARE_DEVICES).is_none());
        assert!(firmware_device(0).is_some());
        let catalog = FIRMWARE_DEVICES.lock();
        let fixture = catalog
            .iter()
            .flatten()
            .find(|d| d.name() == "\\_SB_.NLX0")
            .expect("missing AML firmware fixture");
        assert_eq!(fixture.status, 15);
        let resources = fixture.resources.unwrap();
        assert_eq!(resources.len(), 3);
        assert_eq!(
            resources.iter().collect::<alloc::vec::Vec<_>>(),
            [
                power::Resource::Io {
                    base: 0x3f8,
                    length: 8
                },
                power::Resource::Memory {
                    base: 0xfedc0000,
                    length: 4096
                },
                power::Resource::Irq { mask: 16 }
            ]
        );
        let absent = catalog
            .iter()
            .flatten()
            .find(|d| d.name() == "\\_SB_.NLX1")
            .unwrap();
        assert_eq!(absent.status, 0);
        assert!(absent.resources.is_none());
        for name in ["\\_SB_.NLX2", "\\_SB_.NLX4", "\\_SB_.NLX5"] {
            assert!(!catalog.iter().flatten().any(|d| d.name() == name));
        }
        assert!(catalog
            .iter()
            .flatten()
            .find(|d| d.name() == "\\_SB_.NLX3")
            .unwrap()
            .resources
            .is_none());
        let _ = writeln!(console, "aml: firmware method selftest: computed status, decoded resources, disabled-device gating, unsupported/loop/recursion refusal verified");
    }
}
