//! NVMe controller rules, as far as they can be decided without touching a device.
//!
//! An NVMe controller exposes its registers through the memory BAR named by PCI class 01:08:02.
//! Which register is which, how the controller is enabled, how queue entries are laid out, how a
//! completion says it succeeded, and how a namespace's block size is read are rules over bytes.
//! They live here, host-tested. The kernel maps the registers, allocates the queue memory, and
//! polls.
//!
//! Scope: one admin queue, one I/O queue pair, polled completions, and no interrupts. Namespaces
//! other than 1, multiple paths, and power management are not here.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// Controller capabilities (64-bit). Read-only.
pub const REG_CAP: u64 = 0x00;
/// Controller configuration (32-bit).
pub const REG_CC: u64 = 0x14;
/// Controller status (32-bit). Read-only.
pub const REG_CSTS: u64 = 0x1C;
/// Admin queue attributes (32-bit): the admin submission and completion queue sizes.
pub const REG_AQA: u64 = 0x24;
/// Admin submission queue base address (64-bit).
pub const REG_ASQ: u64 = 0x28;
/// Admin completion queue base address (64-bit).
pub const REG_ACQ: u64 = 0x30;
/// Offset of the first doorbell register.
pub const DOORBELL_BASE: u64 = 0x1000;
/// Bytes in one submission queue entry.
pub const SQE_BYTES: usize = 64;
/// Bytes in one completion queue entry.
pub const CQE_BYTES: usize = 16;

/// CC.EN: the controller is enabled when this bit is set.
pub const CC_EN: u32 = 1 << 0;
/// CSTS.RDY: the controller is ready when this bit is set.
pub const CSTS_RDY: u32 = 1 << 0;
/// The submission queue entry size exponent for CC.IOSQES: 2^6 = 64 bytes.
pub const IOSQES: u32 = 6;
/// The completion queue entry size exponent for CC.IOCQES: 2^4 = 16 bytes.
pub const IOCQES: u32 = 4;

/// Admin command: Create I/O Completion Queue.
pub const OP_CREATE_IO_CQ: u8 = 0x05;
/// Admin command: Create I/O Submission Queue.
pub const OP_CREATE_IO_SQ: u8 = 0x01;
/// Admin command: Identify.
pub const OP_IDENTIFY: u8 = 0x06;
/// I/O command: Write.
pub const OP_WRITE: u8 = 0x01;
/// I/O command: Read.
pub const OP_READ: u8 = 0x02;

/// Identify CNS value that asks for a namespace's data structure.
pub const CNS_NAMESPACE: u32 = 0x00;
/// Identify CNS value that asks for the controller's data structure.
pub const CNS_CONTROLLER: u32 = 0x01;

/// The most entries a queue can have: the queue size field is 16 bits.
pub const QUEUE_ENTRIES_MAX: u16 = 4096;

/// The maximum queue entries the controller supports, from CAP.MQES (zero-based in the field).
#[must_use]
pub fn cap_max_queue_entries(cap: u64) -> u32 {
    (cap & 0xFFFF) as u32 + 1
}

/// The doorbell stride exponent, from CAP.DSTRD. Each doorbell is `4 << stride` bytes apart.
#[must_use]
pub fn cap_doorbell_stride(cap: u64) -> u32 {
    ((cap >> 32) & 0xF) as u32
}

/// The ready timeout in milliseconds, from CAP.TO, which counts 500 ms units.
#[must_use]
pub fn cap_timeout_ms(cap: u64) -> u32 {
    ((cap >> 24) & 0xFF) as u32 * 500
}

/// The offset of a submission queue's tail doorbell.
#[must_use]
pub fn sq_doorbell(queue: u16, stride: u32) -> u64 {
    DOORBELL_BASE + (2 * u64::from(queue)) * (4u64 << stride)
}

/// The offset of a completion queue's head doorbell.
#[must_use]
pub fn cq_doorbell(queue: u16, stride: u32) -> u64 {
    DOORBELL_BASE + (2 * u64::from(queue) + 1) * (4u64 << stride)
}

/// The value written to CC to enable the controller with the NVM command set, 4 KiB pages, and the
/// entry sizes this crate uses.
#[must_use]
pub fn cc_enable() -> u32 {
    CC_EN | (IOSQES << 16) | (IOCQES << 20)
}

/// Whether CSTS reports the controller ready.
#[must_use]
pub fn csts_ready(csts: u32) -> bool {
    csts & CSTS_RDY != 0
}

/// The readback of a stopped NVMe controller. These are decisions over register values, so
/// they can be checked on a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StopState {
    /// The controller configuration register.
    pub control: u32,
    /// The controller status register.
    pub status: u32,
}

impl StopState {
    /// Whether the controller is disabled and no longer ready: it processes no command and
    /// reaches no queue.
    #[must_use]
    pub fn disabled(&self) -> bool {
        self.control & CC_EN == 0 && self.status & CSTS_RDY == 0
    }
}

/// The final readback filter, not a hardware operation: the controller must be disabled and
/// not ready, and the function must be excluded from bus mastering, legacy interrupts and
/// message delivery, before an exposed page may be freed.
#[must_use]
pub fn stop_proven(state: StopState, command: u16, messages_disabled: bool) -> bool {
    state.disabled() && nolx_pci_core::exclusion::excluded(command, messages_disabled)
}

/// The AQA value for an admin queue of `submit` submission entries and `complete` completion
/// entries. Each size is stored one less than the count. Returns `None` for a size outside 1 to
/// 4096.
#[must_use]
pub fn aqa(submit: u16, complete: u16) -> Option<u32> {
    let valid = |entries: u16| (1..=QUEUE_ENTRIES_MAX).contains(&entries);
    if !valid(submit) || !valid(complete) {
        return None;
    }
    Some(u32::from(submit - 1) | (u32::from(complete - 1) << 16))
}

/// A 64-byte submission queue entry, as the bytes the controller reads.
pub type Sqe = [u8; SQE_BYTES];

/// Build a submission queue entry. Command dword 0 holds the opcode and the command identifier;
/// the namespace, the two physical region pointers, and dwords 10 to 15 follow the specification.
#[must_use]
pub fn sqe(opcode: u8, cid: u16, nsid: u32, prp1: u64, prp2: u64, cdw: [u32; 6]) -> Sqe {
    let mut out = [0u8; SQE_BYTES];
    let dw0 = u32::from(opcode) | (u32::from(cid) << 16);
    out[0..4].copy_from_slice(&dw0.to_le_bytes());
    out[4..8].copy_from_slice(&nsid.to_le_bytes());
    out[24..32].copy_from_slice(&prp1.to_le_bytes());
    out[32..40].copy_from_slice(&prp2.to_le_bytes());
    for (index, value) in cdw.iter().enumerate() {
        let at = 40 + 4 * index;
        out[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    out
}

/// An Identify command. `cns` is [`CNS_NAMESPACE`] or [`CNS_CONTROLLER`]; the answer is a
/// 4 KiB structure at `prp1`.
#[must_use]
pub fn identify(cid: u16, nsid: u32, cns: u32, prp1: u64) -> Sqe {
    sqe(OP_IDENTIFY, cid, nsid, prp1, 0, [cns, 0, 0, 0, 0, 0])
}

/// A Create I/O Completion Queue command for queue `queue` of `entries` entries, physically
/// contiguous, with interrupts left off. The entry count is stored one less than the count.
#[must_use]
pub fn create_io_cq(cid: u16, queue: u16, entries: u16, prp1: u64) -> Option<Sqe> {
    queue_size(entries)?;
    let cdw10 = u32::from(queue) | (u32::from(entries - 1) << 16);
    // Bit 0: physically contiguous. Bit 1 (interrupts enabled) stays clear: the driver polls.
    Some(sqe(
        OP_CREATE_IO_CQ,
        cid,
        0,
        prp1,
        0,
        [cdw10, 1, 0, 0, 0, 0],
    ))
}

/// The number of MSI-X table entries an NVMe controller's table can name.
pub const MSIX_VECTOR_LIMIT: u16 = 2048;

/// A Create I/O Completion Queue command with interrupts on: the queue's completions are signalled
/// on MSI-X table entry `vector`. Bit 1 is the interrupts-enabled flag and bit 0 the physically
/// contiguous flag; the vector sits in bits 31:16. Returns `None` for a queue size the controller
/// refuses, or a vector past the table.
#[must_use]
pub fn create_io_cq_msix(
    cid: u16,
    queue: u16,
    entries: u16,
    prp1: u64,
    vector: u16,
) -> Option<Sqe> {
    queue_size(entries)?;
    if vector >= MSIX_VECTOR_LIMIT {
        return None;
    }
    let cdw10 = u32::from(queue) | (u32::from(entries - 1) << 16);
    let cdw11 = 0b11 | (u32::from(vector) << 16);
    Some(sqe(
        OP_CREATE_IO_CQ,
        cid,
        0,
        prp1,
        0,
        [cdw10, cdw11, 0, 0, 0, 0],
    ))
}

/// A Create I/O Submission Queue command for queue `queue` of `entries` entries, bound to
/// completion queue `completion`, physically contiguous.
#[must_use]
pub fn create_io_sq(cid: u16, queue: u16, entries: u16, completion: u16, prp1: u64) -> Option<Sqe> {
    queue_size(entries)?;
    let cdw10 = u32::from(queue) | (u32::from(entries - 1) << 16);
    let cdw11 = 1 | (u32::from(completion) << 16);
    Some(sqe(
        OP_CREATE_IO_SQ,
        cid,
        0,
        prp1,
        0,
        [cdw10, cdw11, 0, 0, 0, 0],
    ))
}

/// The entry count a queue can have: at least one, at most [`QUEUE_ENTRIES_MAX`].
fn queue_size(entries: u16) -> Option<()> {
    (1..=QUEUE_ENTRIES_MAX).contains(&entries).then_some(())
}

/// A read or write of `blocks` blocks starting at `lba`, through one physical region pointer that
/// covers the whole transfer. Returns `None` for a zero-block transfer. The block count is stored
/// one less than the count.
#[must_use]
pub fn read_write(
    write: bool,
    cid: u16,
    nsid: u32,
    lba: u64,
    blocks: u16,
    prp1: u64,
) -> Option<Sqe> {
    if blocks == 0 {
        return None;
    }
    let opcode = if write { OP_WRITE } else { OP_READ };
    let cdw10 = lba as u32;
    let cdw11 = (lba >> 32) as u32;
    let cdw12 = u32::from(blocks - 1);
    Some(sqe(
        opcode,
        cid,
        nsid,
        prp1,
        0,
        [cdw10, cdw11, cdw12, 0, 0, 0],
    ))
}

/// The command identifier a completion carries, from completion dword 3.
#[must_use]
pub fn completion_cid(dw3: u32) -> u16 {
    (dw3 & 0xFFFF) as u16
}

/// The phase tag a completion carries, from completion dword 3.
#[must_use]
pub fn completion_phase(dw3: u32) -> bool {
    dw3 & (1 << 16) != 0
}

/// The status field of a completion: the status code, type, and flags, with the phase tag
/// removed. Zero means success.
#[must_use]
pub fn completion_status(dw3: u32) -> u16 {
    ((dw3 >> 17) & 0x7FFF) as u16
}

/// The submission queue head a completion reports, from completion dword 2.
#[must_use]
pub fn completion_sq_head(dw2: u32) -> u16 {
    (dw2 & 0xFFFF) as u16
}

/// The number of logical blocks in a namespace, from the Identify Namespace structure (NSZE).
#[must_use]
pub fn namespace_blocks(identify: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&identify[0..8]);
    u64::from_le_bytes(bytes)
}

/// The block size in bytes a namespace uses, from the Identify Namespace structure: the format
/// selected by FLBAS, and that format's LBA data size exponent (LBADS). Returns `None` for an
/// exponent below 9 (512 bytes) or above 16.
#[must_use]
pub fn namespace_block_bytes(identify: &[u8]) -> Option<u32> {
    let format = usize::from(identify[26] & 0xF);
    let at = 128 + 4 * format;
    let dw = u32::from_le_bytes([
        identify[at],
        identify[at + 1],
        identify[at + 2],
        identify[at + 3],
    ]);
    let exponent = (dw >> 16) & 0xFF;
    (9..=16).contains(&exponent).then(|| 1u32 << exponent)
}

#[cfg(test)]
mod tests;

/// Flush all prior writes to one namespace; no PRP data buffer is supplied.
#[must_use]
pub fn flush(cid: u16, nsid: u32) -> Sqe {
    sqe(0, cid, nsid, 0, 0, [0; 6])
}

#[cfg(test)]
mod flush_tests {
    use super::*;
    #[test]
    fn flush_names_namespace_and_cid_but_has_no_data_or_command_dwords() {
        let command = flush(7, 3);
        assert_eq!(command[0], 0);
        assert_eq!(&command[2..4], &7u16.to_le_bytes());
        assert_eq!(&command[4..8], &3u32.to_le_bytes());
        assert!(command[8..].iter().all(|&b| b == 0));
    }
}

/// Read a published completion, checking its phase byte before copying any payload.
///
/// The controller publishes the status/phase after the other completion fields. Copying the
/// whole entry first can mix an old CID with a newly published phase. `read` must perform
/// volatile/coherent device-memory reads; the queue entry must not be released to the device
/// until the returned snapshot has been consumed.
pub fn read_completion(
    expected_phase: bool,
    mut read: impl FnMut(usize, &mut [u8]),
) -> Option<[u8; CQE_BYTES]> {
    let mut phase = [0; 1];
    read(14, &mut phase);
    if (phase[0] & 1 != 0) != expected_phase {
        return None;
    }
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    let mut entry = [0; CQE_BYTES];
    read(0, &mut entry);
    Some(entry)
}

#[cfg(test)]
mod publication_tests {
    use super::*;
    #[test]
    fn phase_is_observed_before_copying_the_published_cid() {
        let mut calls = 0;
        let entry = read_completion(true, |offset, out| {
            calls += 1;
            if calls == 1 {
                assert_eq!((offset, out.len()), (14, 1));
                out[0] = 1;
            } else {
                assert_eq!((offset, out.len()), (0, CQE_BYTES));
                // The device's payload became visible with its phase publication.
                out[12..14].copy_from_slice(&71u16.to_le_bytes());
                out[14] = 1;
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
        assert_eq!(
            completion_cid(u32::from_le_bytes(entry[12..16].try_into().unwrap())),
            71
        );
    }
    #[test]
    fn an_unpublished_entry_never_reads_stale_payload() {
        let mut calls = 0;
        assert_eq!(
            read_completion(false, |offset, out| {
                calls += 1;
                assert_eq!((offset, out.len()), (14, 1));
                out[0] = 1;
            }),
            None
        );
        assert_eq!(calls, 1);
    }
}

#[cfg(test)]
mod stop_tests {
    use super::*;

    #[test]
    fn a_disabled_controller_is_neither_enabled_nor_ready() {
        assert!(StopState {
            control: 0,
            status: 0
        }
        .disabled());
        assert!(!StopState {
            control: CC_EN,
            status: 0
        }
        .disabled());
        assert!(!StopState {
            control: 0,
            status: CSTS_RDY
        }
        .disabled());
    }

    #[test]
    fn the_stop_proof_needs_the_controller_and_the_pci_exclusion_together() {
        let state = StopState {
            control: 0,
            status: 0,
        };
        assert!(stop_proven(state, 1 << 10, true));
        assert!(!stop_proven(state, 1 << 10, false));
        assert!(!stop_proven(state, 1 << 10 | 4, true));
        assert!(!stop_proven(state, 0, true));
        let mut enabled = state;
        enabled.control = CC_EN;
        assert!(!stop_proven(enabled, 1 << 10, true));
        let mut ready = state;
        ready.status = CSTS_RDY;
        assert!(!stop_proven(ready, 1 << 10, true));
    }
}
