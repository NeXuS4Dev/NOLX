# Memory Manager

## Address space layout (x86_64, 4-level paging)

```
0xffff_ffff_ffff_ffff  +----------------------------------+
                       | recursive PML4 slot (debug only) |
0xffff_ff80_0000_0000  +----------------------------------+
                       | kernel: direct map of all frames |
                       |         paged/nonpaged pool      |
                       |         system PTEs, modules     |
                       |         kernel image (KASLR)     |
0xffff_8000_0000_0000  +----------------------------------+
                       |            non-canonical hole    |
0x0000_7fff_ffff_ffff  +----------------------------------+
                       | user: stack (grows down), ASLR   |
                       |       mmap region                |
                       |       heap (brk + mmap)          |
                       |       executable + libraries     |
0x0000_0000_0000_0000  +----------------------------------+
```

Milestone M1/M2 run identity-mapped at physical 1 MiB for simplicity; M3 introduces the
higher-half mapping above and turns identity mapping off. The Rust kernel is built with a
target spec that matches whichever model is active, so there is one switch, not two.

## Physical frames

* Boot builds a frame map from the multiboot memory map: reserved regions (BIOS data,
  ACPI NVS, kernel image, initrd, framebuffers) are marked unusable.
* Allocator: **buddy allocator** per NUMA node, with per-CPU magazines (free lists of
  single frames) so the common case is lock-free at `DISPATCH` level.
* Reference counts live in a `struct page`-equivalent array indexed by frame number
  (memmap), allocated at boot from the direct map.
* Frame states: `free`, `allocated`, `page-table`, `slab`, `reserved`, `zeroed` (a
  per-node free list of already-zeroed frames, because every user mapping must be zeroed
  and zeroing at fault time is a measurable cost).

## Virtual memory: the VAD tree

Each process address space is described by a **VAD tree** — an AVL tree of virtual
address descriptors ordered by start address, as in the executive model, rather than a
flat list of VMAs.

A VAD carries:

| Field | Meaning |
|---|---|
| `start`, `end` | half-open range, page-aligned |
| `protection` | current protection, and the *maximum* protection permitted |
| `type` | `private`, `mapped-file`, `shared`, `stack`, `guard`, `free` |
| `backing` | section object + offset for mapped files |
| `flags` | `MAP_LOCKED`, `MAP_NOCORE`, `MAP_GROWSDOWN`, `PROT_EXEC` denied, ... |
| `commit_charge` | how much of the range counts against the process quota |

Why a tree: `mmap`/`munmap`/`mprotect`/`mincore`/`msync` are range operations, and POSIX
requires `mprotect` to split an existing mapping. Range split/merge/coalesce on an AVL
tree is `O(log n)` with well-defined invariants, and — importantly — those invariants are
testable on the host (`crates/nolx-vfs` has no dependency on hardware, and the VAD tree
lives behind a trait so `cargo test` exercises the real code with fake page tables).

Invariants, all asserted in debug builds and property-tested in T1:

1. VADs never overlap.
2. Adjacent VADs with identical attributes are coalesced.
3. The tree is height-balanced; height ≤ 2·log₂(n+1).
4. Every VAD's protection is a subset of its maximum protection.
5. Committed pages never exceed the process commit limit.

## Page faults

One handler, four causes:

| Cause | Action |
|---|---|
| first touch of committed private page | allocate zeroed frame, map, count a soft fault |
| write to a shared read-only COW page | allocate, copy, remap writable |
| access inside a `mapped-file` VAD | ask the Cache Manager for the page, map it |
| stack guard-page hit | extend the stack VAD if below `RLIMIT_STACK`, else `SIGSEGV` |

Everything else becomes a POSIX signal (`SIGSEGV` with `si_addr`, or `SIGBUS` for
alignment/`msync` cases) or, for kernel-mode faults, a bug check with a full frame dump.

## Pools and accounting

* **Nonpaged pool** — always resident; used for anything touched above `PASSIVE`.
* **Paged pool** — pageable; illegal above `PASSIVE`.
* **System PTE pool** — for transient kernel mappings and I/O.
* **Commit charge** — per-process and system-wide limits; `RLIMIT_AS`/`RLIMIT_DATA` and
  `getrusage` are read from these counters, so they cannot disagree.
* **Working sets** — per-process trimming targets with an aged-page model; `mlock`/
  `mlockall` pin pages and are accounted against `RLIMIT_MEMLOCK`.

## Zeroing and security

Every frame handed to a user mapping is zeroed before it is mapped, without exception.
Frames freed from a user mapping are queued for zeroing at low priority rather than
zeroed inline, so the cost is hidden. Kernel heap frees poison memory in debug builds.
