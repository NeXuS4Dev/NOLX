# Assertions: pathname resolution and the mount table

Source: *The Open Group Base Specifications, Issue 8*, System Interfaces volume.

Rows are pipe-delimited: `ID | specification reference | status | test`.
Statuses: `unimplemented`, `implemented`, `tested-T1`, `tested-T2`, `tested-T3`.
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

FD-001 | XSH 2.5.1 Descriptor Tables and Files: the lowest unused descriptor is allocated | tested-T1 | crates/nolx-vfs fd::tests::lowest_free_descriptor_is_allocated
FD-002 | XSH 2.5.1 Descriptor Tables and Files: allocating past OPEN_MAX fails with EMFILE | tested-T1 | crates/nolx-vfs fd::tests::descriptor_limit_is_emfile
FD-003 | XSH close: closing an invalid descriptor fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::closing_an_invalid_descriptor_is_ebadf
FD-004 | XSH dup2: the descriptor is closed first if it was open, then reused | tested-T1 | crates/nolx-vfs fd::tests::dup2_closes_the_target_first
FD-005 | XSH dup2: if old and new are equal, nothing is closed and new is returned | implemented | -
FD-006 | XSH dup2: dup2 past OPEN_MAX fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::dup2_out_of_range_is_ebadf
FD-007 | XSH fcntl F_DUPFD: the new descriptor is the lowest available at or above the argument | tested-T1 | crates/nolx-vfs fd::tests::f_dupfd_skips_lower_descriptors
FD-008 | XSH 2.5.1 Descriptor Tables and Files: duplicated descriptors share the file offset | tested-T1 | crates/nolx-vfs fd::tests::dup_shares_the_file_and_offset
FD-009 | XSH 2.5.1 Descriptor Tables and Files: descriptors 0, 1, and 2 are established before a program starts | tested-T1 | crates/nolx-vfs fd::tests::stdio_descriptors_are_created
FD-010 | XSH fcntl F_SETFD: FD_CLOEXEC survives until exec | tested-T1 | crates/nolx-vfs fd::tests::close_on_exec_round_trips
FD-011 | XSH fcntl F_GETFD: reading the flags of an invalid descriptor fails with EBADF | tested-T1 | crates/nolx-vfs fd::tests::flags_of_a_missing_descriptor
FD-012 | XSH 2.5.1 Descriptor Tables and Files: each descriptor holds its own flags, so dup does not share them | tested-T1 | crates/nolx-vfs fd::tests::descriptor_flags_are_per_descriptor

UN-001 | XSH unlink: an entry is removed but an open file remains accessible until the last descriptor closes | unimplemented | -
UN-002 | XSH link: creating a hard link to a directory fails with EPERM unless privileged | unimplemented | -
UN-003 | XSH rename: renaming a directory onto one of its own descendants fails with EINVAL | unimplemented | -
UN-004 | XSH rmdir: removing a non-empty directory fails with ENOTEMPTY | unimplemented | -
UN-005 | XSH mkdir: creating an existing path fails with EEXIST | unimplemented | -
