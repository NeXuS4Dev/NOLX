# M9 completion gate

M9 is **not complete**. Passing the current CI suite proves only the assertions it exercises.
This checklist tracks the remaining integration work against ROADMAP.md, without shrinking
M9 to the already implemented boot probes. Check items only with executable evidence.

## PCI and driver plumbing

- [x] Select ECAM for driver configuration reads/writes when MCFG mappings are usable;
      serialize legacy address/data port pairs and retain legacy fallback on segment zero.
- [x] Enumerate declared PCI segments/bus ranges without aliasing identical BDFs.
- [x] Exercise ECAM writes with real driver bring-up and storage round trips on Q35.
- [x] Program MSI for AHCI and acknowledge its port/controller causes before APIC EOI;
      prove per-request interrupt delivery and on-disk checksums.
- [x] Route 82540EM firmware-assigned legacy INTx (including shared routes) and
      independent virtio-net MSI-X completions; 82540EM does not support MSI.
- [ ] Discover/reroute PCI interrupts using evaluated ACPI `_PRT`/link devices rather
      than relying on the legacy PCI Interrupt Line value. Bounded PC/Q35 current-route
      discovery/programming is now verified (checkpoint below); arbitrary link reprogramming,
      unsupported root/topology/source models and dynamic vector ownership remain open.
- [x] Assign missing memory32/memory64/I/O BARs and closed type-1 memory/I/O bridge
      windows on the bounded PC/Q35 segment-zero profile, with readback and rollback.
- [x] Consume evaluated host-bridge `_CRS` producer apertures for bounded PC/Q35 missing
      BAR/window assignment; subtract complete loader maps, consumers and mapped windows.
- [ ] Generalize beyond one segment-zero root: bus-number allocation, existing-resource
      rebalancing, translated/variable windows and 64-bit prefetchable forwarding.
- [x] Bind multiple virtio-blk/AHCI/NVMe PCI controllers on segment zero, including
      secondary buses, with per-controller queues, interrupts and independent lifecycle.
- [x] Multi-adapter native e1000/virtio-net contexts with exact-handle public RX/TX.
- [ ] Native nonzero-segment binding.
- [x] Persistent system registry; retain native storage/network state past probe return;
      healthy stop/restart/removal with hardware quiescence and exact DMA reclamation.
- [x] Serialized, reusable, generation-checked MMIO windows with partial-map rollback
      and shared-root-preserving unmap on healthy stop (BSP only).
- [ ] Complete failed-start/failed-stop recovery and interrupt-resource reclamation;
      remaining segment binding and firmware routing are tracked separately above.

## Storage integration

- [x] Run file-level format/write/commit/remount/read/checksum tests through **each**
      storage backend, independently mount the file on the host after QEMU exits, and
      reboot the unchanged disk to read the previous commit before writing again.
- [x] Preserve per-device queues when generic filesystem I/O reaches AHCI/NVMe; no data
      transfer bypasses `blockdev::run`. Merging remains covered by portable queue tests,
      not a claim that synchronous one-at-a-time I/O demonstrates concurrent merging.
- [x] Issue real virtio FLUSH / ATA FLUSH CACHE EXT / NVMe Flush commands at checkpoint
      barriers. Reject damaged GPT/container metadata without formatting existing data.

## Input

- [x] PS/2 controller initialization, keyboard and mouse IRQ paths.
- [ ] xHCI command/event/transfer rings, slots, addressing and device enumeration.
      Direct-root USB2 boot-HID rings/slots/addressing and hotplug are now exercised on PC/Q35;
      two controllers now have framework stop/restart/removal and failure-quarantine checks;
      hubs, SuperSpeed, interrupt delivery and full failed-controller recovery remain open.
- [x] HID boot keyboard/mouse reports delivered to bounded kernel input event objects,
      with independent subscriptions, source/device identity, state resync and stale-handle refusal.
- [ ] Executive/process-handle, security/namespace and waitable/user-facing input bindings;
      the kernel object API below is not a new syscall or POSIX input-device interface.
- [ ] USB disconnect/hotplug cleanup and endpoint/ring ownership; do not leak controller
      resources or leave DMA active after device removal.
- [x] Inject PS/2 keyboard/mouse events under QEMU and assert delivered event contents.
      USB injection and bounded kernel-object consumers now also have PC/Q35 hardware checks;
      general USB, Executive bindings and full failed-controller recovery remain open.

## Networking

- [x] e1000 receive, descriptor recycling and interrupt acknowledgement/delivery.
- [x] virtio-net negotiated RX/TX queues and MSI-X interrupt delivery through the driver
      framework; retain DMA/ring state and expose bounded receive/send frame operations.
- [x] Verify externally supplied packets and transmitted bytes, not only synthetic host rules.
      Higher network/POSIX socket layers belong to later milestones.

## ACPI and power

- [x] Bounded AML namespace loading and pure `_STA`/`_CRS` method evaluation, including
      real PC/Q35 SeaBIOS keyboard/serial methods, with explicit refusals and resource limits.
- [x] Publish a status-gated firmware-device/resource catalog without retaining interpreter heap.
- [ ] Complete hardware-field/operation-region providers and the remaining AML operations
      needed by PCI host-bridge/link-device methods; do not mistake a partial catalog for discovery.
- [x] Replace the fixed memory allocation aperture with evaluated root `_CRS` windows on
      the bounded PC/Q35 profile; ACPI refusal never selects the no-ACPI board fallback.
- [ ] Consume evaluated link resources for PCI interrupt routing and complete general
      device binding, replacing the firmware Interrupt Line fallback.
- [ ] Complete device power-state/quiesce policy and S3 save/resume path.
      BSP device-level shutdown freeze/flush/stop and fail-closed S5 gates now have six-driver
      PC/Q35 evidence; higher-layer cache/user-task/SMP orchestration and S3 remain open.
- [x] ACPI S5 entry with QMP observing actual guest shutdown and QEMU exit zero, not a
      printed hypothetical word. The unsafe primitive requires prior system quiescence;
      its test VM has no active DMA, and a negative test refuses an active bus master.

## Final verification

- [ ] Host tests and negative/malformed-input tests for new portable contracts.
- [ ] Kernel build, formatting, Clippy, unsafe audit, image/layout/conformance checks.
- [ ] Dedicated boot tests for each new hardware path, including absent-device fallbacks.
- [ ] Separate virtio/AHCI/NVMe disk boots with persistent file checksums.
- [ ] Assertion database, README and roadmap agree on the final state and cite CI evidence.
- [ ] All checks on the final revision pass. Only then change M9 to **done**.

## Verified progress in PR #15 (2026-10-16)

Run [38953420334](https://github.com/NeXuS4Dev/NOLX/actions/runs/38953420334) on
`492d7a5` passed all nine jobs, including actual ECAM-backed virtio disk writes,
AHCI MSI delivery, e1000 externally received ARP replies over INTx, injected PS/2
keyboard/mouse events, and separate virtio/AHCI/NVMe boots with disk-image checksums.
Eleven new host tests cover segment isolation, MSI programming, ARP matching,
bounded packet/input queues and PS/2 mouse decoding. Native configuration access is
serialized, never aliases nonzero segments to legacy ports, and reports refused mappings.

Explicit limitations: nonzero segments are enumerated and reported, but the existing
native drivers still bind segment zero. e1000 now consumes evaluated ACPI current-link/direct
GSI routes on the bounded PC/Q35 profile (GSI3…23 excluding reserved ISA/NMI inputs), not the
Interrupt Line byte when ACPI is advertised. General link reprogramming remains open.
Network reachability verification currently uses QEMU's user-network gateway as a boot
self-test peer; this is not a configurable IP/socket stack. Input FIFO delivery exists,
but HID enumeration and public input-device objects remain open. At that checkpoint driver registries still
used one-shot boot callbacks; the persistent lifecycle checkpoint below supersedes that limitation.

The AHCI no-local-APIC polling fallback plus PIC/PIT disk checksum test is also verified:
run [38954085871](https://github.com/NeXuS4Dev/NOLX/actions/runs/38954085871) on `c8431f0`
passed all nine jobs; AHCI-013 is `tested-T2`. The old PCI-014…018 checkpoint alone
was read-only; the checked transport gates above are backed by the new tests, not that
older evidence. **Do not mark M9 done from this partial gate list.**

## Storage-file integration (2026-10-16)

Local Rust 1.88.0 and QEMU 11.0.2 verification now exercises the shared `SectorDevice` /
`PartitionDevice` contract and the native AHCI/NVMe filesystem transports. All three
single-backend boots wrote and remounted `checksum.bin` (8192 bytes, CRC `0x9ea14b00`).
`scripts/check-storage-files.py` independently mounted the persisted partition with
`nolxfs-dump`, compared every file byte, booted the same image again, and verified the
previous commit **before** overwriting the file. Q35 ECAM and AHCI no-APIC variants also
passed locally. Corrupted GPT, corrupted container metadata and an erased superblock
were refused without modifying the image; only entirely blank partitions authorize mkfs.
Nine new portable tests cover partition bounds/overflow, block mapping, error propagation,
cache-flush forwarding, ATA identification, and native flush request encodings.

This closes the filesystem-test gap, **not the milestone**. Persistent controller lifecycle,
multi-device binding, BAR allocation, USB HID, AML and power remain open.
CI now applies the file, host-mount, reboot and malformed-media checks to every storage
backend. Do not treat a local result as a CI result; final CI evidence is recorded separately.

CI run [38958403492](https://github.com/NeXuS4Dev/NOLX/actions/runs/38958403492) on `a9239d7` passed **all nine jobs**,
including all three file-persistence/reboot/malformed-media matrix jobs and the complete
hardware boot smoke regression. BLK-038/039 are now `tested-T2`. Both overlapping local
host test commands passed 686 tests each (zero failures); host/kernel Clippy and formatting,
235-block unsafe audit, boot/layout checks and all 21 image checks passed. Existing ECAM,
PIC/PIT/AHCI fallback and injected PS/2 scripts also passed locally. No pull request was
created for this work. M9 remains incomplete for the unchecked gates above.

## Native virtio-net integration (2026-10-16)

`kernel/src/hal/virtio_net.rs` negotiates VERSION_1 + MAC without checksum/GSO/multiqueue
offload, owns separate RX/TX split queues, takes per-queue MSI-X interrupts on 0x53/0x54,
recycles descriptors and delivers copies through a fixed-capacity frame FIFO. Queue and
buffer DMA is retained after the probe. Send waits never hold the IRQ-state lock.
`scripts/check-virtio-net.py` passed six local QEMU 11.0.2 boots: transitional and modern-only
PCI functions on pc/Q35, absent-device fallback and explicit no-APIC refusal. It validates
both exact transmitted ARP requests and both externally received replies from a host-side
pcap, not only guest-generated log lines. Two portable tests cover feature refusal,
offload rejection, truncation, oversized frames and header layout. The dedicated CI job
is configured; its result must be recorded before promoting the new network assertions.

The public frame operations are an interface-driver transport, not a POSIX socket/IP stack.
The shared persistent framework lifecycle and multi-adapter/segment binding gates remain open.

Native virtio-net CI run [38960237622](https://github.com/NeXuS4Dev/NOLX/actions/runs/38960237622) on `bebe438` passed **all ten jobs**.
The stricter network test on `1cdc218` additionally waits for the complete memory selftests
and requires the unchanged `in use 0 -> 0 bytes, integrity ok` heap assertion; all six
network boots passed locally and in that run's network job. NET-015/016 are `tested-T2`.
Both overlapping host test commands now pass 688 tests each, with zero failures.

A later `1cdc218` NVMe matrix boot failed during filesystem reads under CI load; the
filesystem boundary previously collapsed all native failures into `Io(5)`. The investigation
now preserves NVMe timeout/interrupt/status/identifier categories and changes the shared
completion-interrupt wait from a CPU-speed-only spin count to a bounded two-second HPET
budget (still with a hard iteration ceiling and clockless fallback). Per-command interrupt
assertions remain mandatory. Full CI must pass again before treating this as verified.

The NVMe review also found a publication-order hazard: copying all completion bytes before
checking phase can combine a stale command identifier with a newly published phase bit.
The shared completion reader now observes the phase byte first, fences, then snapshots the
entry before ringing the completion-head doorbell. Two portable tests enforce the read
ordering and ensure unpublished payloads are never consumed. This is separate from the
interrupt deadline fix; both retain strict identifier/status/interrupt validation.

NVMe follow-up run [38961357104](https://github.com/NeXuS4Dev/NOLX/actions/runs/38961357104)
on `6ee3f79` passed **all ten jobs** after both completion fixes. Twenty-four simultaneous
local stress boots (six QEMUs at a time) also passed full file/commit/memory/zero-heap-leak
checks. The preceding deadline-only revision's CI also passed all ten jobs, but the phase
publication ordering is retained because it fixes an independent correctness hazard.

## Actual S5 entry (2026-10-16)

`hal::power::request_s5_quiesced` enables ACPI mode through the validated FADT SMI command
when necessary, preserves unrelated PM1 bits and issues the actual SLP_TYP/SLP_EN write.
Dual-control blocks and unsupported formats are refused rather than guessed. The caller
must already have stopped DMA and other CPUs; this is explicitly an unsafe low-level
primitive, not a claim that the production power/quiesce policy exists.

The dedicated `hal-power-test-s5` feature calls it only after all boot/memory tests, refusing
active PCI bus masters. `scripts/check-power-s5.py` locally observed QMP `SHUTDOWN` with
`guest=true`, `reason=guest-shutdown` and QEMU exit zero on pc and Q35 (SATA disabled in
the positive Q35 no-DMA fixture). An active virtio-net interface was refused without a
shutdown event. Portable tests cover preserved control bits and validated ACPI-enable/port
fields. A dedicated CI job is configured; this does **not** close S3 or device-power policy.

S5/full-regression run [38962036000](https://github.com/NeXuS4Dev/NOLX/actions/runs/38962036000) on `9f8c704` passed
**all eleven jobs**: the new QMP shutdown/refusal job, six virtio-net configurations,
three persisted-file/reboot/malformed-media backend jobs, complete boot smoke, host tests,
lint/unsafe audit and image/layout checks. PWR-010/011 are now `tested-T2`. Locally, both
overlapping host commands passed 692 tests each (zero failures), default and S5-feature
release kernel Clippy passed, all 241 unsafe blocks had safety invariants, and the final
image passed all 21 layout checks. No PR has been opened. **M9 is still not complete:**
BAR/bridge allocation, persistent/multi-device lifecycle, xHCI/HID/input objects, evaluated
AML/resources and global power-quiesce/S3 remain the unchecked acceptance gates.


## Persistent healthy-driver lifecycle (2026-10-16)

The system now keeps one registry beyond probe return. Framework callbacks and retained I/O
check out its state without holding an IRQ lock across hardware waits. Reentrant/concurrent
operations return Busy; public I/O is refused unless its driver is Started. Storage keeps its
transport after borrowed filesystem tests, including NVMe admin queues/identify DMA ownership.
Sector operations reject empty/unaligned, overflow and capacity-exceeding requests; partitions
are checked against physical transport capacity. Both native NICs now reclaim DMA after reset
(plus explicit RX/TX disable for e1000), PCI BME disable and IRQ-state detachment.

Local Rust 1.88.0 / QEMU 11.0.2 tests use
`make kernel KERNEL_FEATURES=hal-driver-lifecycle-test` and
`python3 scripts/check-driver-lifecycle.py`. Five independent fixtures plus one containing all
five drivers each exercise three stop/restart cycles, retained disk read/write/flush/readback or
fresh ARP RX/TX, running-removal refusal, removal and restart refusal. Each cycle proves exact
DMA reclamation/reacquisition: virtio-blk 2 pages, AHCI 2, NVMe 6, e1000 11, virtio-net 4.
Every fixture finishes with zero pinned DMA pages. The host independently verifies the final
sector-1700 write on each disk. The combined fixture has **one controller of each kind**, not
multiple controllers of one kind. Dedicated CI job: `Persistent five-driver hardware lifecycle`.

Restart testing exposed e1000's boot-only assumption that IF remains masked: its ISR can
recycle a descriptor before the poll observes DD. Readiness now accepts a fresh IRQ counter as
well as DD, and waits against the current start's counter baseline instead of the first boot's.

**Still open at that checkpoint (later binding updates below):** singleton native contexts/fixed storage queue IDs and segment-zero binding;
MMIO mappings remain monotonic (no unmap/reuse), and interrupt-vector/routing resource release
is not complete. Failed initialization deliberately pins DMA rather than freeing possibly active
hardware buffers; failure ownership/recovery is not yet complete. These healthy-path tests do
not close the full lifecycle, power quiescence, or multi-device M9 gates.

Local regression for this checkpoint: 695 tests passed in each overlapping host command;
host and default/feature kernel Clippy and formatting passed; unsafe audit 246 blocks / zero
missing invariants; conformance 324 assertions / zero problems. All six lifecycle fixtures,
six native virtio-net configurations, three independent filesystem backends, host mounts,
reboots and nine malformed-media refusals passed. Lifecycle storage cycles additionally prove
that malformed, out-of-capacity and overflowing retained requests dispatch zero commands.

CI run [38968513895](https://github.com/NeXuS4Dev/NOLX/actions/runs/38968513895) on
`66c5d8e` passed **all twelve jobs**, including the new six-fixture lifecycle job, complete
boot smoke, three storage persistence/corruption jobs, native virtio-net and actual S5/refusal.
DF-008/009 record only the newly proven healthy native lifecycle/reentry properties as T2;
DF-010 records stable identity as T1. The updated database has 327 assertions / zero problems.
No PR was opened; the full lifecycle and M9 completion gates remain unchecked as described above.


## Owned reusable MMIO windows (2026-10-17)

The boot-only monotonic mapper limitation above is superseded for healthy BSP lifecycle
operations. MMIO reservations and page-table edits now share an IRQ lock; no hardware wait
runs under it. A bounded first-fit allocator reuses retired ranges with guard pages and
non-wrapping generation identities. Register access through a retired `Region` returns Stale,
even after its address/slot is reused. Every native driver passes its framework handle explicitly
through mapping helpers, including MSI-X table setup; platform/ECAM mappings stay unowned.
Only successful stop retires owned mappings after the callback has detached CPU/IRQ users.
Failed-start mappings remain pinned with potentially active DMA rather than being freed blindly.

Unmap reclaims lower tables but preserves the shared MMIO PML4 child, so address spaces made
before driver restart keep seeing the same subtree. The kernel invalidates BSP translations
before recycling reservations. A failed map rolls back its partial leaves/empty lower tables
and reservation. Unmapping is explicitly unsafe: all window/raw-pointer/IRQ users must already
be retired; this is **not** a general concurrent-unmap or SMP TLB-shootdown implementation.

Five new portable tests cover hole/guard reuse, stale/double release, owner/capacity isolation,
shared-root visibility through a second address space, and failed-leaf cleanup. The seven native
lifecycle fixtures now check MMIO mapping counts decrease at stop and return exactly at restart
on every cycle. An after-boot selftest forces table-allocation OOM, checks reservation/frame
rollback, fills then invalidates a real LAPIC alias, verifies stale read/write refusal after
VA reuse, and checks shared-root identity. These checks passed locally on QEMU 11.0.2.

Local regression: 700 tests passed per overlapping host command; host/feature kernel Clippy,
formatting, all seven lifecycle fixtures, all six native virtio-net configurations, 21 kernel-image
checks and unsafe audit (249 blocks, zero missing invariants) passed. Multi-controller binding,
failure recovery, interrupt-route/vector reclamation and the other unchecked M9 gates remain open.

The seventh fixture contains all five native driver kinds on Q35 (`sata=off` avoids the
unused onboard AHCI controller being selected first). It asserts real ECAM reads/writes and
then runs the same stop/restart/removal checks: the unowned platform configuration window
survives while driver-owned windows are reclaimed. This also passed locally.

### NVMe regression found during MMIO verification

Do not treat the first MMIO CI runs as green: run
[39009409943](https://github.com/NeXuS4Dev/NOLX/actions/runs/39009409943) on `09830d6`
failed boot smoke, and run
[39009517583](https://github.com/NeXuS4Dev/NOLX/actions/runs/39009517583) on `c4325eb`
failed the isolated NVMe boot. The latter serial log reports filesystem `Io(110)` (completion
poll timeout), after 157 block requests. The lifecycle job passed in both runs; that does not
supersede the regression failure.

Inspection found NVMe's completion/reset poll still had the old CPU-speed-only million-loop
budget, unlike AHCI/virtio waits. It now uses the same two-second HPET deadline, with a finite
100-million-iteration ceiling and million-iteration clockless fallback. This fixes the timeout
policy defect; the old logs do not establish why a particular completion was delayed. A kernel
selftest exercises immediate success and an always-false predicate through the actual deadline;
all seven lifecycle fixtures assert its marker. Sixteen additional local NVMe boots (four in
parallel, each with xHCI and e1000) passed filesystem/retained-read checks after the change;
host mount/reboot and three malformed-media refusals also passed again.


## Multi-controller native storage (2026-10-17)

All matching segment-zero PCI storage functions are now offered to the framework, not just
its first matching BDF. Each driver retains BDF, transport/DMA state, diagnostics, completion
counters and IRQ-visible state per framework handle. AHCI acknowledges only the selected HBA's
port/controller causes. MSI/MSI-X vectors are distinct per controller: compatibility vectors
0x51/0x52/0x50 remain for each backend's first binding; additional virtio/AHCI/NVMe instances use
bounded reserved ranges 0x60..0x6f / 0x90..0x9f / 0xa0..0xaf respectively. This is a static
reservation scheme, **not** a completed dynamic interrupt-vector allocator/release policy.

A host-tested bounded binding table assigns each instance a stable block ID. IDs 0/1/2 remain
preferred for the first virtio/AHCI/NVMe controller; further instances take disjoint extra IDs.
IDs and framework records are not recycled after removal. Public block I/O checks the selected
handle's Started state, never another running device of the same driver. Every data transfer
uses that controller's own block queue. Synchronous operations remain globally serialized;
these tests do not claim parallel request dispatch or demonstrate concurrent merging.
Repeated enumeration refuses an already recorded BDF without allocating resources or resetting
its controller; failed/removed records also retain their identity (no automatic hotplug retry).

`make kernel KERNEL_FEATURES=hal-driver-multi-test` and
`python3 scripts/check-storage-multi.py` run ten fixtures: two controllers of each kind
separately and all six together, on both PC/legacy and Q35/ECAM, plus two virtio controllers
behind a firmware-configured PCI bridge on PC, and all six storage controllers alongside both
native NIC kinds on Q35. Each medium has a different host-seeded tag.
Kernel tests verify distinct identities and writes, stopped-target read/write refusal, continued
peer I/O while each controller is stopped, and identity after restart. They additionally repeat
all storage probes and assert unchanged handles/DMA/MMIO resources. Then every controller goes
through the existing three-cycle reclaim/restart/removal tests. The host verifies every image's
own tag/write and final lifecycle write, unique BDFs/block IDs/vectors, and zero pinned DMA.
All ten fixtures passed locally on QEMU 11.0.2; the seven existing lifecycle fixtures passed too.
Dedicated CI job: `Multi-controller storage identity and lifecycle`.

**Limits at the storage checkpoint:** networking was still singleton (superseded below); native binding is still
segment zero; AHCI exposes only the first linked SATA port per controller and NVMe namespace 1.
BARs/bridge windows remain firmware-assigned, capacity is bounded, and there is no automatic
hotplug/rebind policy. Failed initialization/stop recovery, IRQ-resource reclamation, USB/HID,
AML and power gates are not closed by this checkpoint. **M9 is still not complete.**

Local host regression passed 702 tests in each overlapping command. Host and multi-feature
kernel Clippy, formatting, boot/layout/conformance checks and unsafe audit (249 blocks / zero
missing invariants) passed. Current assertion database: 332 assertions / zero problems.

CI run [39011468648](https://github.com/NeXuS4Dev/NOLX/actions/runs/39011468648)
on `18ab7d1` passed **all thirteen jobs**, including the ten multi-controller fixtures,
seven existing lifecycle fixtures and the default smoke/three-backend persistence matrix.
Local default-kernel regressions also passed all three backends' host-mount/reboot and three
malformed-media refusals, six virtio-net fixtures, three S5 fixtures and 21 Multiboot checks.
No pull request was opened for this checkpoint.


## Multi-adapter native networking (2026-10-17)

Native e1000 and virtio-net now enumerate every matching segment-zero PCI function. Each
framework handle retains its original BDF, MAC, transport state, receive FIFO, completion
counters and diagnostics. Restart uses that BDF, not the next first-match search result.
Repeated probing rejects previously registered BDFs without resetting live adapters or changing
handles/DMA/MMIO counts. Removed/failed identities remain reserved; automatic hotplug retry and
resource recycling are still not implemented.

Both backends expose exact-handle `mac`, `send_frame_on` and `receive_frame_on` endpoints.
Admission checks both Started state and the bound driver name under framework exclusion;
a peer cannot authorize a stopped, removed, invalid or wrong-type handle. Recursive public
network I/O returns Busy. e1000 now has a reusable, wrapping synchronous TX path as well as
its retained receive FIFO. Virtio-net compatibility endpoints retain the original first
binding rather than silently failing over to another adapter.

For shared e1000 INTx, the handler scans **every** controller assigned that vector, acknowledges
each controller's ICR and services its own RX ring before the common APIC EOI. Stop disables
only that controller and removes only its IRQ-visible state; it does not mask a peer's shared
route. Virtio-net keeps 0x53/0x54 for the first binding and reserves 0xb0..0xcf for subsequent
per-handle RX/TX pairs, disjoint from the storage reservations. This remains bounded static
reservation, not dynamic vector allocation/reclamation. Firmware's legacy Interrupt Line
continues to select e1000 routes; evaluated ACPI PCI routing is a separate open gate.

Build `hal-driver-net-multi-test` and run `scripts/check-network-multi.py` for eight fixtures:
two of each kind and all four together on PC and Q35, an explicitly shared-INTx e1000 pair,
and a mixed four-adapter secondary-bus fixture. The kernel checks unique MACs, duplicate-probe
refusal, exact-target refusal, continued peer wire I/O while each target is stopped, unchanged
identity and resources after restart, and three further stop/restart/removal cycles. A burst
submits on every NIC before draining any public FIFO. Every exchange must receive its own
fresh matched ARP reply and advance the selected adapter's receive-interrupt counter.
Independent host pcaps verify **178** uniquely tagged public ARP requests and gateway replies
across those eight fixtures, without requests appearing on another adapter's capture.

Build both `hal-driver-multi-test,hal-driver-net-multi-test` and run
`scripts/check-network-multi.py --storage-fixture-only` for the ninth fixture: two of each
storage backend plus two of each NIC kind on Q35. Its 36 additional public ARP exchanges and
all six media tags/writes are independently checked; ten devices complete lifecycle removal
with zero pinned DMA. All nine fixtures passed locally on QEMU 11.0.2. Dedicated CI job:
`Multi-adapter network wire identity and lifecycle`.

**M9 remains open.** Native binding is still segment zero; operations are synchronous and
registry-serialized, not parallel dispatch/SMP verification. Firmware BAR/bridge allocation,
ACPI interrupt discovery, failed-start/failed-stop recovery and IRQ-resource reclamation,
xHCI/HID/input objects, evaluated AML resources and global power-quiesce/S3 remain required.

CI run [39013817622](https://github.com/NeXuS4Dev/NOLX/actions/runs/39013817622)
on `9566b32` passed **all fourteen jobs**, including all nine network fixtures, ten storage
multi-controller fixtures and seven existing lifecycle fixtures. Local regressions passed
702 host tests in each overlapping command, host/default/combined-feature Clippy, formatting,
boot/layout/conformance checks, 21 Multiboot checks, six single-adapter network fixtures,
three backend persistence/reboot/malformed-media suites and three power fixtures. Unsafe audit:
249 blocks / zero missing invariants. Assertion database: 334 assertions / zero problems.
No pull request was opened for this checkpoint.


## Native BAR and bridge-window allocation (2026-10-17)

`nolx-pci-core::resources` now plans missing BAR addresses and closed type-1 memory/I/O
windows across an already numbered segment-zero tree. Existing BARs/windows are reservations;
new allocations are power-of-two aligned and checked for overlap and ancestor forwarding.
Memory64/prefetchable BARs can be assigned below 4 GiB through non-prefetchable bridge windows.
Bridge demand is computed bottom-up and allocated top-down, largest first; nested windows
use 1 MiB memory and 4 KiB I/O granularity. Incomplete/ambiguous topology, unrouted fixed
resources, malformed sizes, aliases and insufficient aperture space refuse the plan.

BAR sizing now distinguishes an implemented zero-address BAR from an unused slot, retains a
64-bit BAR's upper slot even at address zero, rejects non-power-of-two masks, and sizes only
two slots in type-1 headers. Programming disables decoding/bus mastering on touched functions,
checks every proposed address/window and command through readback, and rolls back on refusal.
Command and Secondary Status W1C bits are never copied into writes. An unverified rollback is
fatal to native bring-up; it is not advertised as safe recovery with an unreliable transport.
Twelve new host tests cover allocation, missing 32/64/I/O BARs, reservations, nested routing,
window/endpoint overlap, bad topology/sizes, exhaustion, refused writes and failed rollback.

The native boot hook runs **before driver startup**. The current board policy recognizes the
PC/i440FX and Q35 host IDs, requires an untruncated/nonmalformed loader memory map with no
reported memory in 0xc0000000..0xf0000000, reserves mapped ECAM, and uses 0xc000..0xf000 I/O
ports with chipset PM/SMBus reservations. It never chooses this aperture on unknown host
bridges or ambiguous maps. This is an explicit bounded board profile, **not** evaluated ACPI
`_CRS` or a general platform resource policy. Unrecoverable planning/programming errors halt
before any driver binds. Unknown profiles retain firmware configuration without repair.
Late PCI inventory reporting now uses a fixed boot-time BAR cache rather than temporarily
disabling decoding and re-sizing BARs of running controllers. The cache retains no heap memory.

`hal-pci-resource-test,hal-driver-multi-test,hal-driver-net-multi-test` builds an explicit
fault-injection kernel. Before planning, it clears supported devices' BAR addresses and bridge
memory/prefetch/I/O forwarding while retaining only firmware bus numbers and BAR type flags.
`scripts/check-pci-resources.py` runs six fixtures: PC and Q35, each with all ten devices at the
root, behind one bridge and behind two nested bridges. Every fixture rebuilds 21--23 BARs and
0/2/4 bridge windows, checks non-overlapping aligned addresses and readback, and completes the
existing six-media/four-NIC identity, peer-I/O, restart/removal and zero-pinned-DMA checks.
Independent pcaps verify 216 tagged public ARP exchanges; all six disk tags/writes are checked
on every boot. I/O BAR/window **configuration** is checked; data-path evidence is memory
mapped I/O, not a new legacy port-I/O driver. All six passed locally with QEMU 11.0.2.
Dedicated CI job: `Native PCI BAR and nested bridge resource recovery`.

**Still open:** general root-resource discovery, bus numbering, hotplug/rebalancing, expansion
ROM/CardBus resources, separate 64-bit prefetchable forwarding windows and nonzero-segment
native binding. A BAR already assigned outside a closed ancestor window is refused, not
relocated. Sibling-window conflicts are rejected, not repaired. This checkpoint does not
complete the remaining AML/PCI routing, recovery, USB/HID or global power/S3 gates. M9 remains
in progress.

All native driver/MSI-X memory mapping boundaries additionally require an **assigned nonzero**
BAR base. An implemented-but-unassigned zero BAR on an unsupported/no-repair profile is not
permission to map low physical RAM. The shared host-tested `assigned_memory_base` rule is
used by storage, network, xHCI and MSI-X mapping; zero/IO BARs are refused before mapping.

CI run [39016071195](https://github.com/NeXuS4Dev/NOLX/actions/runs/39016071195)
on `24d94db` passed **all fifteen jobs**, including the six native cleared-resource fixtures,
ten storage fixtures, nine network fixtures and seven lifecycle fixtures. Local regression
also passed 714 host tests in each overlapping command, host/default/all-feature Clippy,
formatting, boot/layout checks, 21 Multiboot checks, six single-NIC fixtures, all three
storage persistence/reboot/malformed-media suites and three power fixtures. Unsafe audit:
249 blocks / zero missing invariants. No pull request was opened.


## Bounded firmware AML evaluation (2026-10-17)

`nolx-power-core::aml` now loads DSDT/SSDT namespace declarations transactionally and executes
supported pure methods rather than scanning for `_STA`/`_CRS` byte patterns. It handles scoped
names, parent search, aliases (including method arity), deferred forward data/package references,
integer expressions, arguments/locals, method calls, integer local stores, Return/If/Else/While,
and literal buffers/packages. Scope opens existing containers; a missing descendant never falls
through into another same-named subtree. External completion rolls back on failed table loads.

Bounds are 2048 objects, combined parser/evaluator nesting 32, 10000 steps per table load or
evaluation, 16 KiB buffers/strings, 256 package elements, 64 KiB aggregate value size, 256 KiB
namespace-owned data, 16 KiB total method-argument data including representation overhead,
and 1 MiB per AML payload. Compound local/argument stores, global mutation, operation-region/
field reads, synchronization and buffer-field mutation are refused rather than emulated. Unknown
module instructions reject the table atomically; failed methods never default to successful status.
Resource decoding checks descriptor-local extents, exact known lengths, ranges/overflow, optional
nonzero end-tag checksum and trailing zero padding. IRQ trigger/polarity/sharing and root-window
semantics are not represented by this limited resource catalog: it must not authorize PCI routing.

The kernel evaluates firmware before PCI driver startup, defaults only an absent `_STA` to 15,
skips disabled-device `_CRS`, and retains at most 128 fixed-size records. Unknown or malformed
resources remain unknown, not empty/available. `firmware_catalog_complete()` is false on any
reported refusal; `firmware_device(index)` copies one record, not the whole array onto the 128 KiB
boot stack. Native testing caught and fixed an earlier whole-array stack-overflow regression.
Interpreter allocations are released before existing memory/heap tests; the catalog owns no heap.

Local Rust 1.88.0 / QEMU 11.0.2 verification: 47 power-core tests (31 new), seven AML boots via
`scripts/check-aml.py`: PC/Q35 computed status and three resources; disabled, unsupported,
malformed, looping and recursive methods; ordinary no-injected-SSDT boots; transactional
late-opcode table refusal; and a no-ACPI VM. Both machine profiles load their real SeaBIOS DSDT
and evaluate actual keyboard/serial `_STA` and `_CRS` methods. Every fixture completes timer,
address-space and zero-leak heap checks. The `acpi-aml` CI job repeats these executable checks and passed in the CI run recorded below.

This does **not** implement PCI `_PRT`/link routing, general root `_CRS` aperture consumption,
operation-region providers, general AML, device power methods, USB HID or S3/global quiescence.
Real root/link methods and several descriptor types are explicitly refused. Existing PCI board
apertures/Interrupt Line policy is unchanged. The literal sleep-package helper remains separate.
M9 remains incomplete, and no new pull request is requested or opened.


CI run [39022540188](https://github.com/NeXuS4Dev/NOLX/actions/runs/39022540188) on
`353f5d0` passed **all sixteen jobs**, including the new firmware AML job and all existing
storage-file/reboot/malformed-media, lifecycle, multi-storage/network, PCI-resource, S5,
virtio-network, ECAM/PIC/PIT, PS/2, boot and memory regressions. PWR-012/013/014 are now
`tested-T1` and PWR-015 is `tested-T2`; conformance contains 341 assertions. Local checks
also passed 743 host tests in each overlapping command, host/default/all-compatible-nonfault
kernel Clippy, formatting, boot/layout/user-program checks, all 21 native image checks and the
249-block unsafe audit (zero missing invariants). The kernel's mutually exclusive fault-injection
features intentionally forbid plain `--all-features`; no test claims that combination works.
The six cleared-BAR/nested-bridge fixtures and three S5 fixtures were rerun locally after AML
integration and passed, including 216 host-captured ARP exchanges and zero pinned DMA.
No general firmware routing, USB/HID, failed-lifecycle recovery or global power/S3 gate is closed
by these results. **M9 is still not complete. No PR was opened.**

The integer model is selected from the DSDT revision and inherited by all SSDTs: revision 1
uses 32-bit constants, arguments, logical results and wrapping arithmetic; revision 2+ uses
64 bits. Native evaluation refuses a missing/invalid DSDT instead of guessing an SSDT integer
model. Two additional host tests cover width boundaries and overflow; 47 power-core tests now
pass. Historical 743-test CI evidence above is unchanged; the width correction is verified
separately on the subsequent revision.
Local verification of the width correction passed 745 tests in each overlapping host command,
host and compatible-feature kernel Clippy, and all seven native firmware AML fixtures.
Namespace name lookup also restricts parent search to single unprefixed NameSeg references;
DualName/MultiName paths stay in their specified scope, even when no local prefix exists.
This rule was cross-checked against ACPICA `AcpiNsLookup` in
[`nsaccess.c`](https://github.com/acpica/acpica/blob/master/source/components/namespace/nsaccess.c)
and added to the existing namespace-shadowing regression test.


## Evaluated PCI root aperture consumption (2026-10-17)

`nolx-power-core::root` separately decodes Word/DWord/QWord address spaces with producer/
consumer ownership retained, plus fixed I/O/memory reservations. Fixed positive decode,
zero granularity/translation, exact bounds/length, supported flags and complete EndTag/checksum
are required; unknown descriptors, resource-source extensions and overlapping producer windows
refuse the entire template. No lossy device-resource record is promoted into allocation authority.
Discovery evaluates PNP0A03/PNP0A08 `_HID`, usable `_STA`, optional `_SEG`/`_BBN` and `_CRS`;
only one segment-zero/base-bus-zero root with a full 0..255 produced bus range is supported.
A failing segment/base method never gets the absent-member zero default. Incomplete table loads
and ambiguous roots prohibit allocation, while unrelated device-status errors remain explicit.

The native planner now consumes multiple evaluated root windows rather than inventing its
old 0xc0000000..0xf0000000 memory aperture. It subtracts every loader-map region regardless
of RAM/reserved type, root consumer descriptors, decoded firmware-device reservations, MCFG
and other live MMIO mappings, chipset PM/SMBus ports, and existing BARs/bridge reservations.
The verified board IDs are still required. Allocations remain above the cached 2 GiB direct
map and below 0xf0000000, with conservative I/O limits 0xc000..0xf000; read-only/prefetch-only
memory apertures do not authorize ordinary MMIO. Native testing caught initial allocations
below the direct map, which MMIO correctly refused as conflicting cache aliases; the planner
now excludes that entire mapping, not just populated RAM. Missing ACPI alone retains the old
bounded fallback. An advertised but invalid/unmapped ACPI root never downgrades to it.

Local Rust 1.88.0 / QEMU 11.0.2 verification: ten new root tests and two disjoint-pool tests;
57 power-core and 48 PCI-core tests passed. Eight cleared-firmware-assignment fixtures cover
PC/Q35 root/one/two nested bridges at 512 MiB, PC at 1536 MiB and Q35 at 3072 MiB: 21/22/23
BARs, 0/2/4 windows, six real disks, four NIC endpoints, 288 host-captured ARP exchanges and
zero pinned DMA. Every assigned memory range is checked against the evaluated producer
apertures and every loader-map exclusion. The AML harness now also checks failing `_SEG`,
multiple roots, incomplete SSDT loads, true no-ACPI fallback and an advertised ACPI root beyond
the direct-map limit (PC with 3 GiB); it has thirteen fixtures in total. Dedicated CI results must
be recorded separately from these local runs.

Still bounded, not general PCI resource discovery: no new bus numbering, rebalancing, arbitrary
roots/nonzero segments, translated windows, ROM BARs or native allocations above 4 GiB.
Firmware tables beyond 2 GiB are refused rather than mapped; PC with 3 GiB demonstrates this
negative path, while Q35's default relocated-memory layout remains supported. IRQ link methods,
operation-region providers, xHCI/HID, failed-lifecycle recovery and global device power/S3 remain
open. Existing physical PCI bindings are retained when allocation is refused; this is not a new
general `_STA`-based driver-binding or power policy. **M9 remains incomplete. No PR opened.**


CI run [39026163686](https://github.com/NeXuS4Dev/NOLX/actions/runs/39026163686) on
`7d58998` passed **all sixteen jobs**, including the eight real root-aperture resource fixtures,
the initial twelve AML/refusal fixtures, and the complete existing storage/network/lifecycle,
S5, ECAM/PS2/PIC/PIT, boot and memory regressions. PWR-016 and PCI-025 are `tested-T1`;
PCI-026 is `tested-T2`. Conformance contains 344 assertions. Local checks passed 757 host
tests in each overlapping command, host/default/compatible-feature kernel Clippy, formatting,
boot/layout/user-program checks, all 21 native image checks and the unchanged 249-block
unsafe audit (zero missing invariants).

A follow-up hardening case also revokes allocation when the 128-device firmware catalog
truncates, so incomplete consumer reservations cannot authorize new BARs; it adds a thirteenth
native AML fixture. Non-integer/non-string `_HID` results now refuse discovery explicitly, with
an additional assertion in the existing malformed-member test. All thirteen AML fixtures and
eight resource fixtures were rerun locally after these changes. The 3 GiB Q35 positive fixture
explicitly caps low RAM at 2 GiB for deterministic firmware-table placement across QEMU
versions; it does not claim support for firmware tables beyond the existing direct map.
The subsequent revision's CI result must be checked independently. **No PR opened; M9 is
still incomplete for the unchecked acceptance gates above.**


## Evaluated PCI INTx checkpoint (2026-10-17)

This supersedes the earlier checkpoint statements that native routing still uses Interrupt
Line or that all operation-region/link evaluation is unwired. It does not complete M9.

- Evaluate the actual root/bridge `_PRT` packages and link `_STA`/`_CRS`, preserving polarity,
  trigger and sharing flags from single current Small IRQ/Extended IRQ descriptors. Direct
  GSIs retain PCI defaults. No `_PRS` guesses, `_SRS`, general firmware writes or link allocation.
- Select the validated `_PIC(Arg0)` Store-to-integer subset **before** root resources and routes;
  Q35 takes the actual APIC table. Unknown/unsupported `_PIC` is a routing refusal, not PIC fallback.
- Snapshot only verified PIIX/ICH9 root-bus PIRQ bytes; support read-only ByteAccess PCI fields
  with static segment-zero/root-bus metadata. Method-private bounded buffers/DWord fields support
  PC IQCR without writing namespace buffers, hardware, or normal global data. Recursive metadata
  methods are refused, and retained region operands count toward the namespace allocation budget.
- Resolve scoped buses from existing numbered bridges and apply standard slot/pin swizzling only
  on undescribed child buses. Missing/ambiguous rows, malformed tables and incomplete catalogs
  revoke routing authority; cache publication is transactional and interpreter heap is released.
- Program/read back covered GSI3…23 with fixed vectors 0x23…0x37, excluding PIT/keyboard/mouse
  and MADT NMI reservations. Shared owners must agree on electrical flags. Failed programming
  stays masked. These are fixed reservations, not dynamic vector allocation/reclamation.
- No advertised-but-refused ACPI namespace may fall back to Interrupt Line. The no-ACPI legacy
  policy remains separate. Unsupported strings in Source, nonzero SourceIndex for links, general
  fields, multiple-current-IRQ choices, edge/unshared PCI links and broader topologies are refused.

Local Rust 1.88 checks: **768 host tests** in each overlapping command, host and compatible-feature
native Clippy, formatting and the unchanged 249-block unsafe audit passed. Eleven new host tests
cover descriptor/package ambiguity, bridge scopes/swizzling/refusal and bounded AML effects.
`check-network-multi.py --intx-routing-only` passed **14 fixtures**: eight PC/Q35 direct/shared/
bridged/nested pairs with Interrupt Line poisoned/read back as 0xff and preserved pins, plus six
malformed-table/unmapped-scope/unsupported-PIC refusal boots. The eight positive fixtures verified
112 captured public ARP exchanges, per-adapter ISR progress and zero pinned DMA after lifecycle.
All eight ten-device native BAR/resource fixtures also passed **with line poisoning enabled**:
six storage controllers plus four NICs, 288 captured exchanges, media checks and zero pinned DMA.
The existing eight network fixtures passed too. CI evidence for this revision is recorded separately.
**No PR opened. General PCI/link allocation, USB/HID, failed-lifecycle recovery and global power/S3
remain open; M9 is incomplete.**


### Firmware-version compatibility correction

The first INTx CI run (`39030745438`, revision `9f30f54`) failed the older PC firmware path:
its `_PRT` constructs 128 rows at runtime with local package copies, Index stores and Increment,
instead of returning QEMU 11's static package. This was a real regression, not a reason to restore
Interrupt Line fallback. Bounded owned local-package construction now supports that method with
a frame-wide 64 KiB/depth budget; Index targets are local packages only, with null reference outputs.
Namespace/argument/index-reference writes and general buffer stores remain refused. Two added host
tests execute the full legacy 128-row algorithm (including the slot-one SCI exception), check every
address/pin/link, and reject escaping/out-of-range targets and recursive package amplification.
The corrected host suite has **770 tests**, including thirteen new INTx-related tests overall.
The corrected revision must pass CI independently; the failed first run is not passing evidence.


## Native direct-root USB boot-HID checkpoint (2026-10-17)

This supersedes older statements that native xHCI stops at reset/port discovery. The default
kernel now retains a bounded live polled controller and scans it in idle context. Command/event
rings, DCBAA, 32/64-byte contexts, Enable/Address/Configure/Disable Slot, setup/data/status control
TDs, descriptor reads, Set Configuration/Set Protocol and interrupt-IN TDs enumerate direct-root
USB2 boot keyboards and relative mice. Actual configuration/interface/endpoint values are parsed;
malformed, composite/alternate, unsupported speeds/classes and scratchpad-requiring hosts refuse.
Transfer/event producer and consumer cycles wrap independently, with DMA cycle ownership checked
before reads and published last on writes. Report completions are matched to slot, endpoint and
pending TRB. A bounded source-tagged FIFO delivers input to the idle console consumer, as PS/2 does.

On unplug, delivery/submission is retired, Disable Slot must complete before the DCBAA entry is
cleared and all five device DMA pages are freed. Held keyboard keys and mouse buttons receive
neutral events. Unsupported tablet enumeration also disables/releases its temporary slot. The
three host ring/table pages intentionally remain owned while the controller stays live for hotplug.
Allocations within a device/host page batch roll back before pointers are exposed. Failed commands
or failed disable pin memory rather than freeing under DMA; comprehensive failure recovery is not
claimed. This is not yet framework-bound USB controller stop/removal, interrupt-driven xHCI, a
public input-object interface, multiple-host support, hubs, SuperSpeed or arbitrary HID reports.
Those remaining gates are **not** closed, and M9 remains incomplete. No PR opened.

`check-usb-input.py` passed on local PC and Q35 with QEMU 11.0.2. Each fixture checked **604 key
transitions**, real mouse motion/buttons, two full transfer/event ring wraps, unsupported HID
refusal, held-input unplug neutralization, reconnect and exact DMA ownership **13 -> 8 -> 3 -> 8 -> 3**
(two active devices to host-only baseline, then a new keyboard and back). It requires USB-tagged
output, not PS/2 input or enumeration logs alone. Three new host tests cover descriptor truncation,
malformed selection and endpoint/interval bounds. CI includes a dedicated default-kernel USB job;
its result must be verified separately from the local run.

Local verification also passed **773 host tests** in each overlapping command, host/default-native
Clippy with warnings denied, formatting, the 251-block unsafe audit (zero missing invariants), all
21 native image checks and the 351-assertion conformance database. The final USB verifier rerun
passed both VMs after fixing a test-only race: it now waits for the disconnect diagnostic as well
as the preceding DMA counter before asserting the complete serial evidence.


## Device-owned kernel input objects checkpoint (2026-10-17)

This supersedes the preceding checkpoint's console-only FIFO and kernel input-API limitation.
`nolx-input-core::objects` now implements bounded generation-checked device objects and reader
subscriptions. Source, class and physical location are validated. Devices at different ports keep
separate held-key/button state even if both report `UsbHid`. Connected-location duplicates refuse;
disconnected metadata is retained by readers, not by hardware buffers. Last-close reclaims it,
and device/reader generation exhaustion retires slots permanently rather than wrapping.

Each reader has its own FIFO and publication sequences. An atomic subscribe-plus-initial-state
operation avoids a query/open gap. A slow reader discards its obsolete journal and receives an
explicit latest-state Resync before any new records or EOF, without disturbing another reader.
On removal, only that device's held keys/buttons are neutralized. Readers drain neutral state or
resync then terminal Disconnected; new readers/writes refuse, and closed/reused handles are stale.
No input record contains a DMA pointer. USB producer retirement therefore precedes Disable Slot;
failed hardware quiescence still pins DMA rather than retaining an apparently connected input source.

Both PS/2 IRQ decoders and native USB completions now publish through the synchronized `hal::input`
API (16 device objects, 32 readers, 64 events per reader). Enumeration/info/subscribe/open/read/close
are kernel-facing public operations; producers cannot call consumers. The idle console is just one
subscription and prints outside the registry lock. It no longer drains private PS/2/USB FIFOs.
This does **not** implement Executive object headers/namespace/security/process handles, syscalls,
waitable reads or a POSIX input-device ABI. Nor does it complete USB controller/framework/power
integration, failed lifecycle recovery, arbitrary HID/hubs/SuperSpeed or global power/S3. M9 remains
incomplete and no PR is opened.

Twelve new portable object tests passed (including 1024 object/reader reuse cycles), bringing the
host suite to **785 tests** in each overlapping command. Native PC/Q35 tests with
`hal-input-object-test` retained the existing 604 console key transitions and wire-device DMA
reclamation, while independent consumers verified 600 ordered hardware events, explicit slow-reader
resync with current held state, neutral EOF, reader-slot reuse and stale producer/reader/open refusal
after device-generation reuse. The actual PS/2 IRQ test also passed through independent keyboard and
pointer subscriptions with matching device identities. These observers inspect real producer data;
they do not inject fabricated records. CI now runs default USB, observer USB and observer PS/2 paths.
The revision's CI result must be recorded separately from local evidence.

Final local checks passed all six native paths: default and observer USB on both PC/Q35, plus
default and observer PS/2. Host/observer-native Clippy, formatting, the unchanged 251-block unsafe
audit (zero missing invariants), all 21 default image checks and the 354-assertion conformance
database passed. Default and observer verifiers use distinct socket/log names so their evidence
cannot accidentally come from another running VM.


## Persistent xHCI controller lifecycle checkpoint (2026-10-17)

Supersedes the preceding single-host/non-framework limitations, not the general USB or power gates.
Every matching segment-zero PCI xHCI function is now offered to the persistent registry exactly once.
Each handle retains its own BDF, log, owned MMIO window, command/event/DCBAA pages, root slots and
input producers. Idle polling admits only the exact Started xHCI handle under framework checkout;
callbacks run outside the framework IRQ lock and reentrant public polling returns Busy. Duplicate
probe and removed/failed identities cannot create a second owner. This remains the bounded direct-root
USB2 boot-HID profile (eight slots/sixteen ports per host, shared input/framework capacity limits),
not arbitrary PCI segments, hubs, composites, SuperSpeed or interrupt-driven USB.

Stop first retires input producers, giving readers neutral state and EOF independently of DMA.
Only acknowledged halt/reset plus PCI bus-master-disable readback permit releasing device and host
DMA. Successful framework stop then releases owned MMIO; old Region generations stay invalid after
restart. Host metadata is installed **before** exposing the first DMA pointer, so a post-run start
error can execute the same checked rollback. Unproven quiescence leaves all DMA/MMIO owned and pinned;
failed-handle polling/start/removal refuse. There is deliberately no force-free or automatic recovery
from quarantine. Removal of a safely rolled-back failed record is allowed.

`scripts/check-xhci-lifecycle.py` runs three images on **both PC and Q35**, each with two physical PCI
controllers: a keyboard on one and a mouse on its peer. The healthy image performs three full restarts,
requires distinct real key presses after every restart, and gates progress on new mouse motion while
the keyboard host is stopped. It verifies persistent peer input identity, neutral keyboard/pointer EOF,
stale producer/reader/open/MMIO rejection, duplicate probes, bus-master readback, DMA/MMIO baselines,
allocator audits and final removal. DMA follows **16 -> 8 -> 16** for three cycles, then **16 -> 8 -> 0**.
The start-fault image injects failure after Run with three exposed host pages and proves retained owner
metadata, rollback to the pre-start DMA/MMIO baseline, live peer input and safe failed-record removal.
The stop-fault image withholds quiescence proof on the final peer: **eight pages and its MMIO stay
pinned with PCI bus mastering still enabled**, input becomes neutral/EOF and removal is refused.
This last fixture intentionally does not claim reclamation or shutdown safety.

All six new native fixtures passed locally with QEMU 11.0.2. CI includes these modes in the USB job;
the exact revision CI result is recorded separately. Existing default/independent-observer USB and
PS/2 paths remain regression requirements. **M9 is still incomplete**: Executive/user input bindings,
general USB/interrupt delivery, quarantine recovery, global power quiescence/S3, general AML/PCI roots
and nonzero-segment binding remain open. No PR is created by this checkpoint.


Checkpoint verification also passed **785 host tests** in each overlapping suite, host and native
feature Clippy, all six default/observer USB/PS2 regressions, and seven storage/network framework
lifecycle fixtures including combined PC/Q35 and recursive USB-poll Busy refusal. Formatting,
**254 unsafe blocks / zero missing invariants**, boot/layout/generated-file checks, **21 default
image checks** and **357 conformance assertions** passed. The verifier waits for complete terminal
messages (not partially written serial prefixes) before evaluating final ownership evidence.


## Fail-closed BSP device shutdown checkpoint (2026-10-17)

The persistent registry now owns an irreversible admission freeze. Once shutdown starts, new
registration/binding/start/stop/removal and public block/network/USB I/O refuse Busy. Diagnostic
snapshots remain available. One framework checkout spans preflight, all flushes, all stops and
post-stop resource release; it never holds the framework IRQ lock over callbacks. Failed records
refuse preflight. Every started driver must pass its barrier before **any** controller is stopped;
stops run in reverse discovery order. Only successful stop permits owned MMIO reclamation. Any
flush/stop failure leaves admission closed, earlier stops intact and unsafe-to-free state pinned.
Unknown started drivers refuse rather than silently bypassing policy. There is no retry/resume
or forced free after a partial shutdown.

`hal::power::quiesce_devices` integrates the six native driver types. Storage uses retained private
FLUSH commands under the checked-out framework, not recursive public admission. PS/2 producers
are retired and IRQ1/IRQ12 masked after successful controller stops. Completion additionally
requires zero owned DMA pages, an allocator audit, and PCI bus-master-disable readback from a
complete scan of **all 256 segment-zero buses**; truncated inventories, malformed bridges and
nonzero-segment MCFG descriptions refuse. An active foreign PCI function is never disabled just
to authorize S5. Register/transport owners cannot restart after the final scan.

Five new `nolx-df-core` tests cover flush-before-stop ordering, reverse stop/release order,
permanent mutation freeze, all-or-nothing barriers, partial-stop failure, failed-record preflight
and empty/repeated shutdown. The host suite now totals **790 tests** in each overlapping suite.
`scripts/check-power-quiesce.py` exercises four modes on **PC and Q35** (eight native fixtures):

- Healthy: all six native driver types, three real disk backends and real held USB key/button
  input. It writes new backend-specific data before shutdown; barriers and all six stops succeed,
  USB/PS2 subscriptions drain neutral state/EOF, new I/O/start/repeated shutdown refuse, DMA drops
  **38 -> 0**, and QMP observes `guest-shutdown` with exit zero. The host independently checks the
  new raw sectors and mounts all three NOLXFS files (8192 bytes, CRC `0x9ea14b00`).
- Flush fault: the injected failed barrier stops no devices, retains all 38 DMA pages and refuses
  S5 with permanent admission freeze. This is a deliberate failure injection, not a disk-fault claim.
- Stop fault: withheld xHCI quiescence proof leaves its Failed record/DMA/MMIO pinned; earlier
  successful reverse-order stops stay stopped, removal refuses and no S5 occurs.
- Unowned master: a test-only VGA PCI bus-master bit remains enabled and unchanged after all
  registered drivers stop/DMA reaches zero. The final audit refuses S5; no force-disable is used.

All eight fixtures passed locally with QEMU 11.0.2, with native feature Clippy and host checks.
CI extends the existing S5 job with these modes and independent disk verification. Final revision
CI evidence is reported separately; no PR is created.

**Scope is device-level BSP shutdown, not complete system suspend/power management.** The public
quiesce operation does not commit higher-layer dirty caches, terminate producers/user tasks, park
other CPUs or stop firmware DMA. The unsafe S5 primitive still requires those preconditions;
its dedicated fixture has completed users/filesystem/cache tests and no APs running. S3 state
save/restore, D-state transitions, general multi-segment/firmware power control, interrupt resource
reclamation and recovery from failed shutdown remain open. M9 is not complete.

Final local hygiene also passed host/native Clippy, formatting, all **256 unsafe blocks with
zero missing invariants**, **21 default image checks**, boot/layout/generated-file checks and
**361 conformance assertions**. Existing default USB PC/Q35 and PS/2 input, seven storage/network
lifecycle fixtures, and the original three primitive-only S5 fixtures passed as regressions.
