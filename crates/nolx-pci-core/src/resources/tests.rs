use super::*;
use crate::tests::{bdf, Machine, Regs};
use crate::{read_bars, read_bridge_bars, FunctionInfo};

fn endpoint() -> Regs {
    let mut r = Regs::endpoint(0x1234, 0x5678, 1, 8, false);
    r.writable[1] = 0xffff;
    r.writable[4] = 0xffff_f000;
    r.writable[5] = 0xffff_c000;
    r.values[5] = 4; // zero-address 64-bit BAR
    r.writable[6] = u32::MAX;
    r.values[7] = 1;
    r.writable[7] = 0xffff_ffc0;
    r
}
fn bridge(secondary: u8) -> Regs {
    let mut r = Regs::bridge(0x1234, 0x1234, secondary);
    r.writable[1] = 0xffff;
    r.values[7] = 0xa000_01f1; // closed IO, immutable 32-bit type; upper status W1C
    r.writable[7] = 0x0000_f0f0;
    r.values[8] = 0x0000_fff0; // closed memory
    r.writable[8] = 0xfff0_fff0;
    r.values[9] = 0x0000_fff0; // closed prefetch
    r.writable[12] = u32::MAX;
    r
}
fn pools() -> (Pool, Pool) {
    (
        Pool::new(Range::new(0xc000_0000, 0x1000_0000).unwrap()).unwrap(),
        Pool::new(Range::new(0xc000, 0x3000).unwrap()).unwrap(),
    )
}
fn proposal(m: &mut Machine) -> Result<Plan, Error> {
    let inventory = crate::enumerate(m);
    let (mem, io) = pools();
    plan(m, 0, &inventory, mem, io)
}

#[test]
fn aligned_pool_reserves_overlaps_and_refuses_without_mutation() {
    let mut pool = Pool::new(Range::new(3, 61).unwrap()).unwrap();
    pool.reserve(Range::new(16, 16).unwrap()).unwrap();
    pool.reserve(Range::new(20, 20).unwrap()).unwrap();
    assert_eq!(pool.allocate(8), Ok(Range { start: 8, end: 16 }));
    assert_eq!(pool.allocate(16), Ok(Range { start: 48, end: 64 }));
    let before = pool.free.clone();
    assert_eq!(pool.allocate(64), Err(Error::NoSpace));
    assert_eq!(pool.allocate(3), Err(Error::InvalidRange));
    assert_eq!(pool.free, before);
    assert_eq!(Range::new(u64::MAX, 1), Err(Error::InvalidRange));
}

#[test]
fn zero_address_bars_are_sized_without_losing_64_bit_upper_slot() {
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), endpoint());
    let bars = read_bars(&mut m, bdf(0, 3, 0));
    assert_eq!(bars.sizes[..4], [Some(4096), Some(16384), None, Some(64)]);
    assert_eq!(
        bars.slots[1],
        Bar::Memory64 {
            base: 0,
            prefetchable: false
        }
    );
    assert_eq!(m.regs(bdf(0, 3, 0)).unwrap().values[5], 4);
    assert_eq!(m.regs(bdf(0, 3, 0)).unwrap().values[6], 0);
    assert_eq!(crate::assigned_memory_base(bars.slots[0]), None);
    assert_eq!(crate::assigned_memory_base(bars.slots[1]), None);
    assert_eq!(crate::assigned_memory_base(bars.slots[3]), None);
    assert_eq!(
        crate::assigned_memory_base(Bar::Memory64 {
            base: 0x1_0000_0000,
            prefetchable: true
        }),
        Some(0x1_0000_0000)
    );
}

#[test]
fn assigns_root_mem32_mem64_and_io_preserving_flags_command_and_status() {
    let mut m = Machine::new();
    let mut r = endpoint();
    r.values[1] = 0xa000_0007;
    m.add(bdf(0, 3, 0), r);
    let p = proposal(&mut m).unwrap();
    assert_eq!(p.assignments().len(), 3);
    p.apply(&mut m).unwrap();
    let bars = read_bars(&mut m, bdf(0, 3, 0));
    assert_eq!(
        bars.slots[1],
        Bar::Memory64 {
            base: 0xc000_0000,
            prefetchable: false
        }
    );
    assert_eq!(
        bars.slots[0],
        Bar::Memory32 {
            base: 0xc000_4000,
            prefetchable: false
        }
    );
    assert_eq!(bars.slots[3], Bar::Io { port: 0xc000 });
    assert_eq!(m.regs(bdf(0, 3, 0)).unwrap().values[1], 0xa000_0007);
    assert!(m
        .writes
        .iter()
        .filter(|(_, off, _)| *off == 4)
        .all(|(_, _, value)| value & 0xffff_0000 == 0));
    assert!(proposal(&mut m).unwrap().assignments().is_empty());
}

#[test]
fn builds_nested_bridge_windows_and_routes_own_bridge_bars_on_parent_bus() {
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), bridge(1));
    m.add(bdf(1, 1, 0), bridge(2));
    m.add(bdf(2, 1, 0), endpoint());
    let p = proposal(&mut m).unwrap();
    assert_eq!(p.windows(), 4);
    p.apply(&mut m).unwrap();
    let leaf = read_bars(&mut m, bdf(2, 1, 0));
    let Bar::Memory64 { base, .. } = leaf.slots[1] else {
        panic!()
    };
    for bridge in [bdf(0, 3, 0), bdf(1, 1, 0)] {
        let raw = m.read_u32(bridge, 0x20);
        assert!(memory_window(raw)
            .unwrap_or_else(|| panic!(
                "bridge {bridge:?} window {raw:#010x} is not a window; base {base:#x}, windows {}",
                p.windows()
            ))
            .contains(Range::new(base, 16384).unwrap()));
        assert_eq!(m.read_u32(bridge, 4) & 7, 7);
    }
    assert!(m
        .writes
        .iter()
        .filter(|(b, off, _)| b.bus < 2 && *off == 0x1c)
        .all(|(_, _, v)| v & 0xffff_0000 == 0));
    assert!(proposal(&mut m).unwrap().assignments().is_empty());
}

#[test]
fn a_closed_memory_window_is_never_invented_from_the_io_base_register() {
    let mut m = Machine::new();
    // The model's bridge publishes a real I/O base/limit word and a closed memory window.
    // Decoding the memory window from offset 0x1c would invent 0x01f00000..0xa0100000 and
    // assign the missing BAR inside it, outside the caller's aperture.
    m.add(bdf(0, 3, 0), bridge(1));
    m.add(bdf(1, 1, 0), endpoint());
    let plan = proposal(&mut m).unwrap();
    assert!(!plan.assignments().is_empty());
    for assignment in plan.assignments() {
        let floor = if assignment.io { 0xc000 } else { 0xc000_0000 };
        assert!(
            assignment.range.start >= floor,
            "assignment {assignment:?} came from a phantom bridge window"
        );
    }
    plan.apply(&mut m).unwrap();
    let window = memory_window(m.read_u32(bdf(0, 3, 0), 0x20)).expect("a real window");
    assert_eq!(window.start, 0xc000_0000);
}

#[test]
fn fixed_bars_are_reserved_and_never_moved() {
    let mut m = Machine::new();
    let mut r = endpoint();
    r.values[4] = 0xc000_0000;
    m.add(bdf(0, 3, 0), r);
    m.add(bdf(0, 4, 0), endpoint());
    let p = proposal(&mut m).unwrap();
    assert!(!p
        .assignments()
        .iter()
        .any(|a| a.bdf == bdf(0, 3, 0) && a.slot == 0));
    assert!(p
        .assignments()
        .iter()
        .filter(|a| !a.io)
        .all(|a| a.range.start >= 0xc000_1000));
    p.apply(&mut m).unwrap();
    assert_eq!(m.read_u32(bdf(0, 3, 0), 0x10), 0xc000_0000);
}

#[test]
fn no_space_overflow_alias_and_unrouted_resources_are_refused() {
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), endpoint());
    let inventory = crate::enumerate(&mut m);
    let (_, io) = pools();
    assert!(matches!(
        plan(
            &mut m,
            0,
            &inventory,
            Pool::new(Range::new(0xc000_0000, 4096).unwrap()).unwrap(),
            io
        ),
        Err(Error::NoSpace)
    ));
    assert_eq!(m.read_u32(bdf(0, 3, 0), 0x10), 0);
    let mut r = endpoint();
    r.values[4] = 0xc000_0000;
    m.add(bdf(0, 4, 0), r.clone());
    m.add(bdf(0, 5, 0), r.clone());
    assert!(matches!(proposal(&mut m), Err(Error::Overlap)));
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), bridge(1));
    m.add(bdf(1, 1, 0), r);
    assert!(matches!(proposal(&mut m), Err(Error::Unrouted)));
}

struct Refuse {
    machine: Machine,
    fail: Option<(Bdf, u8)>,
    permanent: bool,
}
impl ConfigSpace for Refuse {
    fn read_u32(&mut self, b: Bdf, o: u8) -> u32 {
        self.machine.read_u32(b, o)
    }
    fn write_u32(&mut self, b: Bdf, o: u8, v: u32) {
        if self.fail == Some((b, o)) {
            if !self.permanent {
                self.fail = None;
            }
            return;
        }
        self.machine.write_u32(b, o, v);
    }
}
impl crate::ecam::ExtendedConfigSpace for Refuse {
    fn read_dword(
        &mut self,
        segment: u16,
        bdf: Bdf,
        offset: u16,
    ) -> Result<u32, crate::ecam::EcamError> {
        self.machine.read_dword(segment, bdf, offset)
    }
    fn write_dword(
        &mut self,
        segment: u16,
        bdf: Bdf,
        offset: u16,
        value: u32,
    ) -> Result<(), crate::ecam::EcamError> {
        if self.fail == Some((bdf, offset as u8)) {
            if !self.permanent {
                self.fail = None;
            }
            return Ok(());
        }
        self.machine.write_dword(segment, bdf, offset, value)
    }
}
#[test]
fn write_refusal_rolls_back_bars_and_command_without_w1c_writes() {
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), endpoint());
    let p = proposal(&mut m).unwrap();
    let mut refused = Refuse {
        machine: m,
        fail: Some((bdf(0, 3, 0), 0x10)),
        permanent: false,
    };
    assert_eq!(p.apply(&mut refused), Err(Error::Readback));
    assert_eq!(refused.read_u32(bdf(0, 3, 0), 0x10), 0);
    assert_eq!(refused.read_u32(bdf(0, 3, 0), 0x14), 4);
    assert_eq!(refused.read_u32(bdf(0, 3, 0), 4), 0);
}
#[test]
fn malformed_masks_are_refused_and_prefetch_upper_bounds_are_retained() {
    assert_eq!(
        crate::bar_size(
            &Bar::Memory32 {
                base: 0,
                prefetchable: false
            },
            0xffff_a000,
            0
        ),
        None
    );
    let mut m = Machine::new();
    let mut r = bridge(1);
    r.values[9] = 0x1001_f001;
    r.values[10] = 1;
    r.values[11] = 2;
    m.add(bdf(0, 3, 0), r);
    assert_eq!(
        prefetch_window(&mut m, bdf(0, 3, 0)),
        Some(Range {
            start: 0x1_f000_0000,
            end: 0x2_1010_0000
        })
    );
}

#[test]
fn overlapping_sibling_windows_and_incomplete_topology_are_refused() {
    let mut m = Machine::new();
    let mut a = bridge(1);
    a.values[8] = 0xc000_c000;
    let mut b = bridge(3);
    b.values[8] = 0xc000_c000;
    m.add(bdf(0, 3, 0), a);
    m.add(bdf(0, 4, 0), b);
    assert!(matches!(proposal(&mut m), Err(Error::Overlap)));
    let mut inventory = crate::enumerate(&mut m);
    inventory.overflowed = true;
    let (mem, io) = pools();
    assert!(matches!(
        plan(&mut m, 0, &inventory, mem, io),
        Err(Error::Topology)
    ));
}

#[test]
fn resource_repair_is_scoped_to_the_requested_segment() {
    let mut inventory = Inventory::default();
    inventory
        .push(FunctionInfo {
            bdf: Bdf::in_segment(7, 0, 3, 0).unwrap(),
            vendor_id: 0x1234,
            ..FunctionInfo::default()
        })
        .unwrap();
    let (mem, io) = pools();
    // A segment the inventory does not name is refused rather than planned as segment zero.
    let mut machine = Machine::new();
    assert_eq!(
        plan(&mut machine, 0, &inventory, mem.clone(), io.clone()).err(),
        Some(Error::Topology)
    );
    // A bridge on a segment the transport refuses must surface that refusal. A type-0-only
    // inventory cannot: the segment-aware BAR reader treats an unreadable register as
    // absent, so the branch that must not silently vanish is the one with a bridge.
    inventory
        .push(FunctionInfo {
            bdf: Bdf::in_segment(7, 0, 4, 0).unwrap(),
            vendor_id: 0x1234,
            class: 0x06,
            subclass: 0x04,
            header_type: 1,
            ..FunctionInfo::default()
        })
        .unwrap();
    assert!(matches!(
        plan(&mut machine, 7, &inventory, mem, io),
        Err(Error::Ecam(_))
    ));
}

#[test]
fn resource_repair_plans_a_nonzero_segment_when_the_transport_serves_it() {
    let mut inventory = Inventory::default();
    inventory
        .push(FunctionInfo {
            bdf: Bdf::in_segment(7, 0, 3, 0).unwrap(),
            vendor_id: 0x1234,
            class: 0x02,
            subclass: 0x00,
            ..FunctionInfo::default()
        })
        .unwrap();
    let (mem, io) = pools();
    let mut machine = Machine::with_segment(7);
    machine.add(Bdf::in_segment(7, 0, 3, 0).unwrap(), endpoint());
    let plan = plan(&mut machine, 7, &inventory, mem, io).unwrap();
    assert!(!plan.assignments().is_empty());
    plan.apply(&mut machine).unwrap();
    assert_ne!(
        crate::assigned_memory_base(
            read_bars(&mut machine, Bdf::in_segment(7, 0, 3, 0).unwrap()).slots[1]
        ),
        None
    );
}

#[test]
fn bridge_bar_probe_never_sizes_window_or_bus_number_registers() {
    let mut m = Machine::new();
    m.add(bdf(0, 3, 0), bridge(1));
    read_bridge_bars(&mut m, bdf(0, 3, 0));
    assert!(m
        .writes
        .iter()
        .all(|(_, offset, _)| [4, 0x10, 0x14].contains(offset)));
}

#[test]
fn rollback_failure_does_not_reenable_decoding() {
    struct Corrupt(Machine);
    impl ConfigSpace for Corrupt {
        fn read_u32(&mut self, b: Bdf, o: u8) -> u32 {
            self.0.read_u32(b, o)
        }
        fn write_u32(&mut self, b: Bdf, o: u8, v: u32) {
            if o == 0x10 {
                self.0.write_u32(b, o, 0xdead_0000);
            } else {
                self.0.write_u32(b, o, v);
            }
        }
    }
    impl crate::ecam::ExtendedConfigSpace for Corrupt {
        fn read_dword(
            &mut self,
            segment: u16,
            bdf: Bdf,
            offset: u16,
        ) -> Result<u32, crate::ecam::EcamError> {
            self.0.read_dword(segment, bdf, offset)
        }
        fn write_dword(
            &mut self,
            segment: u16,
            bdf: Bdf,
            offset: u16,
            value: u32,
        ) -> Result<(), crate::ecam::EcamError> {
            if offset == 0x10 {
                let _ = self.0.write_dword(segment, bdf, offset, 0xdead_0000);
            } else {
                self.0.write_dword(segment, bdf, offset, value)?;
            }
            Ok(())
        }
    }
    let mut m = Machine::new();
    let mut r = endpoint();
    r.values[1] = 7;
    m.add(bdf(0, 3, 0), r);
    let p = proposal(&mut m).unwrap();
    let mut fault = Corrupt(m);
    assert_eq!(p.apply(&mut fault), Err(Error::Rollback));
    assert_eq!(fault.read_u32(bdf(0, 3, 0), 4) & 7, 0);
}

#[test]
fn command_disable_refusal_never_writes_bar_assignments() {
    let mut m = Machine::new();
    let mut r = endpoint();
    r.values[1] = 7;
    m.add(bdf(0, 3, 0), r);
    let p = proposal(&mut m).unwrap();
    let mut fault = Refuse {
        machine: m,
        fail: Some((bdf(0, 3, 0), 4)),
        permanent: true,
    };
    let before = fault.machine.writes.len();
    assert_eq!(p.apply(&mut fault), Err(Error::Rollback));
    assert!(fault.machine.writes[before..].is_empty());
    assert_eq!(fault.read_u32(bdf(0, 3, 0), 0x10), 0);
}

#[test]
fn disjoint_firmware_apertures_never_allocate_across_holes_or_double_own_overlap() {
    let mut pool = Pool::from_ranges(&[
        Range::new(0xc0000000, 0x100000).unwrap(),
        Range::new(0x80000000, 0x100000).unwrap(),
    ])
    .unwrap();
    assert_eq!(pool.allocate(0x100000).unwrap().start, 0x80000000);
    assert_eq!(pool.allocate(0x100000).unwrap().start, 0xc0000000);
    assert_eq!(pool.allocate(4096), Err(Error::NoSpace));
    assert!(matches!(
        Pool::from_ranges(&[
            Range::new(0x80000000, 0x200000).unwrap(),
            Range::new(0x80100000, 0x100000).unwrap()
        ]),
        Err(Error::Overlap)
    ));
    assert!(matches!(
        Pool::from_ranges(&[Range { start: 1, end: 1 }]),
        Err(Error::InvalidRange)
    ));
    assert_eq!(
        Pool::from_ranges(&[]).unwrap().allocate(4096),
        Err(Error::NoSpace)
    );
}
#[test]
fn reservations_split_firmware_windows_without_joining_separate_ranges() {
    let mut pool = Pool::from_ranges(&[
        Range::new(0x80000000, 0x1000000).unwrap(),
        Range::new(0xc0000000, 0x1000000).unwrap(),
    ])
    .unwrap();
    pool.reserve(Range::new(0x80000000, 0x1000000).unwrap())
        .unwrap();
    pool.reserve(Range::new(0xc0000000, 0x400000).unwrap())
        .unwrap();
    assert_eq!(pool.allocate(0x400000).unwrap().start, 0xc0400000);
    assert_eq!(pool.allocate(0x1000000), Err(Error::NoSpace));
}
