//! Shared types for NOLX: the POSIX scalar types, `errno`, and the syscall ABI numbers.
//!
//! This crate is compiled into the kernel *and* linked by user-space libc and the host
//! tools, so it is the single place where the two sides can disagree -- which is why it
//! has no dependencies and why its tables are checked by tests rather than by hope.
//!
//! Behaviour is defined by *The Open Group Base Specifications, Issue 8*
//! (IEEE Std 1003.1-2024). Where a comment here and that document disagree, the document
//! wins and this file is a bug.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

pub mod errno;
pub mod scalars;
pub mod syscall;

/// NOLX release, reported by `uname(2)` as the release field.
pub const VERSION_MAJOR: u32 = 0;
/// See [`VERSION_MAJOR`].
pub const VERSION_MINOR: u32 = 1;
/// See [`VERSION_MAJOR`].
pub const VERSION_PATCH: u32 = 0;

/// The `sysname` field of `uname(2)`.
pub const SYSNAME: &str = "NOLX";

/// The kernel's ABI revision. The loader refuses an executable whose ABI revision is
/// newer than the kernel's, so a user-space built against a newer ABI fails loudly at
/// `exec` rather than misbehaving later.
pub const ABI_REVISION: u32 = 1;

/// Format the version the way `uname -r` must.
pub const fn version_string() -> &'static str {
    "0.1.0"
}
