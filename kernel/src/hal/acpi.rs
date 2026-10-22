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
static RSDP_ADVERTISED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

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
            RSDP_ADVERTISED.store(true, core::sync::atomic::Ordering::Release);
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
    /// True for the PCI root bridge itself.
    ///
    /// A root bridge's `_CRS` lists the *apertures it produces*, not resources it consumes.
    /// Reserving those as device reservations would collapse the allocator pool they define,
    /// so consumers of this catalog skip them.
    pub aperture: bool,
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

/// Root resource provenance: absent ACPI alone permits the old bounded board fallback.
// Fixed boot metadata must survive interpreter teardown without heap ownership. The
// bounded payload is only 392 bytes, not a large device catalog or DMA allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Copy)]
pub enum RootResourceState {
    /// No ACPI platform exists.
    Absent,
    /// ACPI exists but its namespace or root resources cannot authorize allocation.
    Refused,
    /// One completely validated root template, independent of unrelated device-method errors.
    Ready(power::root::RootResources),
}
static ROOT_RESOURCES: crate::sync::IrqMutex<RootResourceState> =
    crate::sync::IrqMutex::new(RootResourceState::Absent);
/// Snapshot evaluated root resource provenance before PCI allocation.
pub fn root_resources() -> RootResourceState {
    *ROOT_RESOURCES.lock()
}

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
    super::pci_intx::revoke();
    *ROOT_RESOURCES.lock() = RootResourceState::Absent;
    FIRMWARE_CATALOG_COMPLETE.store(false, core::sync::atomic::Ordering::Release);
    let mut table_refusals = 0;
    let Some(platform) = discover() else {
        if RSDP_ADVERTISED.load(core::sync::atomic::Ordering::Acquire) {
            *ROOT_RESOURCES.lock() = RootResourceState::Refused;
            let _ = writeln!(
                console,
                "aml: ACPI advertised but platform tables unavailable; allocation refused"
            );
        } else {
            let _ = writeln!(console, "aml: no ACPI platform; namespace unavailable");
        }
        return;
    };
    *ROOT_RESOURCES.lock() = RootResourceState::Refused;
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
    #[cfg(feature = "hal-pci-link-srs-test")]
    super::pci_intx::disable_links_for_test(console);
    let snapshot = super::pci_intx::Snapshot::capture();
    // Operation-region reads are authorized only for blocks another validated table named.
    // Writes are journaled intents until a writer is installed; then they are applied to
    // authorized hardware with exact permission and a verified readback.
    let regions = {
        let mut regions = super::aml_region::FirmwareRegions::empty();
        if let Some(hpet) = &platform.hpet {
            if hpet.base.address_space_id != 0 {
                let _ = writeln!(
                    console,
                    "aml: HPET registers at {:#x} are not memory; no HPET region window",
                    hpet.base.address
                );
            } else if regions.authorize_memory(hpet.base.address, 1024) {
                let _ = writeln!(
                    console,
                    "aml: operation-region window HPET memory {:#x}..{:#x}",
                    hpet.base.address,
                    hpet.base.address + 1024
                );
            }
        }
        if let Some(fadt) = &platform.fadt {
            let bytes = u64::from(if fadt.pm1_control_bytes == 0 {
                2
            } else {
                fadt.pm1_control_bytes
            });
            if regions.authorize_io(u64::from(fadt.pm1a_control), bytes) {
                let _ = writeln!(
                    console,
                    "aml: operation-region window PM1a control I/O {:#x}..{:#x}",
                    fadt.pm1a_control,
                    u64::from(fadt.pm1a_control) + bytes
                );
            }
        }
        let _ = writeln!(
            console,
            "aml: operation-region provider: {} authorized window(s), writes permission-gated",
            regions.len()
        );
        regions
    };
    let mut namespace = Namespace::with_dsdt_revision(dsdt.bytes()[8]);
    namespace.set_field_reader(&snapshot);
    namespace.set_region_reader(&regions);
    namespace.set_region_writer(&regions);
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
    // Select before any resource evaluation: even a narrowly supported _PIC may change
    // an integer referenced by _SEG/_BBN/_CRS, not only by _PRT.
    let interrupt_model_ready = if table_refusals == 0 {
        match namespace.select_interrupt_model(super::apic::is_enabled()) {
            Ok(notified) => {
                let _ = writeln!(
                    console,
                    "aml: interrupt model={}, _PIC notified={notified}",
                    if super::apic::is_enabled() {
                        "APIC"
                    } else {
                        "PIC"
                    }
                );
                true
            }
            Err(error) => {
                let _ = writeln!(console, "pci: ACPI INTx evaluation refused: _PIC {error:?}");
                false
            }
        }
    } else {
        false
    };
    if table_refusals == 0 {
        match power::root::discover(&namespace) {
            Ok(root_bridges) => {
                for bridge in root_bridges.iter() {
                    for window in bridge.resources.iter() {
                        let _ = writeln!(
                            console,
                            "aml: PCI root segment {:04x} {:?} {} {:#x}..{:#x} flags={:#x}",
                            bridge.segment,
                            window.space,
                            if window.producer {
                                "aperture"
                            } else {
                                "reservation"
                            },
                            window.start,
                            window.end,
                            window.memory_flags
                        );
                    }
                }
                // Store the segment-zero root resources for backward compatibility
                if let Some(seg0) = root_bridges.get(0) {
                    *ROOT_RESOURCES.lock() = RootResourceState::Ready(seg0.resources);
                }
                let _ = writeln!(
                    console,
                    "aml: evaluated {} PCI root bridge(s) authorized",
                    root_bridges.len()
                );
            }
            Err(error) => {
                let _ = writeln!(
                    console,
                    "aml: PCI root resource authorization refused: {error:?}"
                );
            }
        }
    } else {
        let _ = writeln!(
            console,
            "aml: PCI root resource authorization refused: incomplete table load"
        );
    }
    if interrupt_model_ready && matches!(root_resources(), RootResourceState::Ready(_)) {
        super::pci_intx::evaluate(&namespace, &snapshot, &platform.madt, console);
    }
    // The catalog keeps the root bridge's identity so consumers can tell a produced aperture
    // from a consumed reservation.
    let root = power::root::root_path(&namespace).ok();
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
            super::pci_intx::revoke();
            *ROOT_RESOURCES.lock() = RootResourceState::Refused;
            let _ = writeln!(
                console,
                "aml: PCI root resource authorization refused: firmware catalog capacity"
            );
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
            aperture: root.as_ref().is_some_and(|root| root == path),
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
        region_write_selftest(console);
    }
}

/// Exercise the operation-region **write** backend against real hardware and real refusals.
///
/// The journal that produces region-write intents is host-tested, and no method in this board
/// profile writes a region field, so without this the kernel's applying half would never run.
/// The target is the PCI configuration **address** port (0xcf8): it latches a 32-bit value and
/// reads it back, so writing the value it already holds is a real port store whose postcondition
/// is verifiable, and it changes nothing about the configuration transaction in flight. The
/// fixture drives the same transaction the evaluator feeds: exact permission, read-before-write,
/// hardware write, verified readback. It also proves the refusals that keep the backend honest:
/// a value the caller did not permit is never written, a field outside every authorized window
/// never reaches hardware, and an embedded-controller field is refused rather than guessed
/// because no ECDT or EC device authorized a port pair.
#[cfg(feature = "hal-aml-test")]
fn region_write_selftest(console: &mut dyn nolx_hal_core::console::HalConsole) {
    use nolx_power_core::aml::{
        apply_region_field_writes, Error as AmlError, MethodEffects, RegionField,
        RegionFieldReader, RegionFieldWrite, RegionFieldWriteBackend, RegionFieldWritePermission,
        RegionSpace, RegionWriteTransactionError,
    };
    const CONFIG_ADDRESS: u64 = 0x0cf8;
    let mut regions = super::aml_region::FirmwareRegions::empty();
    assert!(
        regions.authorize_io(CONFIG_ADDRESS, 4),
        "the fixture port window was refused"
    );
    let field = RegionField {
        space: RegionSpace::SystemIo,
        address: CONFIG_ADDRESS,
        bits: 32,
        shift: 0,
        access: 4,
    };
    // The value written is the value the port already holds, so the transaction is a real store
    // with a real readback and cannot disturb the address the transport latched.
    let current = RegionFieldReader::read(&regions, RegionSpace::SystemIo, CONFIG_ADDRESS, 4)
        .expect("the configuration address port did not answer");
    let effects = MethodEffects {
        result: None,
        reads: alloc::vec::Vec::new(),
        writes: alloc::vec::Vec::new(),
        region_reads: alloc::vec::Vec::new(),
        region_writes: alloc::vec![RegionFieldWrite {
            field,
            value: current
        }],
    };
    let permission =
        RegionFieldWritePermission::from_values(field, &[current]).expect("permission");
    assert_eq!(
        apply_region_field_writes(&effects, &[permission], &mut regions),
        Ok(())
    );
    let readback = RegionFieldReader::read(&regions, RegionSpace::SystemIo, CONFIG_ADDRESS, 4)
        .expect("readback refused");
    assert_eq!(
        readback, current,
        "the region write did not survive readback"
    );

    // A value the caller did not permit is refused before any hardware access.
    let denied = RegionFieldWritePermission::from_values(field, &[!current & 0xffff_ffff])
        .expect("permission");
    assert_eq!(
        apply_region_field_writes(&effects, &[denied], &mut regions),
        Err(RegionWriteTransactionError::PermissionDenied)
    );

    // A field outside every authorized window never reaches the port.
    let outside = RegionField {
        address: CONFIG_ADDRESS + 0x10,
        ..field
    };
    let outside_effects = MethodEffects {
        region_writes: alloc::vec![RegionFieldWrite {
            field: outside,
            value: current
        }],
        ..effects.clone()
    };
    let outside_permission =
        RegionFieldWritePermission::from_values(outside, &[current]).expect("permission");
    // The transaction reads before it writes, so the refusal surfaces at the read.
    assert_eq!(
        apply_region_field_writes(&outside_effects, &[outside_permission], &mut regions),
        Err(RegionWriteTransactionError::Read(AmlError::Unsupported(
            0x5b80
        )))
    );

    // No embedded controller is authorized, so an EC field is refused rather than defaulted.
    let ec_field = RegionField {
        space: RegionSpace::EmbeddedControl,
        address: 0x00,
        ..field
    };
    let mut ec = super::aml_region::FirmwareRegions::empty();
    assert_eq!(
        RegionFieldReader::read(&ec, RegionSpace::EmbeddedControl, 0, 1),
        Err(AmlError::Unsupported(0x5b82))
    );
    assert_eq!(
        RegionFieldWriteBackend::write_field(&mut ec, ec_field, 1),
        Err(AmlError::Unsupported(0x5b82))
    );
    let _ = writeln!(
        console,
        "aml: region-write selftest: authorized port write applied and read back ({current:#010x}), unpermitted value denied without a write, outside-window write refused, unauthorized embedded controller refused"
    );
}
