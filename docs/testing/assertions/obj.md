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
