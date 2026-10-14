# Assertions: NOLX driver framework (nolx-df)

Source: NOLX M9 -- `ROADMAP.md` (Devices and drivers, Networking: "a driver framework (`nolx-df`) so
third-party drivers are a device object + dispatch table, not kernel patches") and `crates/nolx-df-core`.
These rules are NOLX's own design, not a published specification. The rules cover device identity,
driver match rules, binding order, the device lifecycle, the dispatch table, and registry capacity. The
kernel does not use the framework yet; wiring it to the PCI bus is a later slice.

Rows are pipe-delimited: `ID | specification reference | status | test`.

DF-001 | NOLX M9 Match rules: a rule compares only the fields it sets (vendor and device, or class, subclass, and optionally programming interface), and a rule that sets a different field does not match | implemented | nolx-df-core tests::a_match_rule_compares_only_the_fields_it_sets
DF-002 | NOLX M9 Binding: a device binds to the first registered driver that has a matching rule and whose probe accepts the device; a device no driver accepts stays discovered and can bind when a suitable driver is added later | implemented | nolx-df-core tests::a_device_binds_to_the_first_driver_whose_probe_accepts_it, tests::a_device_with_no_accepting_driver_stays_discovered_and_can_bind_later
DF-003 | NOLX M9 Lifecycle: a device must be bound before it starts, a started device stops, a stopped device starts again, and a bound device cannot be bound again | implemented | nolx-df-core tests::the_lifecycle_moves_through_start_stop_and_start_again
DF-004 | NOLX M9 Failure: a driver start that reports failure makes the device Failed, which cannot start again and can only be removed | implemented | nolx-df-core tests::a_failed_start_marks_the_device_failed_and_only_allows_removal
DF-005 | NOLX M9 Removal: a started device is refused removal until it is stopped; a removed device is terminal and cannot start | implemented | nolx-df-core tests::a_started_device_cannot_be_removed_until_it_is_stopped
DF-006 | NOLX M9 Capacity and handles: the registry refuses a driver or device beyond its fixed capacity, and an operation on a handle that names no device fails | implemented | nolx-df-core tests::the_registry_refuses_work_beyond_its_capacity_and_unknown_handles
DF-007 | NOLX M9 Dispatch: the driver's start operation receives the handle of the device being started | implemented | nolx-df-core tests::the_dispatch_table_receives_the_device_handle
