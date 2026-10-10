//! I/O APIC setup for the legacy IRQ lines described by the ACPI MADT.

use core::ptr;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use nolx_hal_core::acpi::{IoApicNmiSource, InterruptOverride, Madt, MAX_IO_APICS, MAX_IO_APIC_NMIS};

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
    for irq in 0..LEGACY_IRQS {
        let (gsi, flags, has_override) = override_for_irq(madt, irq as u8);
        if !has_override
            && madt
                .interrupt_overrides()
                .iter()
                .any(|entry| entry.bus == 0 && entry.source != irq as u8 && entry.global_interrupt == gsi)
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
        legacy_routes[irq] = Some(LegacyRoute {
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
        let Some((ioapic, pin)) = find_route(
            &states,
            descriptions.len(),
            source.global_interrupt,
        ) else {
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
        ioapic_write(route.base, route.low_register + 1, destination_apic_id << 24);
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
        ptr::write_volatile(base as usize as *mut u32, u32::from(register));
        ptr::read_volatile((base as usize + IOAPIC_REGISTER_WINDOW) as *const u32)
    }
}

fn ioapic_write(base: u64, register: u8, value: u32) {
    // SAFETY: callers validate `base` against the statically mapped UC I/O APIC window.
    unsafe {
        ptr::write_volatile(base as usize as *mut u32, u32::from(register));
        ptr::write_volatile(
            (base as usize + IOAPIC_REGISTER_WINDOW) as *mut u32,
            value,
        );
    }
}
