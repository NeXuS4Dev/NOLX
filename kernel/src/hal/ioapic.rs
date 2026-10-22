//! I/O APIC setup for the legacy IRQ lines described by the ACPI MADT.

use core::ptr;
#[cfg(feature = "hal-e1000-recovery-test")]
use core::sync::atomic::AtomicBool;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::sync::IrqLock;
use nolx_hal_core::acpi::{
    InterruptOverride, IoApicNmiSource, Madt, MAX_IO_APICS, MAX_IO_APIC_NMIS,
};

use super::irq_vectors::{self, VectorKind};

const IOAPIC_REGISTER_WINDOW: usize = 0x10;
const IOAPIC_VERSION: u8 = 0x01;
const IOAPIC_REDIR_BASE: u8 = 0x10;
const REDIRECTION_MASKED: u32 = 1 << 16;
const REDIRECTION_ACTIVE_LOW: u32 = 1 << 13;
const REDIRECTION_LEVEL_TRIGGERED: u32 = 1 << 15;
const REDIRECTION_NMI: u32 = 4 << 8;
const VECTOR_BASE: u8 = 32;
const LEGACY_IRQS: usize = 16;
const STANDARD_MMIO_START: u64 = 0xFEC0_0000;
const STANDARD_MMIO_END: u64 = 0xFEE0_0000;
const MAX_REDIRECTIONS: u32 = 120;

static IRQ_ROUTE_BASE: [AtomicU64; LEGACY_IRQS] = [const { AtomicU64::new(0) }; LEGACY_IRQS];
static IRQ_ROUTE_LOW_REGISTER: [AtomicU32; LEGACY_IRQS] =
    [const { AtomicU32::new(0) }; LEGACY_IRQS];
static NMI_ROUTE_BASE: [AtomicU64; MAX_IO_APIC_NMIS] =
    [const { AtomicU64::new(0) }; MAX_IO_APIC_NMIS];
static NMI_ROUTE_LOW_REGISTER: [AtomicU32; MAX_IO_APIC_NMIS] =
    [const { AtomicU32::new(0) }; MAX_IO_APIC_NMIS];

const PCI_GSIS: usize = 24;
const PCI_REDIRECTION_WRITABLE: u32 = 0x1afff;
const E1000_INTX_KIND: VectorKind = VectorKind::E1000Intx;
static PCI_BASE: [AtomicU64; PCI_GSIS] = [const { AtomicU64::new(0) }; PCI_GSIS];
static PCI_REGISTER: [AtomicU32; PCI_GSIS] = [const { AtomicU32::new(0) }; PCI_GSIS];
static PCI_DESTINATION: AtomicU32 = AtomicU32::new(0);
/// Readback-verified unmasked low words, indexed by their shared GSI route.
static PCI_PROGRAMMED: [AtomicU32; PCI_GSIS] = [const { AtomicU32::new(0) }; PCI_GSIS];
/// A vector retained after a failed route transaction whose remask or owner release was not
/// confirmed; it prevents another acquisition from claiming a second vector for this GSI.
static PCI_QUARANTINED_VECTOR: [AtomicU32; PCI_GSIS] = [const { AtomicU32::new(0) }; PCI_GSIS];
/// Number of live adapter sources sharing each programmed route.
static PCI_REFERENCES: [AtomicU32; PCI_GSIS] = [const { AtomicU32::new(0) }; PCI_GSIS];
/// Serializes route-vector ownership, shared references and I/O APIC programming.
static PCI_ROUTE_LOCK: IrqLock = IrqLock::new();

#[cfg(feature = "hal-e1000-recovery-test")]
static FAIL_NEXT_PCI_PROGRAMMED_READBACK: AtomicBool = AtomicBool::new(false);
#[cfg(feature = "hal-e1000-recovery-test")]
static FAIL_NEXT_PCI_REMASK_READBACK: AtomicBool = AtomicBool::new(false);

/// Failure while validating or programming the I/O APICs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoApicError {
    /// Firmware did not describe an I/O APIC.
    Missing,
    /// An I/O APIC address is outside the early uncached MMIO window.
    UnmappedAddress,
    /// Hardware reported more redirection entries than the early selector interface supports.
    TooManyRedirections,
    /// The BSP's APIC ID cannot be represented by the physical destination field.
    UnsupportedDestination,
    /// The ISA override contains reserved polarity or trigger encodings.
    InvalidOverride,
    /// I/O APIC records overlap in MMIO address or global-system-interrupt coverage.
    ConflictingTopology,
    /// Reinitialization would erase active or quarantined PCI route ownership.
    RouteStateActive,
    /// An NMI-source record contains reserved polarity or trigger encodings.
    InvalidNmiSource,
    /// An NMI source names a GSI not covered by an I/O APIC.
    MissingNmiRoute,
    /// No I/O APIC covers legacy IRQ0, required by the PIT fallback.
    MissingTimerRoute,
}

#[derive(Clone, Copy)]
struct IoApicState {
    base: u64,
    gsi_start: u32,
    gsi_end: u32,
}

#[derive(Clone, Copy)]
struct LegacyRoute {
    base: u64,
    low_register: u8,
    low_value: u32,
}

#[derive(Clone, Copy)]
struct NmiRoute {
    base: u64,
    low_register: u8,
    low_value: u32,
}

/// Mask all I/O APIC inputs and program masked routes for legacy IRQ vectors 32–47.
///
/// IRQ0 is required because it remains the timer fallback if neither calibrated LAPIC nor
/// TSC-deadline mode can be selected. Other ISA routes stay masked until a driver unmasks them.
pub fn init(madt: &Madt, destination_apic_id: u32) -> Result<(), IoApicError> {
    if destination_apic_id > u8::MAX as u32 {
        return Err(IoApicError::UnsupportedDestination);
    }
    if PCI_PROGRAMMED
        .iter()
        .any(|entry| entry.load(Ordering::Acquire) != 0)
        || PCI_REFERENCES
            .iter()
            .any(|entry| entry.load(Ordering::Acquire) != 0)
        || PCI_QUARANTINED_VECTOR
            .iter()
            .any(|entry| entry.load(Ordering::Acquire) != 0)
    {
        // Reinitialization may not erase live or quarantined route ownership.
        return Err(IoApicError::RouteStateActive);
    }
    let descriptions = madt.io_apics();
    if descriptions.is_empty() {
        return Err(IoApicError::Missing);
    }
    if descriptions.len() > MAX_IO_APICS {
        return Err(IoApicError::TooManyRedirections);
    }

    let mut states = [IoApicState {
        base: 0,
        gsi_start: 0,
        gsi_end: 0,
    }; MAX_IO_APICS];
    for (index, description) in descriptions.iter().enumerate() {
        let base = u64::from(description.address);
        let end = base.checked_add(0x20).ok_or(IoApicError::UnmappedAddress)?;
        if base < STANDARD_MMIO_START || end > STANDARD_MMIO_END || base & 0xFFF != 0 {
            return Err(IoApicError::UnmappedAddress);
        }
        let version = ioapic_read(base, IOAPIC_VERSION);
        let redirections = ((version >> 16) & 0xFF) + 1;
        if redirections > MAX_REDIRECTIONS {
            return Err(IoApicError::TooManyRedirections);
        }
        let gsi_end = description
            .global_interrupt_base
            .checked_add(redirections - 1)
            .ok_or(IoApicError::TooManyRedirections)?;
        if states[..index].iter().any(|previous| {
            previous.base == base
                || (description.global_interrupt_base <= previous.gsi_end
                    && gsi_end >= previous.gsi_start)
        }) {
            return Err(IoApicError::ConflictingTopology);
        }
        states[index] = IoApicState {
            base,
            gsi_start: description.global_interrupt_base,
            gsi_end,
        };
    }

    validate_overrides(madt)?;

    let mut legacy_routes = [None; LEGACY_IRQS];
    for (irq, route) in legacy_routes.iter_mut().enumerate() {
        let (gsi, flags, has_override) = override_for_irq(madt, irq as u8);
        if !has_override
            && madt.interrupt_overrides().iter().any(|entry| {
                entry.bus == 0 && entry.source != irq as u8 && entry.global_interrupt == gsi
            })
        {
            // Some firmware maps (notably IRQ0 -> GSI2) use a GSI that aliases a legacy
            // cascade input. Prefer the explicit ISA source and do not overwrite its vector
            // with the synthetic default route for the aliased IRQ.
            continue;
        }
        if madt
            .io_apic_nmi_sources()
            .iter()
            .any(|source| source.global_interrupt == gsi)
        {
            if irq == 0 {
                return Err(IoApicError::MissingTimerRoute);
            }
            continue;
        }
        let Some((ioapic, pin)) = find_route(&states, descriptions.len(), gsi) else {
            continue;
        };
        let (active_low, level_triggered) = decode_override(flags)?;
        let low_register = redirection_register(pin)?;
        let mut low_value = u32::from(VECTOR_BASE + irq as u8) | REDIRECTION_MASKED;
        if active_low {
            low_value |= REDIRECTION_ACTIVE_LOW;
        }
        if level_triggered {
            low_value |= REDIRECTION_LEVEL_TRIGGERED;
        }
        *route = Some(LegacyRoute {
            base: ioapic.base,
            low_register,
            low_value,
        });
    }
    if legacy_routes[0].is_none() {
        return Err(IoApicError::MissingTimerRoute);
    }

    let mut nmi_routes = [None; MAX_IO_APIC_NMIS];
    for (index, source) in madt.io_apic_nmi_sources().iter().enumerate() {
        if madt.io_apic_nmi_sources()[..index]
            .iter()
            .any(|previous| previous.global_interrupt == source.global_interrupt)
        {
            return Err(IoApicError::InvalidNmiSource);
        }
        let Some((ioapic, pin)) = find_route(&states, descriptions.len(), source.global_interrupt)
        else {
            return Err(IoApicError::MissingNmiRoute);
        };
        let (active_low, level_triggered) =
            decode_nmi_source(source).map_err(|_| IoApicError::InvalidNmiSource)?;
        let low_register = redirection_register(pin)?;
        let mut low_value = REDIRECTION_NMI | REDIRECTION_MASKED;
        if active_low {
            low_value |= REDIRECTION_ACTIVE_LOW;
        }
        if level_triggered {
            low_value |= REDIRECTION_LEVEL_TRIGGERED;
        }
        nmi_routes[index] = Some(NmiRoute {
            base: ioapic.base,
            low_register,
            low_value,
        });
    }

    for irq in 0..LEGACY_IRQS {
        IRQ_ROUTE_BASE[irq].store(0, Ordering::Relaxed);
        IRQ_ROUTE_LOW_REGISTER[irq].store(0, Ordering::Relaxed);
    }
    for index in 0..MAX_IO_APIC_NMIS {
        NMI_ROUTE_BASE[index].store(0, Ordering::Relaxed);
        NMI_ROUTE_LOW_REGISTER[index].store(0, Ordering::Relaxed);
    }
    for state in &states[..descriptions.len()] {
        let redirections = state.gsi_end - state.gsi_start + 1;
        for pin in 0..redirections {
            let low = redirection_register(pin)?;
            ioapic_write(state.base, low + 1, 0);
            ioapic_write(state.base, low, REDIRECTION_MASKED);
        }
    }
    for (irq, route) in legacy_routes.iter().enumerate() {
        let Some(route) = route else {
            continue;
        };
        ioapic_write(
            route.base,
            route.low_register + 1,
            destination_apic_id << 24,
        );
        ioapic_write(route.base, route.low_register, route.low_value);
        IRQ_ROUTE_BASE[irq].store(route.base, Ordering::Relaxed);
        IRQ_ROUTE_LOW_REGISTER[irq].store(u32::from(route.low_register), Ordering::Relaxed);
    }
    for (index, route) in nmi_routes.iter().enumerate() {
        let Some(route) = route else {
            continue;
        };
        // ACPI NMI-source records have no processor destination; physical APIC broadcast is
        // therefore required. The 0xFF destination is the architectural all-processors value.
        ioapic_write(route.base, route.low_register + 1, u32::from(u8::MAX) << 24);
        ioapic_write(route.base, route.low_register, route.low_value);
        NMI_ROUTE_BASE[index].store(route.base, Ordering::Relaxed);
        NMI_ROUTE_LOW_REGISTER[index].store(u32::from(route.low_register), Ordering::Relaxed);
    }
    // Retain only non-ISA/non-NMI GSIs in the bounded native PCI vector domain.
    PCI_DESTINATION.store(destination_apic_id << 24, Ordering::Relaxed);
    for gsi in 0..PCI_GSIS {
        PCI_BASE[gsi].store(0, Ordering::Relaxed);
        PCI_PROGRAMMED[gsi].store(0, Ordering::Relaxed);
        PCI_QUARANTINED_VECTOR[gsi].store(0, Ordering::Relaxed);
        PCI_REFERENCES[gsi].store(0, Ordering::Relaxed);
        if gsi < 3
            || gsi == 12
            || madt
                .io_apic_nmi_sources()
                .iter()
                .any(|n| n.global_interrupt == gsi as u32)
        {
            continue;
        }
        let Some((state, pin)) = find_route(&states, descriptions.len(), gsi as u32) else {
            continue;
        };
        let register = redirection_register(pin)?;
        if [0, 1, 12].iter().any(|irq| {
            legacy_routes[*irq].is_some_and(|r| r.base == state.base && r.low_register == register)
        }) {
            continue;
        }
        PCI_REGISTER[gsi].store(u32::from(register), Ordering::Relaxed);
        PCI_BASE[gsi].store(state.base, Ordering::Release);
    }
    Ok(())
}

/// Unmask a legacy IRQ already described by the initialized MADT routes.
#[must_use]
pub fn unmask_legacy_irq(irq: u8) -> bool {
    let index = usize::from(irq);
    if index >= LEGACY_IRQS {
        return false;
    }
    let base = IRQ_ROUTE_BASE[index].load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    let register = IRQ_ROUTE_LOW_REGISTER[index].load(Ordering::Relaxed) as u8;
    let value = ioapic_read(base, register) & !REDIRECTION_MASKED;
    ioapic_write(base, register, value);
    true
}

/// Whether [`acquire_pci_route`] could program this GSI: it must lie in the bounded PCI
/// domain, belong to a routed I/O APIC pin, and avoid the ISA/NMI reservations that the
/// initialization pass excluded. Route planning consults this before a firmware link is
/// steered, so a GSI the platform cannot deliver is never selected in the first place.
pub(crate) fn pci_route_supported(gsi: u32) -> bool {
    let Ok(index) = usize::try_from(gsi) else {
        return false;
    };
    PCI_BASE
        .get(index)
        .is_some_and(|base| base.load(Ordering::Acquire) != 0)
}

/// Why a route could not be acquired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RouteError {
    /// The GSI is outside the bounded PCI domain, unrouted, reserved, quarantined, or its
    /// current programming does not read back as the expected value.
    Unavailable,
    /// The route itself is usable but every vector in its kind's pool is owned. This is the
    /// one refusal an exhausted allocator produces, and drivers report it distinctly so a
    /// start that failed for lack of a vector is never mistaken for a broken route.
    NoVector,
}

/// Acquire the readback-verified current firmware route for one e1000 source.
///
/// The route is keyed by GSI: adapters sharing the same GSI share one local-APIC vector,
/// and its reference count keeps the route alive until the last adapter has stopped. A new
/// route takes a dynamically claimed vector before the I/O APIC is unmasked. Unsupported,
/// conflicting or unverified routes are refused. Failed programming is remasked and checked;
/// if the mask cannot be confirmed, the vector is retained and the GSI quarantined so a later
/// acquisition cannot claim a second vector for the uncertain route.
pub fn acquire_pci_route(irq: nolx_power_core::routing::Interrupt) -> Result<u8, RouteError> {
    let index = usize::try_from(irq.gsi).map_err(|_| RouteError::Unavailable)?;
    if index >= PCI_GSIS || !irq.level || !irq.shared {
        return Err(RouteError::Unavailable);
    }
    let _guard = PCI_ROUTE_LOCK.lock();
    let base = PCI_BASE[index].load(Ordering::Acquire);
    if base == 0 {
        return Err(RouteError::Unavailable);
    }
    let register = PCI_REGISTER[index].load(Ordering::Relaxed) as u8;
    let destination = PCI_DESTINATION.load(Ordering::Relaxed);
    let references = PCI_REFERENCES[index].load(Ordering::Relaxed);
    let programmed = PCI_PROGRAMMED[index].load(Ordering::Relaxed);

    if PCI_QUARANTINED_VECTOR[index].load(Ordering::Acquire) != 0 {
        // Ownership from an earlier failed transaction is unverified. Keep its claim and refuse
        // this GSI rather than leaking another vector on each retry.
        return Err(RouteError::Unavailable);
    }
    if references != 0 {
        let vector = programmed as u8;
        let expected = pci_route_value(vector, irq);
        if programmed != expected
            || irq_vectors::owner(vector) != Some((E1000_INTX_KIND, index))
            || ioapic_read(base, register) & PCI_REDIRECTION_WRITABLE != expected
            || ioapic_read(base, register + 1) & 0xff000000 != destination
        {
            return Err(RouteError::Unavailable);
        }
        let references = references.checked_add(1).ok_or(RouteError::Unavailable)?;
        PCI_REFERENCES[index].store(references, Ordering::Release);
        return Ok(vector);
    }
    if programmed != 0 {
        // A nonempty route without a reference is stale or inconsistent; never overwrite it.
        return Err(RouteError::Unavailable);
    }

    let vector = irq_vectors::claim(E1000_INTX_KIND, index).ok_or(RouteError::NoVector)?;
    let value = pci_route_value(vector, irq);
    let masked = value | REDIRECTION_MASKED;
    ioapic_write(
        base,
        register,
        ioapic_read(base, register) | REDIRECTION_MASKED,
    );
    if ioapic_read(base, register) & REDIRECTION_MASKED == 0 {
        retain_or_release_failed_pci_route(index, base, register, vector, masked);
        return Err(RouteError::Unavailable);
    }
    ioapic_write(base, register + 1, destination);
    ioapic_write(base, register, masked);
    if !pci_route_programmed_readback(base, register, masked)
        || ioapic_read(base, register + 1) & 0xff000000 != destination
    {
        retain_or_release_failed_pci_route(index, base, register, vector, masked);
        return Err(RouteError::Unavailable);
    }
    ioapic_write(base, register, value);
    if ioapic_read(base, register) & PCI_REDIRECTION_WRITABLE != value {
        retain_or_release_failed_pci_route(index, base, register, vector, masked);
        return Err(RouteError::Unavailable);
    }
    PCI_PROGRAMMED[index].store(value, Ordering::Relaxed);
    PCI_REFERENCES[index].store(1, Ordering::Release);
    Ok(vector)
}

/// A failed transaction can relinquish its vector only after the route's mask bit reads back
/// and the matching owner entry releases. Otherwise the claim and per-GSI marker are retained,
/// preventing a retry from allocating a second vector while route state is uncertain.
fn retain_or_release_failed_pci_route(
    index: usize,
    base: u64,
    register: u8,
    vector: u8,
    masked: u32,
) {
    ioapic_write(base, register, masked);
    if pci_route_masked_readback(base, register)
        && irq_vectors::release(vector, E1000_INTX_KIND, index)
    {
        return;
    }
    PCI_QUARANTINED_VECTOR[index].store(u32::from(vector), Ordering::Release);
}

fn pci_route_programmed_readback(base: u64, register: u8, expected: u32) -> bool {
    #[cfg(feature = "hal-e1000-recovery-test")]
    if FAIL_NEXT_PCI_PROGRAMMED_READBACK.swap(false, Ordering::AcqRel) {
        return false;
    }
    ioapic_read(base, register) & PCI_REDIRECTION_WRITABLE == expected
}

fn pci_route_masked_readback(base: u64, register: u8) -> bool {
    #[cfg(feature = "hal-e1000-recovery-test")]
    if FAIL_NEXT_PCI_REMASK_READBACK.swap(false, Ordering::AcqRel) {
        return false;
    }
    ioapic_read(base, register) & REDIRECTION_MASKED != 0
}

#[cfg(feature = "hal-e1000-recovery-test")]
/// Inject a failed route readback followed by an unconfirmed retry-mask readback.
pub(crate) fn inject_pci_route_remask_readback_failure() {
    FAIL_NEXT_PCI_PROGRAMMED_READBACK.store(true, Ordering::Release);
    FAIL_NEXT_PCI_REMASK_READBACK.store(true, Ordering::Release);
}

/// Release one adapter's reference after its device and PCI INTx source have been proven quiet.
/// The final reference masks and readbacks the I/O APIC entry before returning the vector.
pub fn release_pci_route(irq: nolx_power_core::routing::Interrupt, vector: u8) -> bool {
    let Ok(index) = usize::try_from(irq.gsi) else {
        return false;
    };
    if index >= PCI_GSIS || !irq.level || !irq.shared {
        return false;
    }
    let _guard = PCI_ROUTE_LOCK.lock();
    let base = PCI_BASE[index].load(Ordering::Acquire);
    if base == 0 {
        return false;
    }
    let register = PCI_REGISTER[index].load(Ordering::Relaxed) as u8;
    let destination = PCI_DESTINATION.load(Ordering::Relaxed);
    let references = PCI_REFERENCES[index].load(Ordering::Relaxed);
    let expected = pci_route_value(vector, irq);
    if references == 0
        || PCI_PROGRAMMED[index].load(Ordering::Relaxed) != expected
        || irq_vectors::owner(vector) != Some((E1000_INTX_KIND, index))
        || ioapic_read(base, register) & PCI_REDIRECTION_WRITABLE != expected
        || ioapic_read(base, register + 1) & 0xff000000 != destination
    {
        return false;
    }
    if references > 1 {
        PCI_REFERENCES[index].store(references - 1, Ordering::Release);
        return true;
    }

    let masked = expected | REDIRECTION_MASKED;
    ioapic_write(base, register, masked);
    if ioapic_read(base, register) & PCI_REDIRECTION_WRITABLE != masked {
        return false;
    }
    if !irq_vectors::release(vector, E1000_INTX_KIND, index) {
        return false;
    }
    PCI_PROGRAMMED[index].store(0, Ordering::Relaxed);
    PCI_REFERENCES[index].store(0, Ordering::Release);
    true
}

fn pci_route_value(vector: u8, irq: nolx_power_core::routing::Interrupt) -> u32 {
    u32::from(vector)
        | REDIRECTION_LEVEL_TRIGGERED
        | if irq.active_low {
            REDIRECTION_ACTIVE_LOW
        } else {
            0
        }
}

#[cfg(feature = "hal-e1000-recovery-test")]
/// The active reference count of one supported GSI route, for native lifecycle assertions.
pub(crate) fn pci_route_references(gsi: u32) -> usize {
    usize::try_from(gsi)
        .ok()
        .and_then(|index| PCI_REFERENCES.get(index))
        .map(|references| references.load(Ordering::Acquire) as usize)
        .unwrap_or(0)
}

#[cfg(feature = "hal-e1000-recovery-test")]
/// The vector retained for an unverified route transaction, if that GSI is quarantined.
pub(crate) fn pci_route_quarantined_vector(gsi: u32) -> Option<u8> {
    let index = usize::try_from(gsi).ok()?;
    let vector = PCI_QUARANTINED_VECTOR.get(index)?.load(Ordering::Acquire);
    if vector == 0 {
        None
    } else {
        u8::try_from(vector).ok()
    }
}

/// Unmask MADT NMI-source routes after the local APIC and NMI IDT path are ready.
pub fn unmask_nmi_sources() {
    for index in 0..MAX_IO_APIC_NMIS {
        let base = NMI_ROUTE_BASE[index].load(Ordering::Acquire);
        if base == 0 {
            continue;
        }
        let register = NMI_ROUTE_LOW_REGISTER[index].load(Ordering::Relaxed) as u8;
        let value = ioapic_read(base, register) & !REDIRECTION_MASKED;
        ioapic_write(base, register, value);
    }
}

/// Mask a previously initialized legacy IRQ route.
pub fn mask_legacy_irq(irq: u8) {
    let index = usize::from(irq);
    if index >= LEGACY_IRQS {
        return;
    }
    let base = IRQ_ROUTE_BASE[index].load(Ordering::Acquire);
    if base == 0 {
        return;
    }
    let register = IRQ_ROUTE_LOW_REGISTER[index].load(Ordering::Relaxed) as u8;
    let value = ioapic_read(base, register) | REDIRECTION_MASKED;
    ioapic_write(base, register, value);
}

fn validate_overrides(madt: &Madt) -> Result<(), IoApicError> {
    let overrides = madt.interrupt_overrides();
    for (index, entry) in overrides.iter().enumerate() {
        if entry.bus != 0 {
            continue;
        }
        if usize::from(entry.source) >= LEGACY_IRQS {
            return Err(IoApicError::InvalidOverride);
        }
        decode_override(entry.flags)?;
        if overrides[..index].iter().any(|previous| {
            previous.bus == 0
                && (previous.source == entry.source
                    || previous.global_interrupt == entry.global_interrupt)
        }) {
            return Err(IoApicError::ConflictingTopology);
        }
    }
    Ok(())
}

fn override_for_irq(madt: &Madt, irq: u8) -> (u32, u16, bool) {
    madt.interrupt_overrides()
        .iter()
        .find(|entry: &&InterruptOverride| entry.bus == 0 && entry.source == irq)
        .map(|entry| (entry.global_interrupt, entry.flags, true))
        .unwrap_or((u32::from(irq), 0, false))
}

fn find_route(states: &[IoApicState], count: usize, gsi: u32) -> Option<(IoApicState, u32)> {
    states[..count]
        .iter()
        .copied()
        .find(|state| (state.gsi_start..=state.gsi_end).contains(&gsi))
        .map(|state| (state, gsi - state.gsi_start))
}

fn decode_override(flags: u16) -> Result<(bool, bool), IoApicError> {
    if flags & !0x000F != 0 {
        return Err(IoApicError::InvalidOverride);
    }
    let active_low = match flags & 0b11 {
        0 | 1 => false,
        3 => true,
        _ => return Err(IoApicError::InvalidOverride),
    };
    let level_triggered = match (flags >> 2) & 0b11 {
        0 | 1 => false,
        3 => true,
        _ => return Err(IoApicError::InvalidOverride),
    };
    Ok((active_low, level_triggered))
}

fn decode_nmi_source(source: &IoApicNmiSource) -> Result<(bool, bool), IoApicError> {
    decode_override(source.flags)
}

fn redirection_register(pin: u32) -> Result<u8, IoApicError> {
    let register = u32::from(IOAPIC_REDIR_BASE) + 2 * pin;
    u8::try_from(register).map_err(|_| IoApicError::TooManyRedirections)
}

fn ioapic_read(base: u64, register: u8) -> u32 {
    // SAFETY: callers validate `base` against the statically mapped UC I/O APIC window.
    unsafe {
        ptr::write_volatile(
            crate::mm::phys::direct_ptr::<u32>(base),
            u32::from(register),
        );
        ptr::read_volatile(crate::mm::phys::direct_ptr::<u32>(
            base + IOAPIC_REGISTER_WINDOW as u64,
        ))
    }
}

fn ioapic_write(base: u64, register: u8, value: u32) {
    // SAFETY: callers validate `base` against the statically mapped UC I/O APIC window.
    unsafe {
        ptr::write_volatile(
            crate::mm::phys::direct_ptr::<u32>(base),
            u32::from(register),
        );
        ptr::write_volatile(
            crate::mm::phys::direct_ptr::<u32>(base + IOAPIC_REGISTER_WINDOW as u64),
            value,
        );
    }
}
