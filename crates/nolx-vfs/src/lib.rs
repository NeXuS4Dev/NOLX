//! Virtual File System, I/O Manager, Cache Manager, and filesystems (`devfs`, `tmpfs`,
//! `initrd`, and `nolxfs`).
//!
//! This crate houses the host-testable core of Milestone 7 (`docs/architecture/02-posix-subsystem.md`
//! and `docs/architecture/04-io-manager-drivers.md`):
//!
//! * [`io`]: IORBs, stack locations, completion routines, cancellation, completion ports,
//!   driver objects, device stacks, and reference-tracked driver unload.
//! * [`cache`]: `SharedCacheMap`, `copy_read` with sequential read-ahead, `copy_write` with
//!   dirty-page tracking, the background lazy writer, and `MmFlushSection`.
//! * [`path`]: POSIX `namei` pathname resolution (`SYMLOOP_MAX`, `O_NOFOLLOW`, `PATH_MAX`,
//!   `NAME_MAX`, search permissions, `pathconf`).
//! * [`mount`]: mount table with downward/upward crossings, propagation classes (`Private`,
//!   `Shared`, `Slave`, `Unbindable`), bind mounts, and `pivot_root`.
//! * [`fd`]: per-process file descriptor table (`dup`, `dup2`, `fcntl(F_DUPFD)`, `FD_CLOEXEC`).
//! * [`devfs`]: `/dev/null`, `/dev/zero`, `/dev/full`, `/dev/console`, and custom char devices.
//! * [`tmpfs`]: bounded in-memory POSIX filesystem with deferred unlink reclamation (`UN-001`),
//!   hard links, symlinks, and atomic loop-checked `rename`.
//! * [`initrd`]: SVR4 ASCII `cpio` (`070701`/`070702`) and POSIX `ustar` `tar` reader.
//! * [`nolxfs_vfs`]: native NOLXFS volume driver backed by `crates/nolxfs` B-trees and extents.
//! * [`vfs`]: unified `VirtualFileSystem`, `FileObject`, and `ProcessVfsContext` (`umask`,
//!   `chdir`, `chroot`).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod cache;
pub mod devfs;
pub mod fd;
pub mod initrd;
pub mod io;
pub mod mount;
pub mod nolxfs_vfs;
pub mod path;
pub mod tmpfs;
pub mod vfs;
