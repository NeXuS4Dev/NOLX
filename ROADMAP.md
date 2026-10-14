# NOLX Roadmap

Milestones are ordered by dependency, not by excitement. A milestone is **done** only when
(a) the code exists, (b) the tests listed under it pass in CI, and (c) the specification
assertions it claims are recorded in `docs/testing/assertions/`.

Verification tiers used below:

* **T1 host** — `cargo test` on the build machine (pure logic, `no_std`-compatible crates).
* **T2 boot** — QEMU boots the kernel and the serial log contains an expected marker.
* **T3 suite** — an external conformance/functional suite runs *on* NOLX (Open POSIX Test
  Suite, LTP-derived cases, our own `nolx-posix-tests`).

---

## M0 — Foundations (done)

Freeze the decisions that are expensive to change later.

- Kernel model: HAL / Executive / Kernel, IPL levels, driver+device objects,
  POSIX subsystem. → `docs/architecture/01-kernel-model.md`
- User↔kernel ABI: `syscall` instruction, register mapping, error convention, TLS.
  → `crates/nolx-syscall`, `docs/architecture/02-posix-subsystem.md`
- Syscall numbering scheme with a permanent reservation policy (numbers are never reused).
  → `crates/nolx-types/src/syscall.rs`
- NOLXFS on-disk format v1 frozen and generated from one source of truth.
  → `tools/nolxfs-codegen`, `docs/fs/nolxfs-format.md`
- Tests: T1 layout/offset invariants; boot layer assembles and links.

## M1 — Boot layer

Get from firmware to `nolx_kernel_main` in 64-bit long mode.

- Multiboot 1 header (QEMU `-kernel` fast path) + ELF loading.
- 32-bit entry → identity-map first 2 GiB with 2 MiB pages → PAE → `EFER.LME` → paging →
  far jump to 64-bit → 64-bit GDT → stack → call Rust.
- Multiboot info parsing: memory map (`mmap`), module list (initrd), command line.
- **M1b:** Multiboot 2 + native UEFI (EFI stub, GOP framebuffer, UEFI memory map, RT
  services hand-off, Secure Boot–compatible PE layout).
- Tests: T2 `NOLX: boot layer reached long mode`; T1 multiboot header validation.

## M2 — HAL: console, tables, interrupts, time (done)

- Serial (16550) + VGA text console behind one `HalConsole` trait; boot log from the first
  instruction that can print.
- 64-bit GDT (kernel/user code+data, TSS), IDT with 256 gates, ISR stubs preserving an
  `InterruptFrame`, exception dispatch with panic reporting.
- 8259 PIC remap, then APIC/x2APIC; EOI discipline tied to IPL.
- Timer: PIT calibration → LAPIC/HPET/deadline timer; monotonic and wall-clock sources.
- CPU feature detection (CPUID), `MSR` accessors, per-CPU data area (`gs` base).
- Tests: T2 timer ticks logged at expected rate; T2 `#PF`/`#GP` handler prints a register
  dump and halts cleanly.

### M2 implementation checkpoint — 2026-10-10

The single-CPU HAL now discovers ACPI RSDP/root tables, MADT topology, and HPET metadata; it
initializes the HPET counter when its register block is in the early uncached mapping; and it
programs the local APIC in xAPIC or x2APIC mode plus validated I/O APIC routes. MADT NMI
routes and ISA overrides are decoded, legacy redirections start masked, and the local APIC
TPR follows software IPL while APIC EOI is issued before IPL restoration. The APIC timer is
calibrated against HPET; an invariant-TSC deadline source is selected when available, with a
periodic LAPIC timer otherwise. The 8259/PIT remains a fallback when firmware, APIC, or
calibration setup is unavailable. HPET also backs monotonic time (including extension of
32-bit counter wraps); the RTC wall-clock anchor stores its matching monotonic baseline.

The host assertion database covers portable ACPI/APIC parsing and timer arithmetic. CI boots
the normal APIC/HPET timer path, asserts 100 timer interrupts, exercises a separate PIC/PIT
fallback boot with local APIC hidden, and boots `#PF`/`#GP` fault images. Those QEMU checks
are no longer pending: workflow run
[38086516018](https://github.com/NeXuS4Dev/NOLX/actions/runs/38086516018) on revision
`dc2d8d1` passed all six jobs -- boot layer, NOLXFS layout, host tests, fmt + clippy + unsafe
audit, kernel build, and the QEMU boot smoke test -- and the eight HAL assertions that were
`pending-T2` are recorded as `tested-T2` in `docs/testing/assertions/hal.md`. The HAL also
retains the earlier verified console contract, TSS/IST stacks, complete IDT,
register-preserving interrupt stubs, and bootstrap GS-based per-CPU block.

M2 is **done**: the code exists, its listed tests pass in CI, and its assertions are recorded
in the database.

### M2 verification checkpoint — 2026-10-10

The revision named above is the first on which every M2 claim has a passing machine record
behind it. The QEMU boot smoke test asserts the boot markers themselves -- not a summary of
them -- and the serial log for each run is archived as a build artifact and posted on the
pull request, so the evidence can be read rather than trusted.

## M3 — Memory management

- Physical frame allocator: boot-map from the multiboot memory map, buddy allocator with
  per-CPU magazines, NUMA-node aware node list.
- 4-level paging: kernel address space, per-process page tables, copy-on-write, demand
  paging, large-page promotion/demotion.
- Kernel heap: size-class segregated pools + a general allocator; `GlobalAlloc` impl.
- **Higher-half kernel mapping** and switch off identity mapping (KASLR-ready layout).
- Address-space description: **VAD tree** (Windows-style virtual address descriptors)
  rather than a flat VMA list — region objects carry protection, type, and backing.
- Pool accounting: nonpaged pool, paged pool, and per-process commit charge with
  quota objects.
- Tests: T1 buddy allocator + VAD tree property tests (split/coalesce invariants,
  fragmentation bounds, overlap rejection); T2 stress allocation under interrupt load.

M3 is **done**. Its 23 host assertions (MM-001–MM-023) are `tested-T1`, and all eleven
boot assertions (MM-024–MM-034) are `tested-T2` in `docs/testing/assertions/mm.md`; there are
no M3 `pending-T2` rows left to promote. Full CI run [38162486568](https://github.com/NeXuS4Dev/NOLX/actions/runs/38162486568)
on revision `606b3c7` passed all six jobs, including QEMU boot smoke, reasserting the M3 boot markers.

### M3 progress checkpoint — 2026-10-10

Landed:

* `crates/nolx-mm-core` -- the buddy allocator, per-CPU frame magazines with a zeroed-frame
  pool, the four-level paging arithmetic and page-table walker, the VAD tree, the commit and
  pool accounting, and the boundary-tagged kernel heap. `no_std`, `forbid(unsafe_code)`, and
  therefore the same code under `cargo test` on the host as in the kernel, reached through
  the `BlockStore` and `TableStore` traits.
* `kernel/src/mm` -- the frame map gains the reservations that make it honest (low memory,
  the kernel image and BSS, the static heap arena, and frames outside the boot identity map),
  the buddy allocator is built from runs of free frames, and the boot processor's magazine
  sits in front of it. `kernel/src/mm/store.rs` is the `BlockStore` over the direct map: the
  free-list links are written into the free frames themselves.
* `kernel/src/mm/selftest.rs` -- the boot-time stress test: single frames in both
  directions, then a deterministic mixed-order walk, audited every few thousand operations,
  running until the timer has ticked so the allocations demonstrably happen under interrupt
  load. It ends by requiring the free-frame count to return to where it started.

The four allocator assertions that were `pending-T2` are now `tested-T2`, recorded against
workflow run 38088471560 on revision `a025a42` -- the same bar M2 was held to: the claim, the
marker that carries it, and the run that shows it happening.

Landed since the checkpoint above (2026-10-11):

MM-030 is `tested-T2`: the switch marker is asserted by the `boot-smoke` job, and run
38114869085 on revision `b9686fb` shows the machine reporting it -- "image window at
0xffffffff80000000, dropped 1 identity-map PML4 entry/entries; identity map gone=true, direct
map reachable=true, image window reachable=true" -- with the frame map (131040 frames, 8935
taken back for the kernel), the buddy allocator (122008 frames), the 100-tick marker, and the
mm self-test all still passing afterwards. The same run re-stated HAL-008, whose #GP test had
been loading an LDT selector and so had been relying on the identity map still being there.

* the higher-half switch -- the image is linked in the top 2 GiB at
  `0xffff_ffff_8000_0000` and mapped there by the trampoline in PML4 slot 511, the direct map
  carries the low 2 GiB of RAM and the APIC/HPET windows at `0xffff_8000_0000_0000`, and the
  kernel drops the boot identity map once its own GDT, IDT, and stack are live. The switch
  verifies itself: it probes a direct-map frame, an image-window address, and a low physical
  address that must no longer translate, and the boot stops if any probe answers wrong. The
  kernel is built with `-C code-model=kernel`, which is what makes the top-2-GiB address
  reachable; `nolx_mm_core::paging` pins that with a compile-time assertion, so a layout that
  the code model cannot address fails to build instead of failing to link.
* `kernel/src/mm/tables.rs` -- `KernelTableStore`, the `TableStore` over frames from the
  frame allocator, plus `activate`, `invalidate_page`, and the walker probes. This is the
  mechanism per-process address spaces will be built from.
* the image-window layout as a flat-copy contract: `scripts/verify-multiboot.py` now checks
  that every stored section sits at `file_offset = copy_base + (load - load_base)` and that
  the window covers the whole kernel region, because a loader that copies the image as one
  block (QEMU's Multiboot loader does) will otherwise load the wrong bytes into the boot
  layer's page tables. The boot layer's zero-initialised state is stored in the file rather
  than as a NOBITS hole for the same reason.
* `kernel/src/mm/phys.rs` (the two windows and the physical-to-pointer conversion) and
  `kernel/src/mm/pool.rs` (the charged pools, not yet wired to the object manager).

### M3 implementation checkpoint — 2026-10-11

Landed since the checkpoint above, in two steps:

* **the kernel heap** (MM-031). `kernel/src/mm/heap.rs` registers `#[global_allocator]
  KernelHeap`: up to sixteen regions of 256 KiB to 4 MiB, each a run of frames from the buddy
  allocator reached through the direct map and managed by the boundary-tagged allocator in
  `nolx-mm-core::heap`. A request no region can serve adds a region; every region is charged
  to the nonpaged pool when it is taken, and a request whose alignment is above the heap's own
  16 bytes is served by over-allocating and recording the region and offset in the words in
  front of the payload. Critical sections run under `sync::IrqLock`, a spin lock that masks
  interrupts while it is held, because a timer tick that allocated while the heap lock was
  held would deadlock a single processor. The self test drives the heap through the language
  allocator -- a vector that reallocates, a `BTreeMap` across the size-class bins, and a
  512 KiB request that forces a second region -- and then checks in-use bytes are back to
  zero and the metadata is intact.
* **address spaces and the fault path** (MM-028, MM-029, MM-032). `kernel/src/mm/space` holds
  an `AddressSpace`: a PML4 taken from the frame allocator, the kernel half of the address
  space shared with the running root (the direct map, the image window, and the recursive slot
  name the same tables in every space), a VAD tree for the user half, and a commit charge.
  `resolve` implements the four fault causes from `docs/architecture/03-memory-manager.md` --
  demand zero of a committed private page, copy on write of a page two spaces share, a
  refusal for mapped-file ranges until the Cache Manager exists, and stack growth below a
  grow-down region that coalesces back into the stack's own descriptor -- and refuses an
  access in no VAD, a write to a page `mprotect` made read-only, and anything in the kernel
  half. `hal::interrupts` routes vector 14 to it before the fatal report, so a resolvable
  fault returns to the faulting instruction. Teardown is what makes the accounting provable:
  the boot self test creates two address spaces, drives all four causes, and then requires the
  free-frame count and the live page-table count to return to exactly where they started.
  An address space is tens of kilobytes -- 256 VAD descriptors is the bulk of it -- so it is
  built in place and torn down through a borrow: the first version moved one through a local,
  an `Option`, and a `teardown(self)`, which reserved 108 KiB of the 64 KiB kernel stack and
  wrote past the end of it into `.bss`. CI caught it as a `#GP` whose QEMU exception log
  showed the stack pointer 54 KiB deep; the self test now runs at 14,768 bytes.
* **pool accounting and the node list** (MM-033, MM-034). `kernel/src/obj/pool.rs` is the
  executive's view of the three pools -- it reports what the heap and the page tables charged
  and runs the pool accounting's own check while printing. `crates/nolx-mm-core/src/numa.rs`
  is the node list and the zonelist rule (local node first, then by ascending distance, ties
  by identifier), and `kernel/src/mm/numa.rs` builds the machine's one node over the frames
  the frame map describes; `mm::allocate_frames` now walks that zonelist before it allocates,
  so the per-node allocators that SRAT parsing will add have one place to plug into.

The six rows (MM-028, MM-029, MM-031, MM-032, MM-033, MM-034) are asserted by
`.github/workflows/ci.yml` boot-smoke markers and regexes, and they are `tested-T2` as of
workflow run [38125065287](https://github.com/NeXuS4Dev/NOLX/actions/runs/38125065287) on
revision `2e7279b`, which passed every job, exactly as the four allocator assertions were
promoted.

## M4 — Object Manager and handles

The centre of the design. Everything is an object; everything reachable is reachable
through a handle.

- Object headers: type, flags, pointer/handle counts, and body size. Boot-stage security descriptors
  are stored in a parallel table, names are namespace bindings, and per-body process quota ownership
  remains deferred until object-body allocation is wired.
- Object types with create/open/parse/close/delete callbacks.
- Handle tables: fixed-capacity, generation-checked table primitive with access rights,
  inheritance flags, and `DuplicateHandle` semantics; M5 attaches one table to each process.
- Object namespace: root directory, `\Device`, `\FileSystem`, `\BaseNamedObjects`,
  `\NOLX`, symlinks, per-session directories, and boundary-protected private namespaces.
- Security Reference Monitor: SID/ACE/ACL evaluation, tokens (user, group, privilege,
  integrity), `SeAccessCheck`, mandatory-label support, auditing.
- Tests: T1 refcount lifecycle (no leak/no double free under fuzzed open/close),
  handle-table allocation, ACL evaluation vectors.

### M4 implementation checkpoints 1–2 — handle tables and namespace (verified in CI)

M4's first five checkpoints delivered the object header/type plumbing, fixed-capacity handle-table
primitive, base namespace, Security Reference Monitor, T1 host tests, and T2 boot selftests. The
callback and session/private-namespace completion is tracked in checkpoint 6 below. Delivered by
PR #6 (merged 2026-10-11).

* **Checkpoint 1, `crates/nolx-obj-core/src/handle.rs`** -- the fixed-capacity handle-table
  primitive used by processes. Handle values carry a 14-bit slot and a 16-bit generation, so a
  handle that outlived its entry is rejected instead of naming whatever now occupies the slot.
  `DuplicateHandle` may narrow access but never widen it; inheritance copies only inheritable
  entries and keeps the parent's handle values where the child's slot is free; the table is
  fixed-capacity so handle quotas are a number.
* **Checkpoint 2, `crates/nolx-obj-core/src/namespace.rs`** -- the object namespace rooted at `\`:
  directories, object bindings, and symbolic links, with case-sensitive names, strict path
  validation, link following capped at `MAX_SYMLINK_DEPTH` (cycles fail with `SymlinkLoop`), and
  removal that refuses non-empty directories and removes a link rather than its target.
* T1 host tests for every rule above run in CI's `Host tests (cargo test)` job, which passed on
  the checkpoint 2 head commit. The fmt and clippy job also passes.

### M4 implementation checkpoint 3 — the object manager boots (CI run 38133425376)

`kernel/src/obj/boot.rs` gives the executive real objects: a fixed pool of object headers, the
root namespace (`\Device`, `\FileSystem`, `\BaseNamedObjects`, `\NOLX`), and handle tables that
hold object ids. The boot self test drives one object through every reference transition, checks
each count, and requires the object to be freed exactly when its last reference goes. Its marker,
`NOLX: obj selftest: 1 object, 2 handles ...`, is asserted by the boot smoke test (OBJ-006).

To make this boot, the kernel stack went from 64 KiB to 128 KiB (`kernel/linker.ld`). The first
attempt faulted in the address-space self test with the stack pointer deep in `.bss`: the stack sits
directly below `.bss`, and the self test's frames grew with the extra code. Doubling the stack
restored the boot. The headroom is not measured yet; a stack high-water mark or guard page is the
next step to stop guessing.

### M4 implementation checkpoints 4–5 — Security Reference Monitor and T1 (verified in CI)

* **Checkpoint 4, `crates/nolx-obj-core/src/security.rs`** -- SIDs, ACEs, ACLs, tokens (user,
  groups, privileges, integrity level), and `access_check` (the `SeAccessCheck` equivalent).
  Mandatory labels refuse write-class rights to a lower-integrity token. Deny entries are checked
  in order, and a privilege never overrides a deny entry (stricter than NT, by design). Every
  decision is recorded in a bounded audit log that keeps exact totals past capacity. At boot,
  `kernel/src/obj/boot.rs` runs ten access cases against a device and a labelled log; the boot
  marker `NOLX: srm selftest: 10 access checks (6 granted, 4 denied)` is asserted by the boot
  smoke test (OBJ-010, `tested-T2`).
* **Checkpoint 5, T1** -- the handle table has a seeded fuzz test (`random_open_close_matches_a_shadow_model`)
  that runs 20,000 random open, close, and lookup steps against a shadow model. It checks that
  every closed handle stays dead after its slot is reused. OBJ-007 to OBJ-009 cover ACL
  evaluation, integrity labels and privileges, and audit totals, all `tested-T1`.
* **Refcount model, T1** -- `crates/nolx-obj-core/src/refcount.rs` states the pointer and handle
  count rules without atomics. A seeded fuzz test runs 30,000 random open, close, unbind, and
  create steps and checks the counts against an oracle after each step. It checks that no object
  is deleted twice, that none is deleted while a handle or binding still names it, and that
  `handles <= pointers` holds throughout. The kernel's atomic counts in `kernel/src/obj/mod.rs`
  follow the same rules, which the boot self test checks.
* **M5 checkpoint 1 (CI-verified):** the first process context now owns its handle table,
  token, and address-space association; threads and dispatch remain out of scope here.
* **Refcount implementation note:** `crates/nolx-obj-core/src/refcount.rs` is a non-atomic
  lifecycle model used by the host fuzz test. The kernel keeps atomic counters in
  `kernel/src/obj/mod.rs` and checks the same transitions in its boot selftest. Factoring the
  shared rules into the kernel implementation remains a possible follow-up.

### M4 implementation checkpoint 6 — lifecycle callbacks and namespace isolation (verified in CI)

`kernel/src/obj/mod.rs` now gives object types create/open/parse/close/delete callbacks, and
`kernel/src/obj/boot.rs` dispatches each hook at the appropriate object lifecycle transition.
The Device selftest covers successful callback dispatch and create/open rollback. Session paths
are initialized below `\Sessions\<id>`; private namespaces have independent fixed-capacity
roots, boundary-descriptor checks on read/write/delete, audited decisions, stale-generation
rejection, and nonempty-destroy refusal. The boot test uses equal names in two sessions and two
private roots to prove the state stays isolated. The three new markers are asserted by the boot
smoke test, and workflow run
[38249363821](https://github.com/NeXuS4Dev/NOLX/actions/runs/38249363821) on revision `15aee68`
passed every job with them, so the checkpoint is verified in CI rather than merely implemented.

The current header ABI and its out-of-line fields are documented in
`docs/architecture/01-kernel-model.md`: security descriptors are held in the boot manager's
parallel table and names are namespace bindings. The quota-owner pointer/body allocation remains
future work rather than an implemented header field.

### M4 verification checkpoint — 2026-10-12

M4 is **done**: the code exists, the tests listed under it pass in CI, and its assertions are
recorded in `docs/testing/assertions/obj.md`. All fourteen rows are now backed by a passing
record -- nine `tested-T1` host rows and five `tested-T2` boot rows (OBJ-006, OBJ-010, OBJ-011,
OBJ-012, OBJ-013); no `pending-T2` row is left in the area.

* **What CI proves.** Run [38249363821](https://github.com/NeXuS4Dev/NOLX/actions/runs/38249363821)
  ran on revision `15aee68`, the tree that carries checkpoint 6, and every one of the six jobs
  passed: boot layer, NOLXFS layout, host tests, fmt + clippy + unsafe audit, the
  `x86_64-unknown-none` kernel build, and the QEMU boot smoke test. The host-test log names the
  M4 cases individually -- `handle::fuzz_tests::random_open_close_matches_a_shadow_model` (20,000
  steps against a shadow model),
  `refcount::fuzz_tests::random_open_close_unbind_keeps_counts_exact_and_frees_once` (30,000 steps
  against an oracle), the `namespace::tests` suite including the three private-namespace boundary
  cases, and the `security::tests` ACL, integrity, privilege, and audit-wrap vectors -- with no
  failure in any target. The boot smoke test greps for each M4 marker -- `NOLX: objmgr online`,
  `obj: namespace: \Device ...`, `NOLX: obj selftest: 1 object, 2 handles`,
  `NOLX: obj session selftest: sessions 0 and 1 are isolated`,
  `NOLX: obj private namespace selftest: boundaries enforced, isolated roots`,
  `NOLX: obj type callback selftest: create/open/parse/close/delete invoked`, and
  `NOLX: srm selftest: 10 access checks (6 granted, 4 denied)` -- and the same job fails on any
  `PANIC`, `BUGCHECK`, or `CPU exception` line. The self test prints its markers only after every
  check returns `Ok`, and `kernel/src/main.rs` panics on any `Err`, so a marker in the log is the
  checks having passed, not a banner printed regardless of them. The pull request that records this
  promotion re-runs the same six jobs on the tree carrying it, and its log is posted there.
* **What the serial log shows.** `report-serial` posts the log to the pull request, so the detail
  behind the markers is readable rather than trusted:
  `refcount (1,0) -> (2,1) -> (3,2) -> (2,1) -> (1,1) -> freed, live objects 0 -> 0; audit ok`,
  `private namespace selftest: boundaries enforced, isolated roots, stale ids rejected, audit
  delta 15 (1 refused)`, and `srm selftest: 10 access checks (6 granted, 4 denied), 1 backup
  bypass, 1 integrity refusal, 1 deny entry`. The two audit pairs are the manager's own counters
  read back at boot: part two requires the log's delta to equal `cases + 2` decisions and exactly
  the four refusing cases, and the private-namespace phase requires its own delta to be 15 decisions
  with 1 refusal. The printed numbers are therefore what the audit log measured, and a boot that
  disagreed would print nothing at all and panic instead.
* **What was re-run in the sandbox.** `make bootcheck` (boot trampoline assembled, linked against
  the real `kernel/linker.ld`, Multiboot header and 256 stubs validated), `make fscheck` (layout
  generator idempotent, 31 layout tests, conformance database 141 assertions / 0 problems),
  `make usercheck`, and `scripts/check-unsafe.sh` (219 unsafe blocks, 0 without a `SAFETY:`
  comment). This sandbox has no `rustc` and no QEMU, and its network reaches neither the Rust
  distribution nor a package mirror, so `cargo test`, clippy, the kernel build, and the QEMU boot
  are not re-executed here -- they are read off the CI run above, which is the record this project
  promotes on.
* **Two under-claims found and fixed while verifying.** OBJ-014 named three host tests that exist
  and run in CI but was still recorded as `implemented`, so it is now `tested-T1`; and the five M4
  boot rows still read `pending-T2` although `docs/architecture` and this roadmap already described
  checkpoints 3 to 6 as CI-verified, which is what the promotion above settles. `01-kernel-model.md`
  also still described the `unsafe` budget as `hal/`, `mm/paging.rs`, and `obj/raw.rs`, and neither
  of those two files exists; the row now names the trees that actually hold unsafe blocks and the
  rule CI enforces.
* **Not claimed.** Object-body allocation and per-body quota ownership stay deferred (see the
  checkpoint 6 text and `01-kernel-model.md` §3), and the callback assertion is "each Device hook
  ran" rather than an exact invocation count; pinning the counts to exact values is the natural
  next step once a second object type registers callbacks.

## M5 — Executive core: processes, threads, dispatcher

- Process object: address space, handle table, token, job membership, PEB-equivalent.
- Thread object: context, kernel/user stacks, TEB-equivalent, APC queues.
- Dispatcher: ready queues per priority band, 32 priority levels with real-time and
  dynamic bands, quantum and priority-boost rules, per-processor run queues, idle thread.
- Dispatcher objects: event, mutex (with owner + recursion), semaphore, timer, thread
  join — all waitable through one `WaitForMultipleObjects`-shaped primitive.
- IPL model + **DPC** (deferred procedure calls) + system worker threads + queued locks
  and pushlocks.
- Scheduler: SMP with per-CPU affinity, load balancing, group scheduling for job objects.
- Tests: T1 scheduler simulation (priority inversion bounded by boost, FIFO within band,
  no starvation); T2 round-robin of N threads prints interleaved output.

### M5 implementation checkpoint 1 — process-owned context (CI passed: [run 38153497878](https://github.com/NeXuS4Dev/NOLX/actions/runs/38153497878))

* `crates/nolx-exec-core` models process identity and parentage, owns a handle table and token,
  and owns the supplied address-space association. Its host tests cover independent tables and
  process-local interpretation of handle values.
* `kernel/src/exec/process.rs` binds that context to the registered `Process` object header and
  the kernel's stable address-space storage. Object opens use the process token; termination
  closes every process-owned handle, tears down the address space, and leaves a zombie with its
  exit status for the later parent/reaper implementation.
* The boot selftest builds two real process contexts and address spaces, proves isolated
  handles and token-checked opens, then requires object and address-space resources to return.

### M5 implementation checkpoint 2 — host Executive and kernel thread-switch path

`crates/nolx-exec-core` now contains the bounded host-testable Executive core: process and
thread metadata; 32-level per-processor priority queues; FIFO, round-robin, dynamic aging,
affinity, load balancing, mutex donation, and virtual-runtime job-group shares; events, recursive
mutexes, semaphores, timers, thread-join objects, and common WaitAny/WaitAll semantics; an
integrated scheduler/dispatcher coordinator that blocks and wakes threads while retaining wait
results; DPC and passive system-work queues; and queued ticket/push locks. The host tests cover
the T1 priority-inversion, FIFO, starvation, dispatcher, group-share, and resource-lifecycle
contracts.

The kernel binds the shared priority scheduler to timer-interrupt frames and stable synthetic
kernel stacks. Its process self-test now exercises process/job/PEB-equivalent state, a kernel
thread context and exit, isolated/inherited handles, token-checked object opens, and resource
teardown. The configured QEMU T2 test starts three preemptible BSP kernel threads and requires
the serial trace `ABCABCABCABC` plus the exact completion marker. AP startup and actual SMP
interrupt/context switching remain outside this BSP proof; SMP queue, affinity, and group-share
rules are host-tested.

The two M5 boot assertions, EXE-009 and EXE-010, are recorded as `tested-T2` in
`docs/testing/assertions/exec.md`. Workflow [38162270144](https://github.com/NeXuS4Dev/NOLX/actions/runs/38162270144)
on revision `650b71c` passed all six CI jobs, including host tests, strict Clippy, the kernel
build, and QEMU boot smoke. The QEMU serial log contains the process self-test marker and the exact trace
`NOLX: round-robin T2: ABCABCABCABC complete: 3 kernel threads, 12 timer quanta, order verified=true`.
The conformance dashboard is regenerated from those assertion records.

**M5 is done:** its T1 scheduler/dispatcher contracts pass in CI; the T2 process/thread and
interleaved-output markers pass in QEMU; and every M5 assertion has a recorded test result.

## M6 — User mode, ELF, and the syscall ABI

- Ring-3 transition: `syscall`/`sysret` fast path, `TSS.RSP0`, IST stacks for `#DF`/`#MC`/`#NMI`.
- ELF64 loader: `PT_LOAD`, `PT_INTERP`, `PT_TLS`, `PT_GNU_STACK`, ASLR, auxv.
- `nolx-libc`: C17 + POSIX headers generated from the syscall table, `crt0`, dynamic
  loader, pthreads on native threads, locale, `dlopen`.
- Process creation model: `nxposix_spawn` primitive that the POSIX subsystem builds
  `fork`/`vfork`/`posix_spawn`/`exec*` on top of, including a true COW `fork`.
- Tests: T2 "hello world" ELF runs and exits with a status the parent observes;
  T1 syscall table uniqueness/ABI round-trip.

### M6 implementation checkpoint — 2026-10-11

**M6 is done on its milestone contract:** the listed tests pass in CI -- T2 "hello world"
runs at ring 3 and exits with a status the parent observes, and T1 syscall table
uniqueness/ABI round-trip is green in the host workspace -- and every M6 assertion is
recorded in `docs/testing/assertions/` (`HAL-024..025`, `ELF-002..003`, `EXE-011..012`).
The same boot run carries the M5 round-robin trace and the user-mode phase.

What the milestone body lists beyond those tests landed in slices:

- **Ring-3 transition:** `syscall`/`sysret` fast path (`kernel/src/hal/syscall.S`), shared
  conditional-`swapgs` restore path (`interrupts.S`), `TSS.RSP0` and a per-CPU scratch slot
  updated per thread, `SFMASK` masking IF/TF/DF to close the pre-`swapgs` window. The GDT
  orders the user pair data-below-code (0x18/0x23 selectors) for `sysret`, validated by
  `scripts/verify-multiboot.py`.
- **ELF64 loader:** `PT_LOAD` (R/W/X mapped to VAD protections, W^X refused), `PT_INTERP`,
  `PT_TLS`, `PT_GNU_STACK` metadata; `ET_DYN` ASLR bias and `R_X86_64_RELATIVE`
  relocations; auxv (`AT_PHDR`..`AT_EXECFN`, `AT_RANDOM`) and the SysV initial stack.
  Page fills execute through the physical direct map, so `posix_spawn` can build a child's
  space while the parent's space is the running one.
- **Process creation:** `nxposix_spawn` (embedded-image registry, user-space argv pointer
  table), `waitid(P_PID, WEXITED)` with a `siginfo` written at wake, `write`/`exit`/
  `getpid`. The T2 evidence chain is kernel-observed: `hello` writes and exits 42, `init`
  observes exactly 42 through `waitid` and exits 42.

Checkpoint notes, honestly: `PT_INTERP` is parsed and recorded but not launched (there is
no dynamic loader to start), and `nolx-libc` -- C17+POSIX headers, `crt0`, the dynloader,
pthreads, locale, `dlopen` -- remains open work: the T2 programs are hand-assembled
byte-deterministic images (`tools/user-elf/gen.py`) exercising the same ABI libc will.
`fork`/`vfork`/`exec*` on top of `nxposix_spawn` (including true COW) is the process-model
work of a later milestone slice. A full libc is the largest remaining M6 body item, in the
same spirit as M5's SMP caveat.

## M7 — I/O Manager, VFS, and the first filesystems

- **IORB** (I/O Request Block — our IRP): stack locations, major/minor function codes,
  completion ports, cancel routines, synchronous and asynchronous paths, `IoStatus`.
- Driver objects with dispatch tables; device objects forming stacks; attach/detach;
  driver unload with reference tracking.
- Cache Manager: shared cache maps, lazy writer, read-ahead, `MmFlushSection`.
- VFS layer: vnode-equivalent *file objects*, namei/pathname resolution (symlink loop
  limit, `O_NOFOLLOW`, `..` at mount points), mount table with propagation classes,
  descriptor table with `fcntl` duplication semantics, `umask`, `chroot`/pivot-root
  equivalent.
- Filesystems: `devfs`, `tmpfs`, `initrd` (cpio/tar) reader, then **NOLXFS**.
- Tests: T1 pathname-resolution state machine against a scripted filesystem tree
  (every POSIX `pathconf`/`ENOENT`/`ELOOP` case); T2 read/write a file in tmpfs.

### M7 implementation checkpoint — 2026-10-12

**M7 is done:** its code, host tests (T1), QEMU boot self-tests and ring-3 `tmpfs` round-trip
(T2), and specification assertions (`docs/testing/assertions/vfs.md` and
`docs/testing/assertions/io.md`) are in place and verified in CI (workflow
[38240528081](https://github.com/NeXuS4Dev/NOLX/actions/runs/38240528081) on revision
`6dfedca` passed all six jobs):

- **I/O Manager and device stacks (`crates/nolx-vfs/src/io.rs`, `kernel/src/io/mod.rs`):**
  fixed-size `Iorb` with 16 `MajorFunction` codes, 8-deep `IoStackLocation` arrays,
  top-down filter/function device dispatch, bottom-up completion routines with
  `ContinueCompletion` / `ShortCircuit`, `IoCompletionPort` queuing for asynchronous IORBs,
  atomic cancellation routines (`ECANCELED`), and reference-tracked `DriverObject` unload
  (`EBUSY` while devices or in-flight IORBs remain).
- **Cache Manager (`crates/nolx-vfs/src/cache.rs`):** `SharedCacheMap` with 4 KiB
  page-granular caching (`copy_read`, `copy_write`), sequential detector and read-ahead
  prefetching, bounded background `lazy_writer_tick`, and `mm_flush_section` /
  `mm_purge_section` coherency primitives.
- **VFS layer (`crates/nolx-vfs/src/{path,mount,fd,vfs}.rs`):** `FileObject` open-file
  descriptions, `namei` pathname resolution with `SYMLOOP_MAX`, `O_NOFOLLOW`, `..` across
  mount points, `chroot` confinement, `pathconf` / `fpathconf` limits, mount propagation
  classes (`Private`, `Shared` peer groups, `Slave` one-way propagation, `Unbindable`) and
  `pivot_root`, `DescriptorTable` with `dup`, `dup2`, and `fcntl` (`F_DUPFD`,
  `F_DUPFD_CLOEXEC`, `F_GETFD`, `F_SETFD`, `F_GETFL`, `F_SETFL`), and `umask` mode masking.
- **Filesystems (`crates/nolx-vfs/src/{devfs,tmpfs,initrd,nolxfs_vfs}.rs`):**
  `devfs` (`/dev/null`, `/dev/zero`, `/dev/full`, `/dev/console`, and custom `mknod`
  nodes), bounded `tmpfs` (regular files, directories, symlinks, hard links, rename with
  descendant cycle detection, unlink-while-open retention until last descriptor close, and
  inode/byte quotas), read-only `initrd` (SVR4 `newc` cpio and POSIX `ustar` tar archives),
  and the `NOLXFS` VFS driver backed by `crates/nolxfs` (`mkfs::probe`, B-tree
  `INODE`/`DIR_REC`/`FILE_EXTENT` records, and Fletcher-64 verification).
- **Ring-3 `tmpfs` T2 and POSIX syscalls (`kernel/src/psx/mod.rs`, `kernel/src/exec/user/`):**
  `SYS_OPEN`, `SYS_CLOSE`, `SYS_READ`, `SYS_WRITE`, `SYS_LSEEK`, and `SYS_UNLINK` wired into
  the ring-3 syscall table; `init` opens `/tmp/m7-check` (`O_CREAT | O_EXCL | O_RDWR`),
  writes 28 bytes, `lseek`s to 0, reads back 28 bytes, closes and unlinks `/tmp/m7-check`,
  and prints the round-tripped payload to stdout before spawning `hello`.

## M8 — NOLXFS

APFS-shaped design, our own implementation: container → checkpoint ring → live superblock
→ object map → volume → B-tree of typed records, with copy-on-write, snapshots, and a
space manager. Full spec in `docs/fs/nolxfs-format.md`.

- On-disk objects with 32-byte headers and Fletcher-64 checksums; omap (oid,xid)→paddr.
- B-tree: node split/merge, TOC layout, keyed by `(parent inode, name hash, name)`.
- Record types: inode, dentry, xattr, sibling-link, data stream, physical extent, fext.
- Copy-on-write with transaction IDs; checkpointing; crash recovery by replaying the
  checkpoint ring to the newest valid superblock.
- Space manager: chunk-info blocks + allocation bitmaps; reaper for deferred frees.
- Snapshots: per-volume snapshot metadata tree, mount-at-snapshot, delete/rename.
- Atomic write batches (metadata journal) so a power cut never tears a transaction.
- `mkfs.nolxfs`, `fsck.nolxfs`, `nolxfs-dump`, and a FUSE-less host mounter for testing.
- Tests: T1 mkfs→mount→create→write→fsync→remount round trip on an in-memory device,
  crash injection at every write boundary with recovery asserted; fsck finds and repairs
  injected corruption; snapshot isolation property tests.

**Status: implemented.** `crates/nolxfs` now carries the full stack —
`spaceman.rs` (chunk-info blocks, allocation bitmaps, snapshot-horizon reaper),
`checkpoint.rs` (checkpoint-ring commits with the descriptor block written last as
the atomic commit point; recovery selects the newest valid checkpoint, tolerating a
torn newest descriptor), `volume.rs` (the mounted filesystem: COW fs-tree,
per-block extent reference counts, xattrs, hard links via sibling records,
clone_file, snapshots with read-only mount-at-snapshot, and the fast-extent tree),
`fsck.rs` and `dump.rs`. The host tools `fsck.nolxfs` and `nolxfs-dump`
(check/repair; dump/ls/cat/stat as a FUSE-less mounter) join `mkfs.nolxfs`, and the
VFS dispatches rmdir/link/symlink/readlink/unlink/rename/truncate to NOLXFS instead
of returning ENOTSUP; the kernel boot self-test exercises those operations on
`/mnt/nolxfs` (`nolxfs-m8 ok`). Assertions: `docs/testing/assertions/nxfs.md`
(NXFS-001…015). Deliberate deviations from the letter of the spec, recorded in
`docs/fs/nolxfs-format.md`: dentry keys are `(parent, name)` without a separate
name-hash field, and the extent-reference count is stored per physical block
appended to the 24-byte `phys_ext_val`. The passing CI run for this milestone will
CI run [38359528564](https://github.com/NeXuS4Dev/NOLX/actions/runs/38359528564) on revision `be7730a` passed all six jobs
(boot-layer, on-disk-format, host-tests, lint, kernel, boot-smoke with the
`nolxfs-m8 ok` marker asserted).

## M9 — Devices and drivers

**Status: started.** Slices landed on branch `arena/d71a2d5c-nolx`:

- PCI (`crates/nolx-pci-core`): configuration-space logic (`0xCF8` addressing, function identity,
  BAR decoding and sizing with restore-after-probe, capability walking with a loop guard,
  bridge-aware enumeration), bound to ports `0xCF8`/`0xCFC` and reported at boot. BAR sizing runs
  for type-0 functions. PCI-001 to PCI-008 are `tested-T1`; PCI-009 and PCI-010 are `tested-T2`
  (CI runs [38370090989](https://github.com/NeXuS4Dev/NOLX/actions/runs/38370090989) and
  [38371460597](https://github.com/NeXuS4Dev/NOLX/actions/runs/38371460597)).
- virtio (`crates/nolx-virtio-core`, in progress): host-tested transport rules (capability
  discovery, notify address, feature negotiation, status sequence, split virtqueue layout,
  block request format). VIO-001 to VIO-009 are `tested-T1` in `docs/testing/assertions/drv.md`.
- MMIO windows (`crates/nolx-mm-core/src/mmio.rs`, `kernel/src/mm/mmio.rs`): device registers
  outside the direct map are mapped uncached in PML4 slot 257. MMIO-001 to MMIO-004 are
  `tested-T1`; MMIO-005 is `tested-T2` (the boot maps the local APIC through the window and
  checks it against the direct map).
- virtio-blk (`kernel/src/hal/virtio_blk.rs`): the probe finds the device, negotiates features,
  sets up queue 0, and reads sector 0 through the ring, polled. VIO-010 is `tested-T1`; VIO-011
  and VIO-012 are `tested-T2`, with CI attaching a 1 MiB disk whose sector 0 starts with a
  known pattern (run [38404754464](https://github.com/NeXuS4Dev/NOLX/actions/runs/38404754464)).
- virtio-blk writes: a 2 KiB pattern is written to sectors 1600 to 1603, read back, and checked by
  CRC-32; CI checksums the same sectors in the disk image after QEMU exits. VIO-013 is
  `tested-T1`. VIO-014 and VIO-015 are `tested-T2` (run [38456685552](https://github.com/NeXuS4Dev/NOLX/actions/runs/38456685552)). They were first
  `tested-T2` at run [38442239670](https://github.com/NeXuS4Dev/NOLX/actions/runs/38442239670),
  when the pattern was at sectors 8 to 11.
- GPT rules (`crates/nolx-block-core`): header signature, revision, header and table CRC-32,
  the own-LBA and reserved-field checks, entry size, usable area, partition range, and overlap.
  BLK-001 to BLK-021 are `tested-T1`.
- GPT on the virtio disk: CI attaches an image from `tools/make-test-disk.py` with two partitions.
  The probe reads the header and table through the queue, checks them, and prints the partitions.
  BLK-022 and BLK-023 are `tested-T2` (run [38456685552](https://github.com/NeXuS4Dev/NOLX/actions/runs/38456685552)).
- NOLXFS on the first partition: the kernel formats it when it holds no volume, writes an 8 KiB file,
  commits, mounts the volume again from the disk, and reads the file back. CI compares the checksum
  with the pattern and runs `fsck.nolxfs` on the partition. BLK-024 and BLK-025 are `tested-T2`
  (run [38456685552](https://github.com/NeXuS4Dev/NOLX/actions/runs/38456685552)).
- AHCI (`crates/nolx-ahci-core`, `kernel/src/hal/ahci.rs`): register layout, port state, FIS and
  command-header encodings, physical region descriptors, and the WRITE DMA EXT encoding, host-tested
  (AHCI-001 to AHCI-007 and AHCI-010 are `tested-T1`). The probe finds the controller through BAR 5,
  starts the first SATA port, reads sector 0 through command slot 0, then writes a 2 KiB pattern to
  sectors 1600 to 1603, reads it back, and checks CRC-32. AHCI-008 and AHCI-009 are `tested-T2`
  (run [38460754722](https://github.com/NeXuS4Dev/NOLX/actions/runs/38460754722)); AHCI-011 is `tested-T2` (run [38523473276](https://github.com/NeXuS4Dev/NOLX/actions/runs/38523473276)). CI attaches
  a copy of the disk image to the AHCI port and checksums the written sectors in that copy after QEMU exits.
  Not yet done: interrupts, and the other storage paths (request merging, per-device queues).
- NVMe (`crates/nolx-nvme-core`, `kernel/src/hal/nvme.rs`): controller registers, doorbell offsets,
  the enable sequence, 64-byte submission and 16-byte completion entries, the Create I/O queue and
  Read and Write encodings, completion status, and the namespace block size, host-tested
  (NVME-001 to NVME-008 are `tested-T1`). The probe finds class 01:08:02, enables memory decoding and
  bus mastering, maps BAR 0, sets up one admin queue pair and one I/O queue pair of eight entries,
  identifies namespace 1, reads sector 0 through the I/O queue, then writes sectors 1600 to 1603,
  reads them back, and checks CRC-32. NVME-009 to NVME-011 are `tested-T2` (run
  [38558796880](https://github.com/NeXuS4Dev/NOLX/actions/runs/38558796880)). CI attaches a copy of the
  disk image as an NVMe namespace and checksums the written sectors in that copy after QEMU exits.
  Not yet done: interrupts, and the other storage paths.

- MSI and MSI-X rules (`crates/nolx-pci-core/src/msi.rs`): capability layouts, the message address
  and data for a local APIC, the MSI-X table entry and its placement in a BAR, host-tested
  (PCI-011 to PCI-013 are `tested-T1`, run [38567825680](https://github.com/NeXuS4Dev/NOLX/actions/runs/38567825680)).
  The kernel does not program MSI or MSI-X yet.

- Block request queue (`crates/nolx-block-core/src/queue.rs`): adjacent same-direction requests merge
  up to a size limit, a merge can bridge queued requests, and the queue keeps arrival order. Host-tested;
  BLK-026 and BLK-027 are `tested-T1` (run [38570531260](https://github.com/NeXuS4Dev/NOLX/actions/runs/38570531260)).
  The drivers do not submit through the queue yet.

- PS/2 keyboard rules (`crates/nolx-input-core`): controller status and configuration bits, device
  replies, and the scancode set 1 decoder with the E0 and E1 prefixes, producing USB HID usage codes.
  Host-tested; PS2-001 to PS2-007 are `tested-T1` (run [38599792741](https://github.com/NeXuS4Dev/NOLX/actions/runs/38599792741)).
  The kernel does not read the keyboard yet, and the mouse and xHCI are not started.

- Network device rules (`crates/nolx-net-core`): virtio-net identity, features, configuration space,
  queue layout, and the 12-byte net header, plus Ethernet frame limits, header fields, and MAC address
  rules. Host-tested; NET-001 to NET-006 are `tested-T1` (run [38603598789](https://github.com/NeXuS4Dev/NOLX/actions/runs/38603598789)).
  The kernel has no network driver yet.

- ACPI device and power rules (`crates/nolx-power-core`): `_STA` status bits and usability, `_CRS`
  resource templates (IRQ, I/O, fixed I/O, Memory32Fixed, QWord memory), the `_S3_`/`_S5_` sleep-type
  lookup in AML, and the PM1 sleep word. Host-tested; PWR-001 to PWR-005 are `tested-T1` (run
  [38607105889](https://github.com/NeXuS4Dev/NOLX/actions/runs/38607105889)). The kernel does not run
  `_STA` or `_CRS` yet, and does not enter a sleep state.

- USB host rules (`crates/nolx-usb-core`): xHCI register offsets and port status with the
  write-one-to-clear rules, the link, normal, and event TRBs, and the HID boot keyboard report, which
  yields the same key events as the PS/2 decoder. Host-tested; USB-001 to USB-006 are `tested-T1` (run
  [38608775432](https://github.com/NeXuS4Dev/NOLX/actions/runs/38608775432)). The kernel does not
  drive a controller yet, and hubs and other device classes are not here.

- e1000 rules (`crates/nolx-e1000-core`): register map and device identity, the EEPROM MAC read,
  receive address registers, engine control values, legacy receive and transmit descriptors, ring sizing,
  and free-slot arithmetic. Host-tested; NIC-001 to NIC-007 are `tested-T1` (run
  [38611667077](https://github.com/NeXuS4Dev/NOLX/actions/runs/38611667077)). The kernel has no e1000
  driver yet.

Not yet done: PCIe ECAM, MSI/MSI-X programming in the kernel, per-device queues, and every driver below.

- PCI/PCIe enumeration, BAR assignment, MSI/MSI-X, capability walking.
- Storage: AHCI (SATA), NVMe, virtio-blk; block layer with partitions (GPT), request
  merging, per-device queues.
- Input: PS/2, USB (xHCI) → HID → evdev-equivalent event objects.
- Networking: virtio-net, e1000, then a driver framework (`nolx-df`) so third-party
  drivers are a device object + dispatch table, not kernel patches.
- Power: ACPI table parsing, `_STA`/`_CRS`, S3/S5, device power policy.
- Tests: T2 boot with each storage backend and checksum a written file.

## M10 — POSIX conformance waves

Each wave is a numbered, testable set of System Interfaces functions.

- **W1 file & directory:** `open`/`read`/`write`/`pread`/`pwrite`/`lseek`/`close`/`dup*`,
  `stat`/`fstat`/`lstat`/`fstatat`/`statvfs`, `mkdir`/`rmdir`/`unlink`/`rename`/`link`/
  `symlink`/`readlink`/`chmod`/`chown`/`utimensat`/`truncate`/`access`/`faccessat`,
  `opendir`/`readdir`/`seekdir`/`telldir`, `fcntl`, `ioctl`, `umask`, `chdir`/`chroot`,
  `pathconf`/`sysconf`/`confstr`.
- **W2 process & credentials:** `fork`/`exec*`/`_exit`/`wait`/`waitpid`/`waitid`,
  `getpid`/`getppid`/`getuid`/`setuid`/`getgid`/`setgid`/`getgroups`/`setpgid`/`getsid`/
  `setsid`/`setuid` family with saved-set-user-ID semantics, `nice`, `getrlimit`/
  `setrlimit`, `getrusage`, `times`, `alarm`, `gettimeofday`/`clock_gettime`/
  `clock_nanosleep`/`nanosleep`, `umask`, `exit`/`atexit`, `posix_spawn`.
- **W3 signals:** full `sigaction` semantics (SA_SIGINFO, SA_RESTART, SA_NOCLDSTOP,
  SA_RESETHAND, SA_NODEFER), `sigprocmask`/`sigwait`/`sigsuspend`/`sigpending`/
  `sigqueue`/`sigtimedwait`, standard signal dispositions, `kill`/`killpg`/`raise`,
  async-signal-safety list, signal stack (`sigaltstack`), `pthread_kill`/`pthread_sigmask`.
- **W4 terminal & process groups:** termios (canonical/raw, `V*` indices, baud rates,
  `tcgetpgrp`/`tcsetpgrp`, flow control), controlling terminals, `pty`/`openpty`,
  job-control signals (`SIGTTIN`/`SIGTTOU`/`SIGTSTP`/`SIGCONT`), `ttyname`.
- **W5 IPC:** pipes, FIFOs, System V IPC (`msgget`/`msgsnd`/`msgrcv`/`semget`/`semop`/
  `semctl`/`shmget`/`shmat`/`shmctl`), POSIX IPC (`mq_*`, `sem_*`, `shm_open`/
  `shm_unlink`), `socketpair`, `select`/`pselect`/`poll`/`ppoll`/`epoll`-equivalent
  `nxevent`, `pipe2`, `dup3`.
- **W6 real-time & threads:** pthreads (mutex, cond, rwlock, barrier, spin, TLS,
  cancellation, `pthread_atfork`), `clock_*` clock set, `timer_create`/`timer_settime`/
  overrun counting, `sched_*` policies and `SCHED_FIFO`/`RR`/`OTHER`/`SPORADIC`,
  `mlock*`, `shm_*`, `aio_*`, `mmap`/`munmap`/`mprotect`/`msync`/`madvise`/`mlock`.
- **W7 network:** BSD sockets (`socket`/`bind`/`listen`/`accept`/`connect`/`send*`/
  `recv*`/`shutdown`/`getsockopt`/`setsockopt`), `getaddrinfo`/`getnameinfo`,
  `if_*`, routing socket or netlink-equivalent.
- **W8 dynamic loading & locale:** `dlopen`/`dlsym`/`dlclose`/`dlerror`, `iconv`,
  `setlocale`/`nl_langinfo`, wide chars, regex (`regcomp`/`regexec`), `glob`, `fnmatch`,
  `wordexp`, `fts`, `getopt`.
- Tests: T3 for every wave — Open POSIX Test Suite where it covers the area (it is
  POSIX.1-2001-era and stopped updating in 2013, so it is a floor, not a ceiling), plus
  our own assertion-per-spec-sentence suite for Issue 8 additions.

## M11 — Userspace and the shell

- `nxsh`: a conforming Shell & Utilities volume shell (expansion, quoting, job control,
  `set -e/-u/-o pipefail`, functions, traps, here-docs).
- `nolx-coreutils`: the utilities the Shell & Utilities volume requires, with the
  documented options and exit statuses.
- `nolx-init`, `getty`, service manager with dependency ordering and supervision.
- Package/port system with a reproducible build description.
- Tests: T3 shell passes the Shell & Utilities assertions; coreutils golden-output tests.

## M12 — GUI

- Display stack: KMS-equivalent modesetting, DRM-like buffer objects, virtio-gpu +
  native drivers, damage tracking, page-flip.
- **Compositor** in a user-mode server on IPC ports; clients share buffers via
  `nxshm`/dmabuf-equivalent; no client can scribble on another.
- Window server: window objects in the object namespace, focus/activation policy,
  input routing with grab semantics, HiDPI, accessibility hooks from day one.
- Toolkit (`nolx-ui`): retained-mode widget tree, text layout with shaping, theming.
- Reference apps: terminal, file manager, settings, text editor.
- Tests: T2 compositor boots to a desktop under QEMU with virtio-gpu; T1 compositor
  state machine (focus, stacking, damage regions).

## M13 — Robustness, security, and performance

- Verified syscall argument marshalling with explicit bounds (no TOCTOU on user pointers:
  fetch-once, then validate).
- Hardening: stack protector, CFI on the kernel, `noexec`/SMAP/SMEP/CET, KASLR,
  hardened usercopy, driver sandboxing option (user-mode drivers over ports).
- Lock-down and attestation, signed modules, secure boot chain.
- Performance targets and the benchmarks that define them (syscall round trip, page
  fault, file create/append, process spawn, context switch).
- Tests: T1/T2 fuzzing of the syscall boundary and NOLXFS image parser (crash-free
  requirement: a malformed image must never panic the kernel).

## M14 — Conformance posture

- Public conformance dashboard: per-function status generated from the assertion database.
- Run the Open POSIX Test Suite end to end in CI, publish the pass matrix.
- Document every deliberate deviation with its rationale in
  `docs/testing/deviations.md`.
- Evaluate what formal certification would require, and which gaps remain
  (certification is a legal/commercial process separate from engineering completeness).

---

## Explicitly out of scope

- Binary compatibility with Linux, macOS, or any other system's ABI. NOLX runs NOLX ELF
  binaries. Porting is a source-level exercise, which is what POSIX is for.
- Reading or referencing Linux source. NOLX is designed from the published specifications
  and from the FreeBSD and illumos codebases, plus Microsoft's published kernel-mode
  architecture guidance.

---

## Resolved: the kernel entry point did not execute

Recorded 2026-10-10, M1. Closed the same day.

`boot/boot.S` brought the machine to long mode and called `nolx_kernel_main`. Nothing came
back: no output, no panic, no fault report. The image was verified correct throughout --
`readelf` placed every section where the linker script put it, `nm` showed both entry
points, and `objdump` showed the probe containing the port write that would print -- so the
failure was in executing Rust reached from the trampoline, not in building or loading it.

The cause was one constant. `COM1_LSR` was `3`. Offset 3 from COM1's base port is the FIFO
control register, which is write-only on a 16550A; the line status register, the one with
the "transmit holding register empty" bit, is at offset 5 -- port `0x3FD`, which
`boot/boot.S` had been using correctly all along. So the kernel's first Rust function spun
forever on `while inb(0x3FB) & 0x20 == 0`, waiting for a bit a write-only register would
never report. No fault, no output, and nothing on the serial log to distinguish it from a
machine that had simply stopped.

What made it findable was refusing to guess. Three things were added to the boot layer:

| Step | What it established |
|---|---|
| Load an interrupt descriptor table before the first call into Rust | The silence was *not* a triple fault -- no exception ever fired |
| An empty Rust function, called before the printing probe | The trampoline could enter Rust and return, so the handoff was fine |
| The first sixteen bytes at the probe, printed as hex | `31 f6 eb 1c` -- `xor esi,esi` then a jump -- the transmit loop was compiled exactly as written, so the code was present and the loop was not terminating |

That leaves one possibility, and one port read to check.

The lesson is worth more than the fix: a kernel that cannot report its own faults cannot be
debugged from inside, and a boot log that is not posted where it can be read is the same as
no log. Both are now true in the other direction -- the IDT is loaded in the boot layer, in
assembly, so it works even when calling Rust is the thing that is broken, and the serial log
is posted to the pull request on every run.

The whole init sequence now runs under QEMU:

```
NOLX: nolx_probe reached (Rust is callable)
NOLX: nolx_kernel_main entered
NOLX 0.1.0 starting
multiboot: flags=0x24f mem_lower=639KiB mem_upper=523136KiB modules=0 mmap=168B@0x9000
mm: 7 regions, 511 MiB usable, 12288 MiB reserved, highest usable 0x1ffe0000
obj: registered 15 object types
exec: 32 priority levels, real-time band from 16, SCHED_FIFO priority range 0..15
psx: 229 syscall numbers assigned, 0 implemented, the rest return ENOSYS
io: I/O manager online, 16 major function codes, IORB is 656 bytes
NOLX: init sequence complete, halting the boot processor
```

Every one of those lines is now a hard gate in the boot smoke test.
