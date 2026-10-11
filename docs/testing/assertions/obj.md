# Assertions: object manager

Source: NOLX M4 -- `docs/architecture/01-kernel-model.md` section 3 (Object Manager) and the rules
in `crates/nolx-obj-core` (handle tables, namespace). Rows marked `tested-T1` are exercised by
`cargo test` on the host; rows marked `pending-T2` are asserted by the QEMU boot smoke test in
`.github/workflows/ci.yml` and are promoted to `tested-T2` only once a workflow run on this
revision has passed.

OBJ-001 | Handle values are generation-checked: a handle to a closed entry never names the entry that reuses its slot | tested-T1 | `crates/nolx-obj-core/src/handle.rs` tests `closed_handle_is_stale_after_slot_reuse` and `generation_wraps_without_reaching_zero`.
OBJ-002 | DuplicateHandle may narrow access but never widen it, and a refused duplicate inserts nothing | tested-T1 | `handle.rs` test `duplicate_may_narrow_but_not_widen`.
OBJ-003 | Inheritance copies only inheritable entries and keeps the parent's handle values where the child's slot is free | tested-T1 | `handle.rs` tests `inheritance_copies_only_inheritable_entries` and `inheritance_falls_back_when_the_slot_is_taken`.
OBJ-004 | Namespace lookup is case-sensitive, rejects malformed paths, and follows symbolic links with a loop limit | tested-T1 | `crates/nolx-obj-core/src/namespace.rs` tests, including `symlink_cycle_is_a_loop_not_a_hang`.
OBJ-005 | Removing a directory with children is refused; removing a link removes the link, not its target | tested-T1 | `namespace.rs` tests `remove_refuses_nonempty_directory_then_allows_empty` and `remove_takes_the_link_not_its_target`.
OBJ-006 | An object's references follow the rules: a handle implies a pointer reference, and the object is freed exactly when the last reference goes | tested-T2 | `kernel/src/obj/boot.rs` `selftest` drives one object through (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, checks each step, and requires the live count to return to its start. Boot marker `NOLX: obj selftest: 1 object, 2 handles` (CI run 38133425376, revision 35728d3, boot smoke green).
OBJ-007 | SeAccessCheck: a deny entry written before an allow refuses the rights it covers; the owner holds READ_CONTROL and WRITE_DAC implicitly; group membership and the everyone principal match; a null DACL grants and an empty DACL denies | tested-T1 | `crates/nolx-obj-core/src/security.rs` tests `deny_written_before_allow_wins_for_the_same_principal`, `owner_holds_read_control_and_write_dac_implicitly_but_not_read`, `group_membership_is_matched_through_the_token`, and `null_dacl_grants_everything_and_empty_dacl_denies_everything`.
OBJ-008 | Mandatory integrity labels refuse write-class rights to a lower token but not reads; BACKUP grants reads and RESTORE grants writes only where the DACL has not refused them | tested-T1 | `security.rs` tests `integrity_label_refuses_write_class_to_a_lower_token`, `backup_privilege_reads_what_the_dacl_does_not_grant`, and `restore_privilege_writes_but_does_not_override_a_deny`.
OBJ-009 | Every access decision made through `open` is recorded, and the audit totals stay exact after the bounded log wraps | tested-T1 | `security.rs` test `audit_log_keeps_exact_totals_past_its_capacity`; at boot the self test requires the audit delta to equal the number of checks.
OBJ-010 | At boot, `open` enforces the monitor's decisions for a SYSTEM, reader, refused, backup, and low-integrity token against a device and a labelled log, and the audit log agrees with what was asked | tested-T2 | `kernel/src/obj/boot.rs` `selftest`, part two. Boot marker `NOLX: srm selftest: 10 access checks (6 granted, 4 denied)`.
