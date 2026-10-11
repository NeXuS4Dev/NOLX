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

M1/M2 ran identity-mapped at physical 1 MiB. M3 landed the switch: the image is linked at
`KERNEL_IMAGE_BASE = 0xffff_ffff_8000_0000` -- the top 2 GiB, where the `kernel` code model
reaches statics with 32-bit sign-extended absolute addressing -- and the boot trampoline
maps it there in PML4 slot 511 before entering long mode. The direct map covers the first
2 GiB of RAM at `DIRECT_MAP_BASE = 0xffff_8000_0000_0000` (PML4 slot 256), and the standard
LAPIC and HPET MMIO windows are mapped through the same tables with caching disabled, so
they stay reachable after the identity map is gone. Slot 510 is reserved for the debugger's
recursive mapping.

The kernel builds its own GDT, IDT, and stack in the image window during `hal::init`, and
then drops the boot identity map (`mm::tables::drop_identity_map`, the first thing
`nolx_kernel_main` does after the HAL is up). Dropping it is verified rather than assumed:
the switch reports how many PML4 entries went away and probes three addresses -- one that
must still translate (a frame through the direct map), one in the image window, and one low
physical address that must now fault -- and the boot refuses to continue if any answer is
wrong. A physical address and a pointer are different numbers from that point on, which is
the property the rest of the memory manager is written against: every physical address
becomes a pointer through `mm::phys` and nowhere else.

The image layout is a flat-copy contract, not just a link-time one: a Multiboot loader that
uses the a.out kludge fields copies the image as one block, so every section with file
content sits at `file_offset = copy_base + (load_address - load_address_base)` and every
section after `_edata` is a NOBITS hole the loader zeroes. `scripts/verify-multiboot.py`
checks the property on the linked image, which is what keeps the boot layer's page tables
loadable by a loader that has never heard of ELF segments.

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
testable on the host: the tree lives in `crates/nolx-mm-core`, which has no hardware
dependency and no `unsafe`, so `cargo test` exercises the same code the kernel runs.

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

## Where the code lives

The parts that are pure logic live in `crates/nolx-mm-core`, where they run under
`cargo test` on an ordinary host: `buddy` (the allocator), `magazine` (per-CPU frame caches
and the zeroed-frame pool), `paging` (four-level arithmetic, entry encoding, and a walker
over a `TableStore`), `vad` (the descriptor tree), `commit` (quota and pool accounting), and
`heap` (the boundary-tagged kernel heap). The kernel keeps what must touch a machine: the
frame map, the reservations that describe the kernel's own footprint, and the `BlockStore`
implementation in `kernel/src/mm/store.rs`, which reaches the free-list links through the
boot layer's identity map. `kernel/src/mm/selftest.rs` stresses the result at boot, with
interrupts live, before anything else depends on it.

## Zeroing and security

Every frame handed to a user mapping is zeroed before it is mapped, without exception.
Frames freed from a user mapping are queued for zeroing at low priority rather than
zeroed inline, so the cost is hidden. Kernel heap frees poison memory in debug builds.
