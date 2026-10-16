# Assertions: NOLX driver framework (nolx-df)

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, Networking: "a driver framework (`nolx-df`) so
third-party drivers are a device object + dispatch table, not kernel patches") and `crates/nolx-df-core`.
These rules are NOLX's own design, not a published specification. The rules cover device identity,
driver match rules, binding order, the device lifecycle, the dispatch table, and registry capacity. The
kernel binds five native PCI driver kinds through a persistent shared registry. Multi-instance
contexts, failed-initialization cleanup and reusable MMIO/IRQ resources remain open.

Rows are pipe-delimited: `ID | specification reference | status | test`.

DF-001 | NOLX M9 Match rules: a rule compares only the fields it sets (vendor and device, or class, subclass, and optionally programming interface), and a rule that sets a different field does not match | tested-T1 | nolx-df-core tests::a_match_rule_compares_only_the_fields_it_sets
DF-002 | NOLX M9 Binding: a device binds to the first registered driver that has a matching rule and whose probe accepts the device; a device no driver accepts stays discovered and can bind when a suitable driver is added later | tested-T1 | nolx-df-core tests::a_device_binds_to_the_first_driver_whose_probe_accepts_it, tests::a_device_with_no_accepting_driver_stays_discovered_and_can_bind_later
DF-003 | NOLX M9 Lifecycle: a device must be bound before it starts, a started device stops, a stopped device starts again, and a bound device cannot be bound again | tested-T1 | nolx-df-core tests::the_lifecycle_moves_through_start_stop_and_start_again
DF-004 | NOLX M9 Failure: a driver start that reports failure makes the device Failed, which cannot start again and can only be removed | tested-T1 | nolx-df-core tests::a_failed_start_marks_the_device_failed_and_only_allows_removal
DF-005 | NOLX M9 Removal: a started device is refused removal until it is stopped; a removed device is terminal and cannot start | tested-T1 | nolx-df-core tests::a_started_device_cannot_be_removed_until_it_is_stopped
DF-006 | NOLX M9 Capacity and handles: the registry refuses a driver or device beyond its fixed capacity, and an operation on a handle that names no device fails | tested-T1 | nolx-df-core tests::the_registry_refuses_work_beyond_its_capacity_and_unknown_handles
DF-007 | NOLX M9 Dispatch: the driver's start operation receives the handle of the device being started | tested-T1 | nolx-df-core tests::the_dispatch_table_receives_the_device_handle

DF-001 through DF-007 are `tested-T1` (original CI workflow 38614702136).


DF-008 | NOLX M9 Persistent healthy native lifecycle: virtio-blk, AHCI, NVMe, e1000 and virtio-net retain state beyond probe return, reclaim and reacquire exactly their owned DMA on three stop/restart cycles, verify fresh I/O, refuse running removal and refuse restart after removal; no claim of failed-start cleanup or MMIO unmapping | tested-T2 | scripts/check-driver-lifecycle.py; kernel/src/hal/driver_start.rs lifecycle_selftest; CI 38968513895 Persistent five-driver hardware lifecycle on 66c5d8e, five isolated fixtures plus one of each kind together, each ends with zero pinned DMA pages
DF-009 | NOLX M9 Framework exclusion: recursive framework and public block-I/O entry from a start callback return Busy rather than waiting under the registry IRQ lock | tested-T2 | kernel/src/hal/driver_start.rs lifecycle_selftest reenter; scripts/check-driver-lifecycle.py; CI 38968513895 Persistent five-driver hardware lifecycle on 66c5d8e
DF-010 | NOLX M9 Stable driver identity: driver lookup and bound-device driver names remain valid across stop, restart and removal; unbound and unknown handles report errors | tested-T1 | nolx-df-core tests::driver_identity_survives_stop_restart_and_removal; CI 38968513895 Host tests on 66c5d8e
