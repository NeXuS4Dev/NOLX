//! Host tests. Each rule is checked against the byte layout the NVMe specification gives.

use super::*;

#[test]
fn capability_fields_decode_queue_limit_stride_and_timeout() {
    let cap = 0x3FFu64 | (0x14u64 << 24) | (2u64 << 32);
    assert_eq!(cap_max_queue_entries(cap), 1024);
    assert_eq!(cap_doorbell_stride(cap), 2);
    assert_eq!(cap_timeout_ms(cap), 10_000);
}

#[test]
fn doorbells_are_spaced_by_the_stride() {
    assert_eq!(sq_doorbell(0, 0), 0x1000);
    assert_eq!(cq_doorbell(0, 0), 0x1004);
    assert_eq!(sq_doorbell(1, 0), 0x1008);
    assert_eq!(cq_doorbell(1, 0), 0x100C);
    assert_eq!(sq_doorbell(1, 2), 0x1000 + 2 * 16);
    assert_eq!(cq_doorbell(1, 2), 0x1000 + 3 * 16);
}

#[test]
fn enable_selects_64_byte_commands_and_16_byte_completions() {
    assert_eq!(cc_enable(), 0x0046_0001);
    assert!(csts_ready(1));
    assert!(!csts_ready(0));
}

#[test]
fn admin_queue_sizes_are_stored_one_less_and_bounded() {
    assert_eq!(aqa(8, 8), Some(0x0007_0007));
    assert_eq!(aqa(4096, 1), Some(0x0000_0FFF));
    assert_eq!(aqa(0, 8), None);
    assert_eq!(aqa(8, 4097), None);
}

#[test]
fn a_submission_entry_places_each_field_at_its_offset() {
    let entry = sqe(
        0x02,
        0x1234,
        1,
        0x1122_3344_5566_7788,
        0,
        [1, 2, 3, 4, 5, 6],
    );
    assert_eq!(&entry[0..4], &(0x02u32 | (0x1234u32 << 16)).to_le_bytes());
    assert_eq!(&entry[4..8], &1u32.to_le_bytes());
    assert_eq!(&entry[24..32], &0x1122_3344_5566_7788u64.to_le_bytes());
    assert_eq!(&entry[40..44], &1u32.to_le_bytes());
    assert_eq!(&entry[60..64], &6u32.to_le_bytes());
    assert_eq!(entry.len(), SQE_BYTES);
}

#[test]
fn identify_asks_for_the_structure_named_by_cns() {
    let controller = identify(7, 0, CNS_CONTROLLER, 0x4000);
    assert_eq!(controller[0], OP_IDENTIFY);
    assert_eq!(&controller[40..44], &1u32.to_le_bytes());
    let namespace = identify(8, 1, CNS_NAMESPACE, 0x4000);
    assert_eq!(&namespace[40..44], &0u32.to_le_bytes());
    assert_eq!(&namespace[4..8], &1u32.to_le_bytes());
}

#[test]
fn io_queues_are_stored_one_entry_less_and_refused_when_empty_or_too_big() {
    let cq = create_io_cq(3, 1, 8, 0x5000).unwrap();
    assert_eq!(cq[0], OP_CREATE_IO_CQ);
    assert_eq!(&cq[40..44], &((7u32 << 16) | 1).to_le_bytes());
    assert_eq!(
        &cq[44..48],
        &1u32.to_le_bytes(),
        "physically contiguous, no interrupts"
    );

    let sq = create_io_sq(4, 1, 8, 1, 0x6000).unwrap();
    assert_eq!(sq[0], OP_CREATE_IO_SQ);
    assert_eq!(
        &sq[44..48],
        &((1u32 << 16) | 1).to_le_bytes(),
        "bound to completion queue 1"
    );

    assert!(create_io_cq(3, 1, 0, 0x5000).is_none());
    assert!(create_io_sq(4, 1, 4097, 1, 0x6000).is_none());
}

#[test]
fn a_read_or_write_carries_the_split_lba_and_a_zero_based_count() {
    let write = read_write(true, 9, 1, 0x0000_0002_0000_0640, 4, 0x7000).unwrap();
    assert_eq!(write[0], OP_WRITE);
    assert_eq!(&write[40..44], &0x0000_0640u32.to_le_bytes());
    assert_eq!(&write[44..48], &2u32.to_le_bytes());
    assert_eq!(&write[48..52], &3u32.to_le_bytes(), "NLB is 4 minus one");

    let read = read_write(false, 10, 1, 0, 1, 0x7000).unwrap();
    assert_eq!(read[0], OP_READ);
    assert!(read_write(false, 10, 1, 0, 0, 0x7000).is_none());
}

#[test]
fn a_completion_gives_its_identifier_phase_and_status() {
    // CID 7, phase set, status field zero: success.
    let ok = 7u32 | (1 << 16);
    assert_eq!(completion_cid(ok), 7);
    assert!(completion_phase(ok));
    assert_eq!(completion_status(ok), 0);
    // Status code 0x02 (invalid field) in the status field, phase clear.
    let failed = 9u32 | (0x02 << 17);
    assert!(!completion_phase(failed));
    assert_eq!(completion_status(failed), 0x02);
    assert_eq!(completion_sq_head(0x0000_0005), 5);
}

#[test]
fn the_namespace_reports_its_block_count_and_block_size() {
    let mut identify = [0u8; 4096];
    identify[0..8].copy_from_slice(&1_000_000u64.to_le_bytes());
    // Format 0 uses 4 KiB blocks, format 1 uses 512-byte blocks; the namespace selects format 1.
    identify[128 + 2] = 12;
    identify[128 + 4 + 2] = 9;
    identify[26] = 1;
    assert_eq!(namespace_blocks(&identify), 1_000_000);
    assert_eq!(namespace_block_bytes(&identify), Some(512));
    identify[26] = 0;
    assert_eq!(namespace_block_bytes(&identify), Some(4096));
    identify[128 + 2] = 8;
    assert_eq!(
        namespace_block_bytes(&identify),
        None,
        "exponent below 9 is refused"
    );
}
