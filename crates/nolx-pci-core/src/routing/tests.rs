use super::*;
use crate::{
    enumerate,
    tests::{Machine, Regs},
};
fn b(bus: u8, device: u8) -> Bdf {
    Bdf {
        bus,
        device,
        function: 0,
    }
}
fn row(bus: u8, device: u8, pin: u8, route: usize) -> Row {
    Row {
        bus,
        device,
        function: None,
        pin,
        route,
    }
}
fn topology() -> (Machine, Inventory) {
    let mut cfg = Machine::new();
    let mut outer = Regs::bridge(1, 2, 1);
    outer.values[6] = 0x00020100;
    let mut inner = Regs::bridge(1, 3, 2);
    inner.values[6] = 0x00020201;
    cfg.add(b(0, 3), outer);
    cfg.add(b(1, 5), inner);
    cfg.add(b(2, 6), Regs::endpoint(1, 4, 2, 0, false));
    let inv = enumerate(&mut cfg);
    (cfg, inv)
}
#[test]
fn scope_mapping_follows_numbered_bridges_not_namespace_slot_guesses() {
    let (mut cfg, inv) = topology();
    assert_eq!(scope_bus(&mut cfg, &inv, &[3 << 16, 5 << 16]), Ok(2));
    assert_eq!(scope_bus(&mut cfg, &inv, &[4 << 16]), Err(Error::Topology));
    assert_eq!(
        scope_bus(&mut cfg, &inv, &[0x30000 | 8]),
        Err(Error::Topology)
    );
    assert!(cfg.writes.is_empty());
}
#[test]
fn nested_swizzle_uses_each_child_slot_and_never_interrupt_line() {
    let (mut cfg, inv) = topology();
    // INTA from slot6 -> INTC at inner bridge slot5 -> INTD at outer slot3.
    assert_eq!(
        lookup(&mut cfg, &inv, &[row(0, 3, 3, 77)], b(2, 6), 1),
        Ok(77)
    );
    assert_eq!(
        lookup(&mut cfg, &inv, &[row(1, 5, 2, 88)], b(2, 6), 1),
        Ok(88)
    );
    assert!(cfg.writes.is_empty());
}
#[test]
fn described_child_missing_row_does_not_bypass_its_table() {
    let (mut cfg, inv) = topology();
    assert_eq!(
        lookup(
            &mut cfg,
            &inv,
            &[row(1, 4, 0, 1), row(0, 3, 3, 2)],
            b(2, 6),
            1
        ),
        Err(Error::Missing)
    );
}
#[test]
fn invalid_pin_duplicate_rows_and_incomplete_inventory_refuse() {
    let (mut cfg, mut inv) = topology();
    for pin in [0, 5, 255] {
        assert_eq!(
            lookup(&mut cfg, &inv, &[], b(2, 6), pin),
            Err(Error::Topology)
        );
    }
    let rows = [
        row(2, 6, 0, 1),
        Row {
            function: Some(0),
            ..row(2, 6, 0, 2)
        },
    ];
    assert_eq!(
        lookup(&mut cfg, &inv, &rows, b(2, 6), 1),
        Err(Error::Ambiguous)
    );
    inv.overflowed = true;
    assert_eq!(
        lookup(&mut cfg, &inv, &rows, b(2, 6), 1),
        Err(Error::Topology)
    );
    inv.overflowed = false;
    inv.refused_bridges = 1;
    assert_eq!(scope_bus(&mut cfg, &inv, &[]), Err(Error::Topology));
}
#[test]
fn duplicate_parents_and_bus_cycles_are_not_guessed() {
    let (mut cfg, _) = topology();
    let mut alias = Regs::bridge(1, 5, 2);
    alias.values[6] = 0x00020200;
    cfg.add(b(0, 7), alias);
    let inv = enumerate(&mut cfg);
    assert_eq!(
        lookup(&mut cfg, &inv, &[], b(2, 6), 1),
        Err(Error::Ambiguous)
    );
}
