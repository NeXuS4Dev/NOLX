//! The NOLX VFS: pathname resolution, mounts, and descriptors.
//!
//! This crate holds the parts of the filesystem layer whose behaviour POSIX specifies
//! sentence by sentence and whose bugs are the most confusing kind -- a shell script that
//! behaves differently on NOLX than on any other UNIX system. It is deliberately free of
//! hardware and of any particular on-disk format, so all of it is testable on a host.
//!
//! * [`path`] -- `namei`: the pathname resolution state machine.
//! * [`mount`] -- the mount table and what happens at a mount point.
//! * [`fd`] -- the per-process descriptor table.
//!
//! Errors here are POSIX `errno` values from [`nolx_types::errno`], because that is what
//! the caller must ultimately return and translating twice is how error codes get lost.

#![cfg_attr(not(test), no_std)]
#![deny(missing_docs)]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod fd;
pub mod mount;
pub mod path;

pub use crate::fd::{Descriptor, DescriptorTable};
pub use crate::mount::{MountTable, NodeId};
pub use crate::path::{resolve, Resolution, Resolver};
