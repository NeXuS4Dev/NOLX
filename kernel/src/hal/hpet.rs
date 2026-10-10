//! Early HPET MMIO access and a bounded counter wait used to calibrate the local APIC.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nolx_hal_core::acpi::HpetInfo;
use nolx_hal_core::apic::hpet_frequency_hz;
use nolx_hal_core::time::extend_u32_counter;

const GENERAL_CAPABILITIES: usize = 0x000;
const GENERAL_CONFIGURATION: usize = 0x010;
const MAIN_COUNTER: usize = 0x0F0;
const ENABLE_COUNTER: u64 = 1 << 0;
const LEGACY_REPLACEMENT: u64 = 1 << 1;
const STANDARD_MMIO_START: u64 = 0xFEC0_0000;
const STANDARD_MMIO_END: u64 = 0xFEE0_0000;
const MAX_WAIT_SPINS: u32 = 50_000_000;

static HPET_BASE: AtomicU64 = AtomicU64::new(0);
static HPET_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);
static HPET_COUNTER_64BIT: AtomicBool = AtomicBool::new(false);
static HPET_LAST_EXTENDED: AtomicU64 = AtomicU64::new(0);

/// Failure while validating or initializing the firmware-described HPET.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HpetError {
    /// The table did not describe system-memory registers.
    UnsupportedAddressSpace,
    /// The register block was outside the early uncached MMIO window.
    UnmappedAddress,
    /// Hardware reported an invalid femtosecond period.
    InvalidPeriod,
}

/// Validate and start the HPET main counter, returning its measured hardware frequency.
///
/// The boot page tables map the standard HPET window uncached. Nonstandard bases are rejected
/// until the memory manager can install a dedicated uncached mapping.
pub fn init(info: &HpetInfo) -> Result<u64, HpetError> {
    if info.base.address_space_id != 0 {
        return Err(HpetError::UnsupportedAddressSpace);
    }
    let base = info.base.address;
    let end = base.checked_add(0x1000).ok_or(HpetError::UnmappedAddress)?;
    if base & 0xFFF != 0 || base < STANDARD_MMIO_START || end > STANDARD_MMIO_END {
        return Err(HpetError::UnmappedAddress);
    }

    // SAFETY: the physical address was constrained to the UC HPET page-table window and
    // the mapped device exposes an aligned 64-bit General Capabilities register at offset 0.
    let capabilities = unsafe { mmio_read64(base, GENERAL_CAPABILITIES) };
    let period_fs = (capabilities >> 32) as u32;
    let frequency = hpet_frequency_hz(period_fs).ok_or(HpetError::InvalidPeriod)?;
    if frequency == 0 {
        return Err(HpetError::InvalidPeriod);
    }
    let counter_64bit = capabilities & (1 << 13) != 0;

    // SAFETY: `base` remains the validated HPET MMIO block. Configuration and counter offsets
    // are architecturally defined and 64-bit aligned; mask legacy replacement before start.
    unsafe {
        let config = mmio_read64(base, GENERAL_CONFIGURATION);
        mmio_write64(
            base,
            GENERAL_CONFIGURATION,
            config & !(ENABLE_COUNTER | LEGACY_REPLACEMENT),
        );
        if counter_64bit {
            mmio_write64(base, MAIN_COUNTER, 0);
        } else {
            mmio_write32(base, MAIN_COUNTER, 0);
        }
        mmio_write64(
            base,
            GENERAL_CONFIGURATION,
            (config & !LEGACY_REPLACEMENT) | ENABLE_COUNTER,
        );
    }

    HPET_COUNTER_64BIT.store(counter_64bit, Ordering::Relaxed);
    HPET_LAST_EXTENDED.store(0, Ordering::Relaxed);
    HPET_FREQUENCY_HZ.store(frequency, Ordering::Relaxed);
    HPET_BASE.store(base, Ordering::Release);
    Ok(frequency)
}

/// HPET main-counter frequency, or `None` before initialization.
#[must_use]
pub fn frequency_hz() -> Option<u64> {
    let frequency = HPET_FREQUENCY_HZ.load(Ordering::Acquire);
    (frequency != 0).then_some(frequency)
}

/// Whether the initialized HPET main counter is 64 bits wide.
#[must_use]
pub fn counter_is_64bit() -> bool {
    HPET_BASE.load(Ordering::Acquire) != 0 && HPET_COUNTER_64BIT.load(Ordering::Relaxed)
}

/// Elapsed HPET ticks between two raw counter values, accounting for 32-bit wrap.
#[must_use]
pub fn elapsed_ticks(start: u64, end: u64) -> u64 {
    if counter_is_64bit() {
        end.wrapping_sub(start)
    } else {
        u64::from((end as u32).wrapping_sub(start as u32))
    }
}

/// Read the current HPET main counter, or `None` before initialization.
#[must_use]
pub fn counter() -> Option<u64> {
    let base = HPET_BASE.load(Ordering::Acquire);
    if base == 0 {
        return None;
    }
    if HPET_COUNTER_64BIT.load(Ordering::Relaxed) {
        // SAFETY: `HPET_BASE` is published only after validating the mapped 64-bit counter.
        Some(unsafe { mmio_read64(base, MAIN_COUNTER) })
    } else {
        // SAFETY: a 32-bit HPET main counter must be accessed as a 32-bit MMIO register.
        Some(u64::from(unsafe { mmio_read32(base, MAIN_COUNTER) }))
    }
}

/// Observe the 32-bit counter so periodic clock interrupts keep wrap extension current.
pub fn maintain_counter_epoch() {
    if counter_is_64bit() {
        return;
    }
    if let Some(raw) = counter() {
        let _ = extend_counter(raw as u32);
    }
}

/// Read the HPET-backed monotonic clock in nanoseconds, extending a 32-bit counter across wraps.
#[must_use]
pub fn monotonic_ns() -> Option<u64> {
    let frequency = frequency_hz()?;
    let raw = counter()?;
    let ticks = if counter_is_64bit() {
        raw
    } else {
        extend_counter(raw as u32)
    };
    Some(
        (u128::from(ticks) * 1_000_000_000 / u128::from(frequency))
            .min(u128::from(u64::MAX)) as u64,
    )
}

fn extend_counter(raw: u32) -> u64 {
    let mut previous = HPET_LAST_EXTENDED.load(Ordering::Relaxed);
    loop {
        let next = extend_u32_counter(previous, raw);
        if next == previous {
            return previous;
        }
        match HPET_LAST_EXTENDED.compare_exchange_weak(
            previous,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(observed) => previous = observed,
        }
    }
}

/// Wait until at least `ticks` have elapsed since `start`; return the ending count.
#[must_use]
pub fn wait_until_elapsed(start: u64, ticks: u64) -> Option<u64> {
    if HPET_BASE.load(Ordering::Acquire) == 0 {
        return None;
    }
    let mut previous = start;
    let mut elapsed = 0u64;
    let mut remaining = MAX_WAIT_SPINS;
    while remaining != 0 {
        let current = counter()?;
        elapsed = elapsed.saturating_add(elapsed_ticks(previous, current));
        if elapsed >= ticks {
            return Some(current);
        }
        previous = current;
        core::hint::spin_loop();
        remaining -= 1;
    }
    None
}

/// Wait for at least `ticks` HPET counter increments; return false if the counter stops.
#[must_use]
pub fn wait_ticks(ticks: u64) -> bool {
    counter()
        .and_then(|start| wait_until_elapsed(start, ticks))
        .is_some()
}

unsafe fn mmio_read64(base: u64, offset: usize) -> u64 {
    let address = (base as usize + offset) as *const u64;
    // SAFETY: callers validate that the aligned address belongs to the mapped UC HPET block.
    unsafe { core::ptr::read_volatile(address) }
}

unsafe fn mmio_read32(base: u64, offset: usize) -> u32 {
    let address = (base as usize + offset) as *const u32;
    // SAFETY: callers validate that the aligned address belongs to the mapped UC HPET block.
    unsafe { core::ptr::read_volatile(address) }
}

unsafe fn mmio_write32(base: u64, offset: usize, value: u32) {
    let address = (base as usize + offset) as *mut u32;
    // SAFETY: callers validate that the aligned address belongs to the mapped UC HPET block.
    unsafe { core::ptr::write_volatile(address, value) }
}

unsafe fn mmio_write64(base: u64, offset: usize, value: u64) {
    let address = (base as usize + offset) as *mut u64;
    // SAFETY: callers validate that the aligned address belongs to the mapped UC HPET block.
    unsafe { core::ptr::write_volatile(address, value) }
}
