//! Object Manager primitives that are tested on an ordinary host.
//!
//! What lives here:
//!
//! * [`handle`] -- the per-process handle table: generation-checked handle values, access
//!   masks, inheritance flags, and `DuplicateHandle` semantics. The kernel supplies the
//!   object pointers and the reference counting; this crate decides which handle refers to
//!   which entry and whether an operation is allowed.
//! * [`refcount`] -- the pointer and handle reference counts of an object: what each open,
//!   close, and unbind does to them, and when an object is deleted.
//! * [`namespace`] -- the object namespace: a tree of directories, objects, and symbolic
//!   links rooted at `\`, with lookup that follows links up to a fixed depth.
//! * [`security`] -- the Security Reference Monitor's rules: SIDs, ACLs, tokens, integrity
//!   labels, privileges, `SeAccessCheck`, and a bounded audit log.
//!
//! The crate is `no_std` and free of unsafe code, so every rule it enforces is a red test on
//! a laptop rather than a corrupted table in the middle of boot.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod handle;
pub mod namespace;
pub mod refcount;
pub mod security;
