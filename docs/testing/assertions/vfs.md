# Assertions: pathname resolution, mount table, descriptors, and filesystems

Source: *The Open Group Base Specifications, Issue 8*, System Interfaces volume, and NOLX M7 (`ROADMAP.md` and `crates/nolx-vfs`).

Rows are pipe-delimited: `ID | specification reference | status | test`.
Statuses: `unimplemented`, `implemented`, `tested-T1`, `pending-T2`, `tested-T2`, `tested-T3`.
A `tested-*` row that names no test is rejected by the generator.

PN-001 | XBD 4.12 Pathname Resolution: a pathname beginning with a slash resolves against the root | tested-T1 | crates/nolx-vfs path::tests::leading_slash_ignores_the_start_node
PN-002 | XBD 4.12 Pathname Resolution: the empty pathname is invalid | tested-T1 | crates/nolx-vfs path::tests::empty_path_is_enoent
PN-003 | XBD 4.12 Pathname Resolution: the last component may be empty, and then refers to the directory itself | tested-T1 | crates/nolx-vfs path::tests::trailing_slash_requires_a_directory
PN-004 | XBD 4.12 Pathname Resolution: a slash may appear as the last two bytes of a pathname, and then the pathname shall not resolve | tested-T1 | crates/nolx-vfs path::tests::dot_dot_and_dot
PN-005 | XBD 4.12 Pathname Resolution: a pathname longer than PATH_MAX bytes fails with ENAMETOOLONG | tested-T1 | crates/nolx-vfs path::tests::path_too_long_is_enametoolong
PN-006 | XBD 4.12 Pathname Resolution: any single component longer than NAME_MAX fails with ENAMETOOLONG | tested-T1 | crates/nolx-vfs path::tests::component_too_long_is_enametoolong
PN-007 | XBD 4.12 Pathname Resolution: dot refers to the directory itself | tested-T1 | crates/nolx-vfs path::tests::dot_is_a_no_op
PN-008 | XBD 4.12 Pathname Resolution: dot-dot refers to the parent directory | tested-T1 | crates/nolx-vfs path::tests::dot_dot_and_dot
PN-009 | XBD 4.12 Pathname Resolution: dot-dot at the root resolves to the root | tested-T1 | crates/nolx-vfs path::tests::dot_dot_at_the_root_is_the_root
PN-010 | XBD 4.11 Pathname: successive slashes are treated as a single slash | tested-T1 | crates/nolx-vfs path::tests::repeated_slashes_collapse
PN-011 | XSH 4.12 Pathname Resolution: a loop of symbolic links shall fail with ELOOP | tested-T1 | crates/nolx-vfs path::tests::symlink_loop_is_eloop
PN-012 | XSH 4.12 Pathname Resolution: more than SYMLOOP_MAX links in a pathname shall fail with ELOOP | tested-T1 | crates/nolx-vfs path::tests::symlink_depth_limit_is_enforced
PN-013 | XSH 4.12 Pathname Resolution: a symbolic link is followed unless it is the final component and O_NOFOLLOW is set | tested-T1 | crates/nolx-vfs path::tests::no_follow_stops_at_the_last_component
PN-014 | XSH 4.12 Pathname Resolution: search permission is required on every prefix directory | tested-T1 | crates/nolx-vfs path::tests::missing_execute_is_eacces
PN-015 | XSH 4.12 Pathname Resolution: a nonexistent intermediate component fails with ENOENT | tested-T1 | crates/nolx-vfs path::tests::missing_component_is_enoent
PN-016 | XSH 4.12 Pathname Resolution: an intermediate component that is not a directory fails with ENOTDIR | tested-T1 | crates/nolx-vfs path::tests::not_a_directory_is_enotdir
PN-017 | XBD 3.222 Mount Point: a pathname naming a mounted directory resolves into the mounted filesystem | tested-T1 | crates/nolx-vfs mount::tests::crossing_down_replaces_the_covered_directory
PN-018 | XBD 3.222 Mount Point: dot-dot from the root of a mounted filesystem resolves into the covering directory | tested-T1 | crates/nolx-vfs mount::tests::crossing_up_returns_the_covered_node
PN-019 | XSH 2.3 Error Numbers: two filesystems may both use the same inode number, so identity includes the mount | tested-T1 | crates/nolx-vfs mount::tests::node_ids_are_scoped_by_mount
PN-020 | XSH fstat: the device and inode of an open file identify it | tested-T1 | crates/nolx-vfs mount::tests::mount_over_replaces_the_root
PN-021 | XSH pathconf / fpathconf: _PC_LINK_MAX, _PC_NAME_MAX, _PC_PATH_MAX, _PC_PIPE_BUF, _PC_CHOWN_RESTRICTED, _PC_NO_TRUNC, _PC_VDISABLE, and _PC_SYMLINK_MAX return the filesystem limits and reject unknown names with EINVAL | tested-T1 | crates/nolx-vfs path::tests::pathconf_reports_posix_limits_and_rejects_unknown_names
PN-022 | XBD 4.12 Pathname Resolution: a chroot or pivot-root directory confines both `/` and `..` traversal to the process root | tested-T1 | crates/nolx-vfs path::tests::chroot_confines_absolute_and_dot_dot_resolution

MNT-001 | NOLX M7 Mount Propagation: shared peer groups replicate submounts and unmounts, slave mounts receive master events one-way, private mounts isolate events, and unbindable mounts refuse bind operations with EINVAL | tested-T1 | crates/nolx-vfs mount::tests::shared_peer_group_propagates_submounts_and_unmounts, slave_propagation_is_one_way_from_master_to_slave, private_and_unbindable_mounts_enforce_isolation
MNT-002 | NOLX M7 Pivot Root: pivot_root swaps the root mount and moves the previous root under put_old inside the new root | tested-T1 | crates/nolx-vfs mount::tests::pivot_root_moves_old_root_under_put_old

FD-001 | XSH 2.5.1 Descriptor Tables and Files: the lowest unused descriptor is allocated | tested-T1 | crates/nolx-vfs fd::tests::lowest_free_descriptor_is_allocated
FD-002 | XSH 2.5.1 Descriptor Tables and Files: allocating past OPEN_MAX fails with EMFILE | tested-T1 | crates/nolx-vfs fd::tests::descriptor_limit_is_emfile
FD-003 | XSH close: closing an invalid descriptor fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::closing_an_invalid_descriptor_is_ebadf
FD-004 | XSH dup2: the descriptor is closed first if it was open, then reused | tested-T1 | crates/nolx-vfs fd::tests::dup2_closes_the_target_first
FD-005 | XSH dup2: if old and new are equal, nothing is closed and new is returned | tested-T1 | crates/nolx-vfs fd::tests::dup2_same_descriptor_is_a_no_op
FD-006 | XSH dup2: dup2 past OPEN_MAX fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::dup2_out_of_range_is_ebadf
FD-007 | XSH fcntl F_DUPFD: the new descriptor is the lowest available at or above the argument | tested-T1 | crates/nolx-vfs fd::tests::f_dupfd_skips_lower_descriptors
FD-008 | XSH 2.5.1 Descriptor Tables and Files: duplicated descriptors share the file offset | tested-T1 | crates/nolx-vfs fd::tests::dup_shares_the_file_and_offset
FD-009 | XSH 2.5.1 Descriptor Tables and Files: descriptors 0, 1, and 2 are established before a program starts | tested-T1 | crates/nolx-vfs fd::tests::stdio_descriptors_are_created
FD-010 | XSH fcntl F_SETFD: FD_CLOEXEC survives until exec | tested-T1 | crates/nolx-vfs fd::tests::close_on_exec_round_trips
FD-011 | XSH fcntl F_GETFD: reading the flags of an invalid descriptor fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::flags_of_a_missing_descriptor
FD-012 | XSH 2.5.1 Descriptor Tables and Files: each descriptor holds its own flags, so dup does not share them | tested-T1 | crates/nolx-vfs fd::tests::descriptor_flags_are_per_descriptor
FD-013 | XSH fcntl F_DUPFD_CLOEXEC and F_GETFL/F_SETFL: F_DUPFD_CLOEXEC sets FD_CLOEXEC atomically, and F_SETFL updates mutable status flags while preserving access mode | tested-T1 | crates/nolx-vfs fd::tests::f_dupfd_cloexec_sets_close_on_exec_atomically, f_setfl_updates_status_flags_but_preserves_access_mode

UN-001 | XSH unlink: an entry is removed but an open file remains accessible until the last descriptor closes | tested-T1 | crates/nolx-vfs vfs::tests::un001_unlink_removes_entry_while_open_file_remains_accessible_until_last_close
UN-002 | XSH link: creating a hard link to a directory fails with EPERM unless privileged | tested-T1 | crates/nolx-vfs vfs::tests::un002_hard_link_to_directory_fails_with_eperm
UN-003 | XSH rename: renaming a directory onto one of its own descendants fails with EINVAL | tested-T1 | crates/nolx-vfs vfs::tests::un003_rename_directory_into_own_descendant_fails_with_einval
UN-004 | XSH rmdir: removing a non-empty directory fails with ENOTEMPTY | tested-T1 | crates/nolx-vfs vfs::tests::un004_rmdir_non_empty_directory_fails_with_enotempty
UN-005 | XSH mkdir: creating an existing path fails with EEXIST | tested-T1 | crates/nolx-vfs vfs::tests::un005_mkdir_existing_path_fails_with_eexist

FS-001 | NOLX M7 devfs: /dev/null discards writes and reads EOF, /dev/zero reads NUL bytes, /dev/full fails writes with ENOSPC, /dev/console records console output, and mknod registers custom character/block nodes | tested-T1 | crates/nolx-vfs devfs::tests::null_discards_writes_and_reads_eof, zero_reads_zero_bytes_at_any_offset, full_reads_zeroes_and_rejects_writes_with_enospc, console_captures_written_bytes
FS-002 | NOLX M7 tmpfs: bounded in-memory POSIX filesystem supports regular files, directories, symlinks, hard links, rename, unlink-while-open, truncate, and enforces inode/byte capacity with ENOSPC | tested-T1 | crates/nolx-vfs tmpfs::tests::create_write_read_and_truncate_regular_file, unlink_open_file_keeps_data_until_last_handle_closes, rename_moves_entry_and_rejects_descendant_directory_move, byte_and_inode_quotas_return_enospc
FS-003 | NOLX M7 initrd: read-only initial ramdisk parses both SVR4 newc cpio and POSIX ustar tar archives, synthesizes intermediate directories, resolves symlinks, and rejects mutations with EROFS | tested-T1 | crates/nolx-vfs initrd::tests::cpio_newc_parses_directories_files_and_symlinks, ustar_tar_parses_files_directories_and_symlinks, auto_detection_accepts_cpio_and_tar_and_mutations_return_erofs
FS-004 | NOLX M7/M8 NOLXFS VFS: formats, probes, mounts, and persists directories, regular file extents, hard links, symlinks, unlinks, renames and snapshots through copy-on-write checkpoints with Fletcher-64 checksum verification | tested-T1 | crates/nolx-vfs nolxfs_vfs::tests::format_mount_create_write_unmount_and_remount_persists_on_disk, links_symlinks_rename_and_snapshots_through_the_adapter
FS-005 | NOLX M7/M8 VFS Boot Self-Test: kernel mounts tmpfs (/), devfs (/dev), initrd (/initrd), and NOLXFS (/mnt/nolxfs), verifies cross-mount resolution and I/O at boot, and exercises the M8 link/symlink/rename/rmdir/unlink operations on NOLXFS | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserted `NOLX: vfs selftest: mounted tmpfs(/), devfs(/dev), initrd(/initrd), nolxfs(/mnt/nolxfs); tmpfs write/read 32 bytes verified=true, devfs ok, initrd ok, nolxfs ok, nolxfs-m8 ok; audit ok`; workflow [38359528564](https://github.com/NeXuS4Dev/NOLX/actions/runs/38359528564) on revision `be7730a` passed.
FS-006 | NOLX M7 Ring-3 tmpfs T2: ring-3 init opens /tmp/m7-check with O_CREAT+O_EXCL+O_RDWR, writes 28 bytes, seeks to 0, reads back 28 bytes, closes, unlinks, and prints the payload to stdout | tested-T2 | `.github/workflows/ci.yml` boot-smoke asserted `tmpfs: ring-3 read/write ok` and `NOLX: tmpfs T2: ring-3 open/write/lseek/read/close/unlink on /tmp/m7-check (28 bytes round-tripped, verified=true)`; workflow [38240528081](https://github.com/NeXuS4Dev/NOLX/actions/runs/38240528081) on revision `6dfedca` passed.
