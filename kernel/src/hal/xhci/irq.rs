//! Bounded MSI/MSI-X top half: acknowledge/mask the source and schedule idle work, never touch rings.
use super::*;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use nolx_pci_core::msi;
use usb::interrupt as rule;
const KIND: super::super::irq_vectors::VectorKind = super::super::irq_vectors::VectorKind::Xhci;
const NO_VECTOR: u8 = 0;
const _: () = assert!(driver_start::DEVICES <= 16);
#[derive(Clone, Copy)]
struct Source {
    regs: Region,
    op: u64,
    runtime: u64,
}
static SOURCES: [IrqMutex<Option<Source>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
static WORK: [AtomicBool; driver_start::DEVICES] =
    [const { AtomicBool::new(false) }; driver_start::DEVICES];
static DELIVERED: [AtomicUsize; driver_start::DEVICES] =
    [const { AtomicUsize::new(0) }; driver_start::DEVICES];
static VECTORS: [AtomicU8; driver_start::DEVICES] =
    [const { AtomicU8::new(NO_VECTOR) }; driver_start::DEVICES];
#[cfg(any(
    feature = "hal-xhci-lifecycle-test",
    feature = "hal-xhci-recovery-test",
    feature = "hal-xhci-transfer-fault-test",
    feature = "hal-xhci-start-fault-test"
))]
static LAST_VECTOR: [AtomicU8; driver_start::DEVICES] =
    [const { AtomicU8::new(NO_VECTOR) }; driver_start::DEVICES];
pub(super) fn vector(dev: usize) -> u8 {
    VECTORS[dev].load(Ordering::Acquire)
}
pub(super) fn count(dev: usize) -> usize {
    DELIVERED[dev].load(Ordering::Acquire)
}
pub(super) fn handles(vector: u8) -> bool {
    matches!(
        super::super::irq_vectors::owner(vector),
        Some((kind, dev)) if kind == KIND && dev < driver_start::DEVICES
    )
}
pub(super) fn interrupt(vector: u8) {
    let Some((kind, dev)) = super::super::irq_vectors::owner(vector) else {
        return;
    };
    if kind != KIND {
        return;
    }
    if dev >= driver_start::DEVICES || VECTORS[dev].load(Ordering::Acquire) != vector {
        return;
    }
    let source = SOURCES[dev].lock();
    let Some(source) = source.as_ref() else {
        return;
    };
    let Ok(status) = source.regs.read32(source.op + usb::OP_USBSTS) else {
        return;
    };
    let Some(ack) = rule::acknowledge(status) else {
        return;
    };
    // Mask IE until the idle consumer has drained the event ring. Never echo other W1C bits.
    if source.regs.write32(source.runtime, rule::PENDING).is_err()
        || source
            .regs
            .write32(source.op + usb::OP_USBSTS, ack)
            .is_err()
    {
        return;
    }
    DELIVERED[dev].fetch_add(1, Ordering::Release);
    WORK[dev].store(true, Ordering::Release);
}
fn disable_messages(bdf: Bdf) -> Result<bool, Failure> {
    let caps = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    if caps.truncated {
        return Err(Failure::Interrupt);
    }
    for cap in caps.as_slice() {
        let mask = match cap.id {
            msi::CAP_MSI => u32::from(msi::MSI_CTRL_ENABLE) << 16,
            msi::CAP_MSI_X => u32::from(msi::MSIX_CTRL_ENABLE) << 16,
            _ => continue,
        };
        let old = ConfigAccess.read_u32(bdf, cap.offset);
        ConfigAccess.write_u32(bdf, cap.offset, old & !mask);
        if ConfigAccess.read_u32(bdf, cap.offset) & mask != 0 {
            return Err(Failure::Interrupt);
        }
    }
    Ok(caps.find(msi::CAP_MSI).is_some() || caps.find(msi::CAP_MSI_X).is_some())
}
fn program(dev: usize, bdf: Bdf, regs: Region, vector: u8) -> Result<&'static str, Failure> {
    let caps = nolx_pci_core::capabilities(&mut ConfigAccess, bdf);
    // PCI command status is W1C: never echo the upper half while blocking legacy INTx.
    let command = ConfigAccess.read_u32(bdf, PCI_COMMAND) & 0xffff;
    ConfigAccess.write_u32(bdf, PCI_COMMAND, command | (1 << 10));
    if ConfigAccess.read_u32(bdf, PCI_COMMAND) & (1 << 10) == 0 {
        return Err(Failure::Interrupt);
    }
    if let Some(cap) = caps.find(msi::CAP_MSI_X) {
        // The full 12-byte capability must fit conventional PCI configuration space.
        if cap.offset > 0xf4 {
            return Err(Failure::Interrupt);
        }
        let header = ConfigAccess.read_u32(bdf, cap.offset);
        let control = (header >> 16) as u16;
        let layout = msi::parse_msix(
            control,
            ConfigAccess.read_u32(bdf, cap.offset + 4),
            ConfigAccess.read_u32(bdf, cap.offset + 8),
        );
        // This bounded profile uses the existing BAR0 register window, never an unowned alias.
        if layout.table_bar != 0 || layout.pba_bar != 0 || !msi::msix_fits(&layout, REGISTERS_BYTES)
        {
            return Err(Failure::Interrupt);
        }
        let parameters = regs.read32(usb::CAP_HCSPARAMS1)?;
        let interrupters = u64::from((parameters >> 8) & 0x7ff);
        let ports_end = u64::from(regs.read8(usb::CAP_CAPLENGTH)?)
            + 0x400
            + 16 * u64::from(usb::max_ports(parameters));
        let runtime_end = u64::from(regs.read32(0x18)? & !31) + 0x20 + 0x20 * interrupters;
        let doorbell_end = u64::from(regs.read32(usb::CAP_DBOFF)? & !3)
            + 4 * (1 + u64::from(usb::max_slots(parameters)));
        if interrupters == 0
            || !rule::message_table_window(
                ports_end.max(runtime_end).max(doorbell_end),
                u64::from(layout.table_offset),
                u64::from(layout.table_size) * msi::MSIX_ENTRY_BYTES,
                u64::from(layout.pba_offset),
                msi::pba_bytes(layout.table_size),
                REGISTERS_BYTES,
            )
        {
            return Err(Failure::Interrupt);
        }
        let destination =
            u8::try_from(super::super::apic::current_apic_id()).map_err(|_| Failure::Interrupt)?;
        let words = msi::msix_entry(
            u64::from(msi::message_address(destination)),
            vector,
            false,
        )
        .ok_or(Failure::Interrupt)?;
        let at = u64::from(layout.table_offset);
        regs.write32(at + 12, msi::MSIX_VECTOR_MASKED)?;
        for (i, word) in words.chunks_exact(4).enumerate() {
            let value = u32::from_le_bytes(word.try_into().unwrap());
            regs.write32(at + 4 * i as u64, value)?;
            if regs.read32(at + 4 * i as u64)? != value {
                return Err(Failure::Interrupt);
            }
        }
        let enabled = msi::msix_enable(control);
        ConfigAccess.write_u32(
            bdf,
            cap.offset,
            (header & 0xffff) | (u32::from(enabled) << 16),
        );
        if (ConfigAccess.read_u32(bdf, cap.offset) >> 16) as u16 != enabled {
            return Err(Failure::Interrupt);
        }
        Ok("MSI-X")
    } else {
        super::super::msix::enable_msi(bdf, vector).map_err(|_| Failure::Interrupt)?;
        let cap = caps.find(msi::CAP_MSI).ok_or(Failure::Interrupt)?;
        if (ConfigAccess.read_u32(bdf, cap.offset) >> 16) as u16 & msi::MSI_CTRL_ENABLE == 0 {
            return Err(Failure::Interrupt);
        }
        Ok("MSI")
    }
}
fn claim_vector(dev: usize) -> Result<u8, Failure> {
    if VECTORS[dev].load(Ordering::Acquire) != NO_VECTOR {
        return Err(Failure::Busy);
    }
    let vector = super::super::irq_vectors::claim(KIND, dev).ok_or(Failure::NoVector)?;
    VECTORS[dev].store(vector, Ordering::Release);
    Ok(vector)
}
/// Release the source's vector only after the controller stop has proved quiescence.
pub(super) fn release_vector(dev: usize) -> bool {
    let vector = VECTORS[dev].load(Ordering::Acquire);
    if vector == NO_VECTOR {
        return true;
    }
    if !super::super::irq_vectors::release(vector, KIND, dev) {
        return false;
    }
    #[cfg(any(
        feature = "hal-xhci-lifecycle-test",
        feature = "hal-xhci-recovery-test",
        feature = "hal-xhci-transfer-fault-test",
        feature = "hal-xhci-start-fault-test"
    ))]
    LAST_VECTOR[dev].store(vector, Ordering::Release);
    VECTORS[dev].store(NO_VECTOR, Ordering::Release);
    true
}
pub(super) fn arm(
    dev: usize,
    bdf: Bdf,
    regs: Region,
    op: u64,
    console: &mut dyn HalConsole,
) -> Result<(), Failure> {
    let available = disable_messages(bdf)?;
    if !super::super::apic::is_enabled() || !available {
        let _ = writeln!(
            console,
            "xhci: handle={dev} polled fallback (no APIC or message capability)"
        );
        return Ok(());
    }
    // Reserve the vector before the controller or its MSI/MSI-X table can use it.
    let vector = claim_vector(dev)?;
    let runtime = u64::from(regs.read32(0x18)? & !31) + 0x20;
    let before = count(dev);
    let transport;
    {
        let _interrupts = super::super::cpu::InterruptGuard::disable();
        WORK[dev].store(false, Ordering::Release);
        regs.write32(runtime, rule::PENDING)?;
        regs.write32(op + usb::OP_USBSTS, rule::STATUS_EVENT)?;
        *SOURCES[dev].lock() = Some(Source { regs, op, runtime });
        transport = program(dev, bdf, regs, vector)?;
        regs.write32(runtime + 4, 0)?; // no moderation delay for this bounded profile
        regs.write32(runtime, rule::ENABLE)?;
        regs.write32(
            op + usb::OP_USBCMD,
            regs.read32(op + usb::OP_USBCMD)? | rule::COMMAND_ENABLE,
        )?;
    }
    // No host lock is held while waiting for the top half. It never checks out the framework.
    live::interrupt_probe(dev)?;
    if super::super::msix::wait_for_interrupts(&DELIVERED[dev], before + 1) <= before {
        return Err(Failure::Interrupt);
    }
    #[cfg(feature = "hal-xhci-irq-test")]
    {
        // The two observations must exclude a legitimate in-flight vector between them.
        let interrupts = super::super::cpu::InterruptGuard::disable();
        assert!(work(dev).is_some(), "probe IRQ must schedule idle work");
        assert!(
            work(dev).is_none(),
            "armed sources must not fall back to timer polling"
        );
        WORK[dev].store(true, Ordering::Release); // retain the real probe's pending work
        drop(interrupts);
        let _ = writeln!(console, "xhci: IRQ-only admission verified handle={dev}");
    }
    let _ = writeln!(
        console,
        "xhci: handle={dev} {transport} vector={:#x} delivered={}, deferred HID",
        vector,
        count(dev)
    );
    #[cfg(feature = "hal-xhci-irq-start-fault-test")]
    {
        static INJECT: AtomicBool = AtomicBool::new(true);
        if INJECT.swap(false, Ordering::AcqRel) {
            let _ = writeln!(
                console,
                "xhci-test: injected post-run start failure (after armed {transport})"
            );
            return Err(Failure::Interrupt);
        }
    }
    Ok(())
}
/// Detach before MMIO/DMA release, including failed-stop quarantine. Late vectors become no-ops.
pub(super) fn disarm(dev: usize, bdf: Bdf) -> Result<(), Failure> {
    let source = SOURCES[dev].lock().take();
    WORK[dev].store(false, Ordering::Release);
    let mut result = Ok(());
    if let Some(s) = source {
        let masked = (|| {
            s.regs.write32(s.runtime, rule::PENDING)?;
            s.regs.write32(
                s.op + usb::OP_USBCMD,
                s.regs.read32(s.op + usb::OP_USBCMD)? & !rule::COMMAND_ENABLE,
            )?;
            if s.regs.read32(s.runtime)? & rule::ENABLE != 0
                || s.regs.read32(s.op + usb::OP_USBCMD)? & rule::COMMAND_ENABLE != 0
            {
                return Err(Failure::Interrupt);
            }
            Ok(())
        })();
        if masked.is_err() {
            result = masked;
        }
    }
    // Mask the controller first: disabling MSI-X while IE/INTE remain set could assert INTx.
    let pci = disable_messages(bdf).map(|_| ());
    result.and(pci)
}
/// No timer polling for an armed source: only a claimed interrupt or budget continuation admits work.
pub(super) fn work(dev: usize) -> Option<usize> {
    if SOURCES[dev].lock().is_none() {
        return Some(0);
    }
    WORK[dev].swap(false, Ordering::AcqRel).then(|| count(dev))
}
pub(super) fn finish(dev: usize, more: bool) -> Result<(), Failure> {
    if more {
        WORK[dev].store(true, Ordering::Release);
    }
    if let Some(s) = SOURCES[dev].lock().as_ref() {
        s.regs.write32(s.runtime, rule::ENABLE)?;
    }
    live::finish_events(dev)
}
#[cfg(any(
    feature = "hal-xhci-lifecycle-test",
    feature = "hal-xhci-recovery-test",
    feature = "hal-xhci-transfer-fault-test"
))]
pub(super) fn detached_test(dev: usize, retained: bool) {
    assert!(SOURCES[dev].lock().is_none());
    assert!(!WORK[dev].load(Ordering::Acquire));
    let current = vector(dev);
    let retired = if current != NO_VECTOR {
        current
    } else if retained {
        NO_VECTOR
    } else {
        LAST_VECTOR[dev].load(Ordering::Acquire)
    };
    if retained {
        if current != NO_VECTOR {
            assert_eq!(
                super::super::irq_vectors::owner(current),
                Some((KIND, dev)),
                "quarantined controller lost its vector"
            );
        }
    } else {
        assert_eq!(current, NO_VECTOR, "checked stop retained its vector");
        if retired != NO_VECTOR {
            let owner = super::super::irq_vectors::owner(retired);
            assert_ne!(
                owner,
                Some((KIND, dev)),
                "checked stop did not release its vector"
            );
            if owner.is_none() {
                // A free retired vector is safe to inject as a stale interrupt. If another
                // source has already claimed it, injection would be that source's real vector.
                let before = count(dev);
                interrupt(retired);
                assert_eq!(count(dev), before, "late vector touched retired state");
            }
        }
    }
}
