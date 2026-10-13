# Assertions: NOLXFS — COW container, checkpoints, snapshots, space manager

Source: NOLX M8 (`ROADMAP.md` §M8 and `docs/fs/nolxfs-format.md`), implemented in `crates/nolxfs` with the VFS adapter in `crates/nolx-vfs`.

Rows are pipe-delimited: `ID | specification reference | status | test`.
Statuses: `unimplemented`, `implemented`, `tested-T1`, `pending-T2`, `tested-T2`, `tested-T3`.
A `tested-*` row that names no test is rejected by the generator.

NXFS-001 | NOLXFS format §3–§6: mkfs→mount→create→write→fsync→remount round trip on an in-memory device returns byte-identical data | implemented | crates/nolxfs volume::tests::create_write_read_round_trip_survives_remount
NXFS-002 | NOLXFS format §7: crash injection at every write boundary of a commit recovers to the last committed checkpoint (descriptor written last is the atomic commit point; no journal replay) | implemented | crates/nolxfs volume::tests::crash_at_every_write_boundary_keeps_last_commit
NXFS-003 | NOLXFS format §7: a torn newest checkpoint descriptor is skipped and the previous valid checkpoint is selected on mount | implemented | crates/nolxfs checkpoint::tests (torn-descriptor fallback), fsck::tests::torn_checkpoint_descriptor_is_found_and_cleared
NXFS-004 | NOLXFS format §5: the fs-tree B-tree splits on insert and merges under-filled nodes on remove, shrinking a single-child root, without losing records | implemented | crates/nolxfs btree::tests, volume::tests::links_rename_unlink_and_rmdir_rules
NXFS-005 | NOLXFS format §4: the object map resolves (oid, xid) by floor lookup so snapshots pin historical roots while newer versions shadow them | implemented | crates/nolxfs omap::tests, volume::tests::snapshot_is_isolated_from_later_writes
NXFS-006 | NOLXFS format §8: the space manager allocates/frees through chunk-info bitmaps and the reaper only reclaims blocks whose garbage xid is at or below the oldest live snapshot xid | implemented | crates/nolxfs spaceman::tests, volume::tests::snapshot_is_isolated_from_later_writes
NXFS-007 | NOLXFS format §8: snapshot creation pins the fs/extent-reference trees; later COW writes are invisible to the snapshot, and mount-at-snapshot is read-only | implemented | crates/nolxfs volume::tests::snapshot_is_isolated_from_later_writes, crates/nolx-vfs nolxfs_vfs::tests::links_symlinks_rename_and_snapshots_through_the_adapter
NXFS-008 | NOLXFS format §6: hard links share one inode via dentry + sibling-link records, nlink tracks the link count, and unlink of one name leaves the data readable through the other | implemented | crates/nolxfs volume::tests::links_rename_unlink_and_rmdir_rules
NXFS-009 | NOLXFS format §6: rename moves a dentry across directories, updates parent_id and directory nlink, replaces non-directory targets, and refuses to move a directory into its own descendant | implemented | crates/nolxfs volume::tests::links_rename_unlink_and_rmdir_rules
NXFS-010 | NOLXFS format §6: symlinks store their target through the extent machinery, xattrs round-trip, truncate shrinks and extends, and unwritten holes read as zeroes | implemented | crates/nolxfs volume::tests::symlinks_xattrs_truncate_and_holes
NXFS-011 | NOLXFS format §6: clone_file shares physical extents under per-block reference counts and a later write to either file copy-on-writes without disturbing the other | implemented | crates/nolxfs volume::tests::clone_file_shares_then_diverges
NXFS-012 | NOLXFS format §9: fsck verifies checksums, nlink, extent refcounts, bitmaps, parents, timestamps and counters on a healthy volume and reports it clean | implemented | crates/nolxfs volume::tests::fsck_reports_healthy_volume_clean_and_dump_reads_it
NXFS-013 | NOLXFS format §9: fsck finds injected corruption and repairs the safe classes (torn descriptors cleared, leaked bitmap rebuilt) leaving a mountable volume | implemented | crates/nolxfs fsck::tests::torn_checkpoint_descriptor_is_found_and_cleared, fsck::tests::leaked_bitmap_block_is_found_and_rebuilt
NXFS-014 | NOLXFS format §9: nolxfs-dump renders container, checkpoint, spaceman and tree state read-only without mounting | implemented | crates/nolxfs volume::tests::fsck_reports_healthy_volume_clean_and_dump_reads_it (dump_to_string), tools/nolxfs-dump
NXFS-015 | NOLX M8 VFS: the VFS dispatches rmdir/link/symlink/readlink/unlink/rename/truncate to NOLXFS (previously ENOTSUP) and the kernel boot self-test exercises them on /mnt/nolxfs | pending-T2 | `.github/workflows/ci.yml` boot-smoke asserts `nolxfs ok, nolxfs-m8 ok; audit ok` in the `NOLX: vfs selftest:` marker
