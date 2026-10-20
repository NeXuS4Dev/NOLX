//! Host tests: every function here runs against a modelled configuration space, so the
//! discovery, sizing, and walking rules are exercised without any port access.

use super::*;
use std::vec::Vec;

/// One function's 64 dwords, plus the mask of bits software is allowed to change.
#[derive(Clone)]
pub(super) struct Regs {
    pub(super) values: [u32; 64],
    pub(super) writable: [u32; 64],
}

impl Regs {
    fn blank() -> Self {
        Self {
            values: [0; 64],
            writable: [0; 64],
        }
    }

    /// A type-0 endpoint with the given identity. Command register 0 means no capability list.
    pub(super) fn endpoint(
        vendor: u16,
        device: u16,
        class: u8,
        subclass: u8,
        multifunction: bool,
    ) -> Self {
        let mut regs = Self::blank();
        regs.values[0] = (vendor as u32) | ((device as u32) << 16);
        regs.values[2] = ((class as u32) << 24) | ((subclass as u32) << 16);
        regs.values[3] = if multifunction { 0x0080_0000 } else { 0 };
        regs
    }

    /// A PCI-to-PCI bridge whose secondary bus is `secondary`.
    pub(super) fn bridge(vendor: u16, device: u16, secondary: u8) -> Self {
        let mut regs = Self::endpoint(vendor, device, CLASS_BRIDGE, SUBCLASS_PCI_BRIDGE, false);
        regs.values[3] |= 0x0001_0000; // header type 1
        regs.values[6] = ((secondary as u32) << 8) | ((secondary as u32 + 1) << 16);
        regs
    }
}

/// A machine: a set of functions at fixed addresses. Anything else reads as "no device".
pub(super) struct Machine {
    functions: Vec<(Bdf, Regs)>,
    /// Every write that reached configuration space, so restore-after-probe can be checked.
    pub(super) writes: Vec<(Bdf, u8, u32)>,
}

impl Machine {
    pub(super) fn new() -> Self {
        Self {
            functions: Vec::new(),
            writes: Vec::new(),
        }
    }

    pub(super) fn add(&mut self, bdf: Bdf, regs: Regs) {
        self.functions.push((bdf, regs));
    }

    pub(super) fn regs(&self, bdf: Bdf) -> Option<&Regs> {
        self.functions
            .iter()
            .find(|(addr, _)| *addr == bdf)
            .map(|(_, regs)| regs)
    }
}

impl ConfigSpace for Machine {
    fn read_u32(&mut self, bdf: Bdf, offset: u8) -> u32 {
        match self.functions.iter().find(|(addr, _)| *addr == bdf) {
            Some((_, regs)) => regs.values[offset as usize / 4],
            None => 0xFFFF_FFFF,
        }
    }

    fn write_u32(&mut self, bdf: Bdf, offset: u8, value: u32) {
        self.writes.push((bdf, offset, value));
        if let Some((_, regs)) = self.functions.iter_mut().find(|(addr, _)| *addr == bdf) {
            let index = offset as usize / 4;
            let keep = regs.values[index] & !regs.writable[index];
            regs.values[index] = (value & regs.writable[index]) | keep;
        }
    }
}

pub(super) fn bdf(bus: u8, device: u8, function: u8) -> Bdf {
    Bdf::new(bus, device, function).expect("valid test address")
}

fn cap(offset: u8, id: u8) -> Capability {
    Capability { offset, id }
}

#[test]
fn config_address_packs_bus_device_function_and_dword() {
    // enable | bus 1 << 16 | device 2 << 11 | function 3 << 8 | dword 0x0C
    assert_eq!(config_address(bdf(1, 2, 3), 0x0E), 0x8001_130C);
    assert_eq!(config_address(bdf(0, 0, 0), 0), 0x8000_0000);
    assert_eq!(config_address(bdf(255, 31, 7), 0xFC), 0x80FF_FFFC);
}

#[test]
fn addresses_the_hardware_cannot_express_are_refused() {
    assert_eq!(Bdf::new(0, 32, 0), Err(PciError::InvalidDevice));
    assert_eq!(Bdf::new(0, 0, 8), Err(PciError::InvalidFunction));
    assert!(Bdf::new(255, 31, 7).is_ok());
}

#[test]
fn equal_bus_device_function_tuples_in_distinct_segments_are_distinct() {
    let segment_zero = Bdf::new(3, 4, 1).unwrap();
    let segment_seven = Bdf::in_segment(7, 3, 4, 1).unwrap();
    assert_eq!(
        (segment_zero.bus, segment_zero.device, segment_zero.function),
        (3, 4, 1)
    );
    assert_eq!(
        (
            segment_seven.bus,
            segment_seven.device,
            segment_seven.function
        ),
        (3, 4, 1)
    );
    assert_eq!(segment_zero.packed_id(), (3_u32 << 8) | (4 << 3) | 1);
    assert_eq!(
        segment_seven.packed_id(),
        (7_u32 << 16) | (3 << 8) | (4 << 3) | 1
    );
    assert_ne!(segment_zero, segment_seven);
}

#[test]
fn an_absent_function_is_not_recorded() {
    let mut machine = Machine::new();
    assert_eq!(read_function(&mut machine, bdf(0, 5, 0)), None);
}

#[test]
fn a_present_function_reports_its_identity_and_multifunction_bit() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x8086, 0x1237, 0x06, 0x00, true);
    regs.values[2] |= (0x02 << 8) | 0x7; // prog-if 2, revision 7
    machine.add(bdf(0, 1, 0), regs);
    let info = read_function(&mut machine, bdf(0, 1, 0)).expect("present");
    assert_eq!(info.vendor_id, 0x8086);
    assert_eq!(info.device_id, 0x1237);
    assert_eq!(
        (info.class, info.subclass, info.prog_if, info.revision),
        (6, 0, 2, 7)
    );
    assert_eq!(info.header_type, 0);
    assert!(info.multifunction);
}

#[test]
fn io_bars_decode_to_their_port_and_zero_is_unused() {
    assert_eq!(decode_bar(0x0000_C041, 0), Bar::Io { port: 0xC040 });
    assert_eq!(decode_bar(0x0000_0001, 0), Bar::Unused);
    assert_eq!(decode_bar(0, 0), Bar::Unused);
}

#[test]
fn memory_bars_decode_width_and_prefetch() {
    assert_eq!(
        decode_bar(0xFEBC_0008, 0),
        Bar::Memory32 {
            base: 0xFEBC_0000,
            prefetchable: true,
        }
    );
    assert_eq!(
        decode_bar(0xFEBC_0000, 0),
        Bar::Memory32 {
            base: 0xFEBC_0000,
            prefetchable: false,
        }
    );
    // Type 0b10 is a 64-bit window: the high dword supplies bits 63:32.
    assert_eq!(
        decode_bar(0x0010_0004, 0x0000_0002),
        Bar::Memory64 {
            base: 0x2_0010_0000,
            prefetchable: false,
        }
    );
    // Type 0b01 is the legacy below-1-MiB window, reserved in PCI 3.0; refused, not misread.
    assert_eq!(decode_bar(0x0000_0002, 0), Bar::Reserved);
    assert_eq!(bar_slots(&decode_bar(0x0010_0004, 2)), 2);
    assert_eq!(bar_slots(&decode_bar(0xFEBC_0000, 0)), 1);
}

#[test]
fn bar_sizes_are_powers_of_two_from_the_probe_mask() {
    let mem32 = Bar::Memory32 {
        base: 0,
        prefetchable: false,
    };
    assert_eq!(bar_size(&mem32, 0xFFFF_F000, 0), Some(0x1000));
    assert_eq!(bar_size(&mem32, 0xFFF0_0000, 0), Some(0x10_0000));
    assert_eq!(bar_size(&Bar::Io { port: 0 }, 0xFFFF_FFC1, 0), Some(0x40));
    let mem64 = Bar::Memory64 {
        base: 0,
        prefetchable: false,
    };
    assert_eq!(bar_size(&mem64, 0xFFF0_0000, 0xFFFF_FFFF), Some(0x10_0000));
    // A device that implements no bits is a slot that is not there.
    assert_eq!(bar_size(&mem32, 0, 0), None);
    assert_eq!(bar_size(&Bar::Unused, 0xFFFF_FFFF, 0), None);
}

#[test]
fn bars_are_sized_decoded_and_restored() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x1AF4, 0x1000, 0x02, 0x00, false);
    // BAR0-1: 64-bit prefetchable window of 1 MiB at 0x2_0010_0000.
    regs.values[4] = 0x0010_000C;
    regs.values[5] = 0x0000_0002;
    regs.writable[4] = 0xFFF0_0000;
    regs.writable[5] = 0xFFFF_FFFF;
    // BAR2: 32-bit window of 16 KiB at 0xFEBC_0000.
    regs.values[6] = 0xFEBC_0000;
    regs.writable[6] = 0xFFFF_C000;
    // BAR3: unused.
    // BAR4: I/O window of 64 bytes at port 0xC040.
    regs.values[8] = 0x0000_C041;
    regs.writable[8] = 0xFFFF_FFC0;
    // BAR5: a 64-bit type in the last slot has nowhere to put its high half.
    regs.values[9] = 0x0000_0004;
    machine.add(bdf(0, 3, 0), regs.clone());

    let set = read_bars(&mut machine, bdf(0, 3, 0));
    assert_eq!(
        set.slots[0],
        Bar::Memory64 {
            base: 0x2_0010_0000,
            prefetchable: true,
        }
    );
    assert_eq!(set.slots[1], Bar::Unused);
    assert_eq!(set.sizes[0], Some(0x10_0000));
    assert_eq!(
        set.slots[2],
        Bar::Memory32 {
            base: 0xFEBC_0000,
            prefetchable: false,
        }
    );
    assert_eq!(set.sizes[2], Some(0x4000));
    assert_eq!(set.slots[3], Bar::Unused);
    assert_eq!(set.sizes[3], None);
    assert_eq!(set.slots[4], Bar::Io { port: 0xC040 });
    assert_eq!(set.sizes[4], Some(0x40));
    assert_eq!(set.slots[5], Bar::Reserved);

    // Every register came back exactly as it was: sizing never leaves a probe behind.
    let after = machine.regs(bdf(0, 3, 0)).expect("present");
    assert_eq!(after.values[4..=9], regs.values[4..=9]);
    let probed = machine
        .writes
        .iter()
        .filter(|write| write.1 == 0x10)
        .any(|write| write.2 == 0xFFFF_FFFF);
    assert!(probed);
}

#[test]
fn sizing_switches_decoding_off_and_restores_the_command_register() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x1AF4, 0x1000, 0x02, 0x00, false);
    // Bus mastering (bit 2) is on; memory decoding (bit 1) is on. Only the decode bits are
    // writable here, so the probe cannot disturb the status half of the register.
    regs.values[1] = 0x0000_0006;
    regs.writable[1] = 0x0000_0003;
    regs.values[4] = 0xFEBC_0000;
    regs.writable[4] = 0xFFFF_F000;
    machine.add(bdf(0, 4, 0), regs);

    let set = read_bars(&mut machine, bdf(0, 4, 0));
    assert_eq!(set.sizes[0], Some(0x1000));

    let command_writes: Vec<u32> = machine
        .writes
        .iter()
        .filter(|write| write.1 == 0x04)
        .map(|write| write.2)
        .collect();
    // Decoding off before the first probe, then the original value (decoding on) at the end.
    assert_eq!(command_writes.first(), Some(&0x0000_0004));
    assert_eq!(command_writes.last(), Some(&0x0000_0006));
    let after = machine.regs(bdf(0, 4, 0)).expect("present");
    assert_eq!(after.values[1], 0x0000_0006);
}

#[test]
fn a_function_without_a_capability_bit_has_no_list() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x8086, 0x1, 0x01, 0x06, false);
    regs.values[13] = 0x40; // a capability pointer exists but status says there is no list
    machine.add(bdf(0, 1, 0), regs);
    assert_eq!(capabilities(&mut machine, bdf(0, 1, 0)).len, 0);
}

#[test]
fn the_capability_list_is_walked_in_link_order() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x8086, 0x2, 0x02, 0x00, false);
    regs.values[1] = STATUS_CAP_LIST; // status bit 4
    regs.values[13] = 0x50; // capability pointer at 0x34
    regs.values[0x50 / 4] = 0x0000_6001; // id 0x01 (power), next 0x60
    regs.values[0x60 / 4] = 0x0000_7005; // id 0x05 (MSI), next 0x70
    regs.values[0x70 / 4] = 0x0000_0011; // id 0x11 (MSI-X), end of list
    machine.add(bdf(0, 1, 0), regs);
    let list = capabilities(&mut machine, bdf(0, 1, 0));
    assert!(!list.truncated);
    assert_eq!(
        list.as_slice(),
        &[cap(0x50, 0x01), cap(0x60, 0x05), cap(0x70, 0x11)]
    );
    assert_eq!(list.find(0x05).map(|entry| entry.offset), Some(0x60));
    assert_eq!(list.find(0x10), None);
}

#[test]
fn a_pointer_into_the_header_is_refused_and_a_loop_is_bounded() {
    let mut machine = Machine::new();
    let mut regs = Regs::endpoint(0x8086, 0x3, 0x02, 0x00, false);
    regs.values[1] = STATUS_CAP_LIST;
    regs.values[13] = 0x20; // points into the header: no entries at all
    machine.add(bdf(0, 1, 0), regs);
    assert_eq!(capabilities(&mut machine, bdf(0, 1, 0)).len, 0);

    let mut looped = Regs::endpoint(0x8086, 0x4, 0x02, 0x00, false);
    looped.values[1] = STATUS_CAP_LIST;
    looped.values[13] = 0x40;
    looped.values[0x40 / 4] = 0x0000_4010; // id 0x10 links to itself
    machine.add(bdf(0, 2, 0), looped);
    let list = capabilities(&mut machine, bdf(0, 2, 0));
    assert_eq!(list.len, MAX_CAPABILITIES);
    assert!(list.truncated);
}

#[test]
fn enumeration_follows_multifunction_devices_and_secondary_buses() {
    let mut machine = Machine::new();
    // Host bridge at 0:0.0 (class 06/00, not a PCI-to-PCI bridge, so not followed).
    machine.add(
        bdf(0, 0, 0),
        Regs::endpoint(0x8086, 0x1237, 0x06, 0x00, false),
    );
    // 0:1 is multifunction: functions 0 and 2 present, function 1 absent.
    machine.add(
        bdf(0, 1, 0),
        Regs::endpoint(0x1AF4, 0x1000, 0x02, 0x00, true),
    );
    machine.add(
        bdf(0, 1, 2),
        Regs::endpoint(0x1AF4, 0x1001, 0x01, 0x00, false),
    );
    // 0:2 is a PCI-to-PCI bridge whose secondary bus is 1.
    machine.add(bdf(0, 2, 0), Regs::bridge(0x8086, 0x2448, 1));
    // Bus 1 holds an endpoint at 1:0.0.
    machine.add(
        bdf(1, 0, 0),
        Regs::endpoint(0x10EC, 0x8168, 0x02, 0x00, false),
    );

    let inventory = enumerate(&mut machine);
    let found: Vec<Bdf> = inventory.as_slice().iter().map(|info| info.bdf).collect();
    assert_eq!(found.len(), 5);
    assert_eq!(found[0], bdf(0, 0, 0));
    assert_eq!(found[1], bdf(0, 1, 0));
    assert_eq!(found[2], bdf(0, 1, 2));
    assert_eq!(found[3], bdf(0, 2, 0));
    assert_eq!(found[4], bdf(1, 0, 0));
    assert_eq!(inventory.refused_bridges, 0);
    assert!(!inventory.overflowed);
    let nic = inventory
        .find_class(0x02, 0x00)
        .expect("network controller on bus 1 is found");
    assert_eq!(nic.bdf, bdf(0, 1, 0));
}

#[test]
fn a_bridge_that_points_back_is_refused_and_enumeration_terminates() {
    let mut machine = Machine::new();
    machine.add(bdf(0, 0, 0), Regs::bridge(0x1022, 0x1, 0));
    let inventory = enumerate(&mut machine);
    assert_eq!(inventory.len, 1);
    assert_eq!(inventory.refused_bridges, 1);
}

#[test]
fn multiple_root_ranges_scan_a_bus_and_refused_bridge_only_once() {
    let mut machine = Machine::new();
    machine.add(bdf(0, 0, 0), Regs::bridge(0x1022, 0x1, 0));
    let inventory = enumerate_ranges(&mut machine, &[(0, 0), (0, 1), (1, 1)]);
    assert_eq!(inventory.len, 1);
    assert_eq!(inventory.refused_bridges, 1);

    let mut machine = Machine::new();
    machine.add(bdf(0, 0, 0), Regs::bridge(0x8086, 0x2448, 1));
    machine.add(
        bdf(1, 0, 0),
        Regs::endpoint(0x10ec, 0x8168, 0x02, 0x00, false),
    );
    let inventory = enumerate_ranges(&mut machine, &[(0, 0), (0, 1), (1, 1)]);
    assert_eq!(inventory.len, 2);
    assert_eq!(inventory.refused_bridges, 0);
    assert_eq!(inventory.as_slice()[1].bdf, bdf(1, 0, 0));
}

#[test]
fn an_inventory_refuses_the_function_after_its_last_slot() {
    let mut inventory = Inventory::default();
    let info = FunctionInfo {
        vendor_id: 1,
        ..FunctionInfo::default()
    };
    for _ in 0..MAX_FUNCTIONS {
        assert_eq!(inventory.push(info), Ok(()));
    }
    assert_eq!(inventory.push(info), Err(PciError::Full));
    assert!(inventory.overflowed);
    assert_eq!(inventory.len, MAX_FUNCTIONS);
}
