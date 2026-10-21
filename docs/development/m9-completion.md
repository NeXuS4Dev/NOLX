# M9 completion gate

M9 is **not complete**. Passing the current CI suite proves only the assertions it exercises.
This checklist tracks the remaining integration work against ROADMAP.md, without shrinking
M9 to the already implemented boot probes. Check items only with executable evidence.

**Verification update (2026-10-20):** Exact-revision CI run [39462936440](https://github.com/NeXuS4Dev/NOLX/actions/runs/39462936440)
on `ee0253b36a8383377f2794e49fe61d0ee4bcc9d0` passed all 19 jobs, including the new audited
kernel `_SRS` backend, its host tests, and the disabled-link QEMU fixture (checkpoint at the end
of this file), plus the kernel build, formatting, Clippy, unsafe audit, image/layout and
conformance checks. A production kernel backend now applies planned link `_SRS` writes to the
whitelisted PIRQ bytes, re-evaluates live `_CRS` as the transaction postcondition, and publishes
routes only from a fresh discovery; on Q35 the same preflight retains the fixed GSIA..GSIH
routes with zero writes. The segment-qualified PCI host tests also ran; no nonzero-segment
QEMU/native-binding fixture ran, so native binding remains unverified. This is host/QEMU CI
evidence, not physical-hardware verification. `_PRT`/link reprogramming beyond the verified
segment-zero board profile and the broader open M9 gates below remain incomplete.

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
      discovery/programming is verified, and unavailable current routes on the steerable
      PIIX profile are now reprogrammed through the audited `_SRS` backend (checkpoints
      below); link reprogramming beyond that verified ByteAcc/PIRQ-byte profile and
      unsupported root/topology/source models remain open.
- [x] Assign missing memory32/memory64/I/O BARs and closed type-1 memory/I/O bridge
      windows on the bounded PC/Q35 segment-zero profile, with readback and rollback.
- [x] Consume evaluated host-bridge `_CRS` producer apertures for bounded PC/Q35 missing
      BAR/window assignment; subtract complete loader maps, consumers and mapped windows.
- [ ] Generalize beyond one segment-zero root: bus-number allocation, existing-resource
      rebalancing, translated/variable windows and 64-bit prefetchable forwarding.
- [x] Bind multiple virtio-blk/AHCI/NVMe PCI controllers on segment zero, including
      secondary buses, with per-controller queues, interrupts and independent lifecycle.
- [x] Multi-adapter native e1000/virtio-net contexts with exact-handle public RX/TX.
- [ ] Native nonzero-segment binding. The segment-qualified `Bdf` identity and native scan
      grouping for successfully mapped MCFG allocations compile, and the host tests pass in
      CI run [39391895780](https://github.com/NeXuS4Dev/NOLX/actions/runs/39391895780). There is
      still no multi-segment QEMU/native fixture or end-to-end nonzero-segment binding evidence.
      Nonzero-segment `_PRT`/e1000 INTx and BAR/bridge resource assignment remain refused or
      unsupported, so this gate stays open.
- [x] Persistent system registry; retain native storage/network state past probe return;
      healthy stop/restart/removal with hardware quiescence and exact DMA reclamation.
- [x] Serialized, reusable, generation-checked MMIO windows with partial-map rollback
      and shared-root-preserving unmap on healthy stop (BSP only).
- [x] Checked e1000 partial-start/failed-stop recovery with independently retained MMIO/DMA,
      per-source shared-INTx detachment, explicit reset/PCI proof and fresh captured peer traffic.
- [x] Checked modern/transitional virtio-net partial-start/failed-stop recovery, with
      retained queue/MMIO ownership, MSI-X detachment/readback and captured peer traffic.
- [x] Checked modern/transitional virtio-blk partial-start/failed-stop recovery, including
      published-request quarantine, independent durable peer I/O and unchanged-media reboot.
- [ ] Complete failed-start/failed-stop recovery for remaining native drivers and general
      interrupt-resource reclamation. All six native driver kinds (xHCI, e1000, virtio-net,
      virtio-blk, AHCI, NVMe) have checked recovery; the shared dynamic allocator now covers
      xHCI, virtio-net and e1000 as well as storage, including shared-GSI e1000 references and
      APIC acknowledgment of unowned managed vectors. CI run [39391895780](https://github.com/NeXuS4Dev/NOLX/actions/runs/39391895780)
      passed the changed recovery/build checks. This bounded checkpoint does not close general
      interrupt-resource reclamation or every exhaustion case. PS/2/input-object paths still
      have no framework recovery contract; remaining segment binding and firmware routing are
      tracked separately above.

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
      checked xHCI failed-start/failed-stop recovery is now exercised with repeated refusal;
      MSI/MSI-X input delivery now has IRQ-only idle admission and explicit polling fallbacks;
      failed-transfer retirement, root-slot repair and command-timeout quarantine now have
      executable checks; full/high-speed HID and USB3 EP0/class-refusal reclamation are checked below;
      USB 2.0 hub enumeration, hub-port hotplug and subtree teardown are implemented with host rules
      (USB-032..USB-035) and a PC/Q35 fixture (USB-036..USB-038, `tested-T2` in run [39655463587](https://github.com/NeXuS4Dev/NOLX/actions/runs/39655463587) on `01c5aa3`);
      hardware-verified SuperSpeed HID and general endpoint-level recovery remain open.
- [x] Per-controller MSI-X/MSI boot-HID delivery with IRQ-only idle admission, bounded event
      batches, safe stop/re-arm and explicit no-capability/PIC-only polling fallbacks (below).
- [x] HID boot keyboard/mouse reports delivered to bounded kernel input event objects,
      with independent subscriptions, source/device identity, state resync and stale-handle refusal.
- [x] Executive/process-handle, security/namespace and waitable input bindings: every
      attached device publishes an `InputDevice` object at `\\Device\\InputN` with a DACL
      (SYSTEM owner; user 1001 refused; user 1000 read+synchronize), a type mask without any
      write right, a per-incarnation name that is withdrawn on disconnect, one handle per
      device opened into the spawned process, and an auto-reset waitable set by the
      publication and cleared by the draining reader. Boot evidence below; INP-004/INP-005
      are `tested-T2`.
- [x] User-facing `read(2)`-shaped path on top of those objects: `open(2)` resolves the
      Executive name `\\Device\\InputN` (DACL-checked through the same `open` the spawn path
      uses, `O_RDONLY` only, no synthetic VFS inode), each descriptor owns a non-blocking
      reader bound to the device incarnation, `read(2)` returns one eight-byte record per
      call (`nolx-input-core::wire`), `close(2)` releases the reader and a later read on that
      number is `EBADF`, and a process exit releases whatever it still held. Empty queue is
      `EAGAIN`; a disconnected device drains and then returns zero bytes. The boot fixture
      (`scripts/check-ps2-input.py --read-t2`, `hal-input-read-test` image) injects one key
      through QMP, so the record travels the real i8042/IRQ1/decoder/producer/registry path
      into a ring-3 `read(2)`, and run
      [39633910912](https://github.com/NeXuS4Dev/NOLX/actions/runs/39633910912) on `7f0d208`
      passed all 19 jobs with it. INP-006 is `tested-T1`, INP-007/INP-008 `tested-T2`.
- [x] A scheduler-blocking input wait: `poll(2)` on an input descriptor validates the caller's
      eight-byte `struct pollfd` array once at call time, suspends the calling thread in the
      scheduler when nothing is ready, and is completed by the idle loop when a producer
      publishes or the deadline passes (`kernel/src/exec/user/mod.rs`). `revents` is written back
      for every entry, so a readiness query never consumes the record it reports; `POLLNVAL`
      covers stdio and descriptors this kernel does not serve. `read(2)` keeps its POSIX
      behaviour: an empty queue is still `EAGAIN`, and the descriptor stays non-blocking. The
      ring-3 program `inputwait` (`hal-input-wait-test`) polls for 20 ms (must expire with zero),
      then blocks for five seconds; the boot fixture injects relative pointer motion only after
      the kernel prints `input: poll blocked`, and the program exits 42 only when the wake
      reports one ready descriptor with `POLLIN` and the following `read(2)` returns the injected
      record field for field. A machine with no USB pointer reports the phase as skipped and
      keeps booting. INP-010..INP-012.
- [x] USB disconnect/hotplug cleanup and endpoint/ring ownership; do not leak controller
      resources or leave DMA active after device removal. Root-port unplug/reconnect has been
      checked since USB-011; a hub now adds route-scoped unplug, leaf-first subtree teardown and
      a status-change endpoint that wakes the controller, with the neutral state of a held button
      or key published before the slot is disabled (USB-036..USB-038).
- [x] Inject PS/2 keyboard/mouse events under QEMU and assert delivered event contents.
      USB injection and bounded kernel-object consumers now also have PC/Q35 hardware checks;
      general USB, Executive bindings and general device/endpoint recovery remain open.

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
      The read snapshot plus the `_SRS` byte-write backend now cover the verified PIIX/ICH9
      PIRQ fields on segment zero/root bus zero; general providers remain open.
      The step before the providers is done: firmware resource templates now decode in the
      firmware's own field order (Word/DWord/QWord windows, extended interrupts, DMA channels),
      so the PC boot publishes 16 device templates instead of 9 (`PWR-022..PWR-025`, verified in
      this sandbox, CI pending) and the root bridge's producer apertures are no longer reserved
      as device resources.
      Region *reads* are provided: `Field` elements over SystemMemory, SystemIo and
      EmbeddedControl regions resolve to bounded fields read through a caller's
      `RegionFieldReader`, with unit widths from the access flags, alignment and region bounds
      enforced before the provider is called, and every read journaled (`PWR-026`). The kernel
      authorizes only windows another validated table named — the HPET block, the FADT PM1a
      control block — so the HPET device's own `_STA` now reads its registers: a PC boot reports
      `\_SB_.HPET _STA=0xf, defaulted=false` and `17 resource results, 0 refusals`, Q35 reports
      `28 resource results, 0 refusals` (`PWR-027`; run
      [39620576799](https://github.com/NeXuS4Dev/NOLX/actions/runs/39620576799) on `528f4f4`
      passed all 19 jobs). Region *writes* are planned
      too, but only against a caller's exact field-and-value permission: the intent is journaled,
      applied by a transaction that re-checks permissions, reads before writing, verifies the
      readback and restores attempted writes in reverse order, and the kernel backend writes only
      inside the same authorized windows (`PWR-028`, host-tested; no PC/Q35 method in this
      profile writes a region field, so no fixture exercises it yet). Still open, and why this
      box stays unticked: an embedded-controller handshake the kernel does not have, a
      firmware-exercised write path, and the remaining AML operations beyond the verified ones.
- [x] Replace the fixed memory allocation aperture with evaluated root `_CRS` windows on
      the bounded PC/Q35 profile; ACPI refusal never selects the no-ACPI board fallback.
- [ ] Consume evaluated link resources for PCI interrupt routing and complete general
      device binding, replacing the firmware Interrupt Line fallback. Current `_CRS`
      resources and, when a current route is unavailable on the bounded PIIX profile,
      `_PRS`-selected `_SRS` reprogramming are consumed with live `_CRS` confirmation;
      general device binding beyond evaluated PCI INTx remains open.
- [ ] Complete device power-state/quiesce policy and S3 save/resume path.
      BSP device-level shutdown freeze/flush/stop and fail-closed S5 gates now have six-driver
      PC/Q35 evidence; higher-layer cache/user-task/SMP orchestration and S3 remain open.
- [x] ACPI S5 entry with QMP observing actual guest shutdown and QEMU exit zero, not a
      printed hypothetical word. The unsafe primitive requires prior system quiescence;
      its test VM has no active DMA, and a negative test refuses an active bus master.

## Final verification

- [x] Host tests and negative/malformed-input tests for new portable contracts (CI run
      [39406985079](https://github.com/NeXuS4Dev/NOLX/actions/runs/39406985079)).
- [x] Kernel build, formatting, Clippy, unsafe audit, image/layout/conformance checks (same
      19-job CI run).
- [ ] Dedicated boot tests for each new hardware path, including absent-device fallbacks;
      nonzero-segment binding and some allocator-exhaustion cases still lack fixtures.
      `scripts/check-absent-devices.py` now covers the absent-controller half of this item
      (see the 2026-10-21 checkpoint).
- [x] Separate virtio/AHCI/NVMe disk boots with persistent file checksums (same CI run).
- [x] Assertion database, README and roadmap agree on the final state and cite CI evidence
      (408 assertions, 0 problems, 0 `pending-T2`; README and ROADMAP both cite run
      [39609375684](https://github.com/NeXuS4Dev/NOLX/actions/runs/39609375684) on `e16ee67`).
- [x] All checks on the final revision pass: run
      [39609375684](https://github.com/NeXuS4Dev/NOLX/actions/runs/39609375684) on `e16ee67`
      passed all 19 jobs on 2026-10-21. This item gates the *checks*; the boot-test item above is
      still open, so M9 stays **in progress**.

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
  GSIs retain PCI defaults. Current route discovery uses no `_PRS` guesses, `_SRS`, general
  firmware writes or link allocation.
- A portable helper parses one simple `_PRS` IRQ descriptor: Small/Extended IRQs, including
  Small IRQ wake bit 5 and legal mode/polarity combinations, plus Extended IRQ producer/consumer
  usage. It builds a candidate `_SRS` buffer from `_CRS`, preserving descriptor order/type/length,
  ignored Small IRQ bits, and recalculating a present checksum. `select_link_route` prefers an
  available current `_CRS` route, otherwise chooses an available level/shared `_PRS` option after
  caller-supplied ISA-to-GSI mapping, checks that `_SRS` has one argument when a reprogramming
  candidate is needed, and returns the candidate buffer. These selector, effects-planner, and portable transaction host tests passed in exact-revision CI [39420993249](https://github.com/NeXuS4Dev/NOLX/actions/runs/39420993249).
  `plan_link_srs` software-evaluates the selected `_SRS` body in a bounded AML frame, including
  DWord buffer fields sourced from the candidate `Arg0`, AML locals, or method-private named
  buffers, and returns an ordered journal only for exact, caller-permitted PCI-config byte fields
  (ByteAcc, Preserve, aligned 8-bit fields on the bounded segment-zero/root-bus-zero profile).
  Other field shapes and unpermitted values are refused; failed evaluation returns no partial
  journal. `MethodEffects` also retains the unique PCI field read-set used during planning.
  `apply_pci_field_writes` validates the whole permission set before hardware access, checks the
  planning read-set before its first write, applies exact byte writes with readback, runs a caller
  postcondition, and reverses/validates attempted writes if any step fails. Ordinary `Namespace::evaluate` remains read-only. This is
  still only a portable helper: no kernel backend applies the journal, re-evaluates live `_CRS`,
  or publishes the resulting route. General link reprogramming stays open.
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


## Checked xHCI quarantine recovery checkpoint (2026-10-17)

This supersedes the earlier statement that quarantined xHCI records have no explicit recovery.
`Registry::recover` admits **only Failed** records while admission is open and invokes that record's
original stop callback, never its start callback. Failure preserves Failed; success moves to Stopped
without starting hardware. Explicit start is a separate operation. Frozen shutdown admission always
returns Busy, including after failed shutdown; recovery cannot thaw it. Diagnostics/driver identity
remain stable. Platform admission is deliberately restricted to **xHCI**: the other native drivers'
partial-start cleanup is not yet audited and their recovery requests refuse.

The native wrapper holds framework checkout across the retry and MMIO release. xHCI reuses its
existing halt/reset, PCI bus-master-disable/readback and unique DMA-owner teardown. If any proof is
missing, host metadata, pages and mapping stay owned. Only successful callback completion permits
MMIO release. Duplicate PCI probes cannot create a second owner while the record is quarantined.
Old input objects remain retired/neutral/EOF through recovery; explicit restart allocates new object
and MMIO generations. No implicit enumeration/restart, forced DMA free, or automatic retry is added.

Three new portable framework tests cover original-driver/selected-handle dispatch, failed startup,
repeated cleanup refusal, successful transition without implicit start, peer isolation, invalid states
and recovery refusal after failed shutdown. **793 host tests** pass in each overlapping suite.
`scripts/check-xhci-lifecycle.py --mode recovery` (`hal-xhci-recovery-test`) passed on local PC and
Q35 with two physical controllers. Its dedicated hooks inject failure after Run, refuse startup
rollback and one explicit recovery, and prove **11 DMA pages/MMIO stay pinned**. Real mouse input
on the peer then gates successful cleanup and explicit restart, followed by a fresh actual key press.
It next refuses stop and one cleanup retry with a held key: **16 pages/MMIO remain pinned**, the
old reader drains neutral EOF, and old producer/reader/open tokens refuse. A new peer mouse report
gates successful recovery and restart; a different real key proves the new producer works. Stale
MMIO never revives, peer identity stays unchanged, final successful stops/removal yield **DMA 0**,
and a final admission freeze refuses recovery. Allocator audits and DMA/MMIO baselines are checked.
These are deliberate proof-refusal hooks, not a claim that a physically broken controller was repaired.

The existing power-quiescence fixtures additionally assert recovery Busy for every handle after
shutdown freeze, including the Failed xHCI in the injected stop-failure fixture. The reentrant
framework test asserts recovery Busy during callbacks and proves an unaudited Failed synthetic
driver cannot invoke its stop callback again through native recovery. CI includes the new recovery mode in the
USB job; exact revision CI evidence is recorded separately. No PR is created.

M9 remains incomplete. This is bounded controller recovery for polled direct-root USB2 boot HID,
not general endpoint/stall recovery, hubs/composites/SuperSpeed, USB interrupts, other drivers'
failed-start recovery, IRQ/vector resource reclamation, Executive/user input bindings, general
AML/PCI segment support, higher-layer/SMP shutdown orchestration, D-state policy or S3 resume.


Final local verification passed all **20 native fixtures** in this checkpoint: eight xHCI
recovery/lifecycle/fault PC/Q35 boots, default USB PC/Q35 and PS/2, seven storage/network lifecycle
boots and two failed-shutdown boots with recovery refusal. Host/native Clippy, formatting,
**257 unsafe blocks / zero missing invariants**, **21 default image checks**, boot/layout/generated
checks and **363 conformance assertions** passed. These local results are separate from CI evidence.


## xHCI interrupt delivery checkpoint (2026-10-17)

This supersedes the earlier **polled-only** xHCI limitation, not the remaining general USB gates.
The supported direct-root USB2 boot-HID profile now uses per-framework-handle vectors `0xd0..0xdf`.
MSI-X entry zero is programmed through the existing owned BAR0 window, after validating that table
and PBA fit and do not overlap each other or the standard capability/port, runtime/interrupter or doorbell register blocks. MSI is supported
when MSI-X is absent. MSI-X tables outside this bounded BAR0 window are explicitly refused, not
silently relabelled as IRQ-driven. Missing message capability or local APIC selects an explicitly
logged polled profile; failed programming/delivery proof fails startup and uses checked rollback.

Startup and synchronous USB control/enumeration commands remain bounded/polled. After enumeration,
a No-op command and actual delivered vector prove the interrupt path without holding the host lock
while waiting. The top half only reads the retained source status, masks IE, acknowledges EINT/IP,
counts the interrupt and notifies idle. It does not lock the host/framework, touch rings, decode
input, log, allocate or reclaim DMA. IMAN IP need not remain set after MSI/MSI-X delivery; USBSTS
EINT is checked instead and no unrelated W1C status is echoed. The normal idle event batch is at
most 256 events. ERDP advances without clearing EHB per event; re-enable/clear-EHB at batch end
rechecks queued work, and a non-empty ring schedules continuation. Existing bounded synchronous
root-device enumeration can also consume events while waiting for its commands; this is not a
claim of asynchronous/real-time enumeration or a general USB scheduler.

A routed host does **not** consume events because the timer happened to fire: service requires a
claimed interrupt or continuation of its bounded batch. Stop retires input, detaches the software
source, masks controller IE/INTE, disables/read-backs PCI MSI/MSI-X, then performs the existing
halt/reset/bus-master proof before releasing DMA/MMIO. Failed quiescence keeps ownership pinned
with its source detached; recovery/restart proves a fresh route. The reserved vector handler still
EOIs late vectors without touching retired MMIO. Vectors are statically reserved, not dynamically
allocated/reclaimed; general interrupt-resource ownership and other-driver failure recovery remain open.

Executable verification:

- `hal-xhci-irq-test` + `check-usb-input.py --interrupts` and `--interrupts --transport msi`:
  PC/Q35, 604 actual HID report batches and 600+ real controller interrupts per fixture, 604 key
  transitions, mouse motion/button, two ring wraps, unsupported tablet refusal, unplug/reconnect
  and exact DMA `13 -> 8 -> 3 -> 8 -> 3`. The native observer asserts that an armed host without
  pending work refuses idle admission. Parser negative tests reject console-only, polling, zero
  counters, stale routes and one probe followed by 600 purported polled reports.
- Forced MSI and absent-capability profiles use `nec-usb-xhci`: older QEMU exposes its
  MSI/MSI-X switches there, not on `qemu-xhci`. Default MSI-X still uses `qemu-xhci`; no IRQ
  requirement is skipped because a model does not expose a property.
- The same image with `--transport polled` or `--no-apic`: PC/Q35 repeat the complete workload,
  require the explicit fallback marker and forbid IRQ-work markers.
- `hal-input-object-test,hal-xhci-irq-test`: PC/Q35 repeat IRQ-driven input with independent readers,
  overflow repair, neutral EOF and stale-generation refusal; the PS/2 observer also remains covered.
- `check-xhci-lifecycle.py --interrupts`: lifecycle/start-fault/stop-fault/recovery modes combine their
  existing feature with `hal-xhci-irq-test`. PC/Q35 require distinct controller vectors, advancing
  IRQ counters for every re-arm, actual IRQ-admitted fresh keys/peer mouse reports and existing
  DMA/MMIO/stale-object assertions. Native stopped/recovered/quarantined checks inject a late
  reserved vector and assert its source is detached and its delivery counter unchanged.
- `--mode irq-start-fault`, `hal-xhci-irq-start-fault-test`: inject failure **after** HID enumeration,
  MSI-X setup and a delivered IRQ, then require safe rollback, ignored late vectors, actual peer
  input, failed-record removal and final DMA zero. This complements the earlier post-Run fault,
  which occurs before message delivery is armed.

The USB CI job builds and runs all these profiles. Four new portable tests cover W1C/EHB/dequeue
and malformed/overlapping MSI-X window rules. General USB/hubs/SuperSpeed/endpoint recovery,
Executive/user input bindings, nonzero-segment native drivers, other-driver failure recovery,
general AML/PCI routing and device D-states/S3/SMP power remain open. **M9 remains incomplete.**


Local checkpoint verification passed **37 distinct native fixtures**: 21 interrupt/input/object/
xHCI lifecycle fixtures, three default USB/PS2 fixtures, seven existing storage/network lifecycle
fixtures and six normal/failed six-driver shutdown fixtures. Both overlapping Rust host suites
passed **797 tests** each; five negative proof-parser tests, host/native Clippy, formatting, image
validation (21 checks), boot/link/layout/generated/conformance checks (**369 assertions**) and the
unsafe audit (**257 blocks, zero missing SAFETY comments**) also passed. The complete CI workflow
is required on the pushed revision; these local results do not close the remaining M9 gates.


## xHCI failed-transfer and root-slot repair checkpoint (2026-10-18)

This extends the IRQ/lifecycle checkpoint to failed **device transfers**, not just controller
start/stop faults. Ordinary interrupt-IN completion now matches event type, slot, endpoint and
outstanding aligned TRB pointer before considering status or reading data; retired/foreign and
Event Data completions cannot consume or rearm a Normal TRB. Complete successful reports decode;
short reports skip stale trailing bytes and rearm; invalid residuals or unsuccessful completions
retire the public producer to neutral EOF, clear pending work and never rearm the failed device.

Idle cleanup tries Disable Slot once. Only its successful matching completion permits DCBAA
pointer removal and release of device DMA. Failure retains every device page and a distinct
quarantined state; subsequent timer ticks, IRQs or physical reconnection cannot silently retry or
bind a second slot for that port. Other root ports/controllers remain serviceable. Kernel-only
`repair_port_on` admits only the exact Started xHCI handle and a quarantined slot or safely retired
faulted port, under framework callback exclusion. It retries checked cleanup, then resets and
re-enumerates only the connected root device. Healthy ports, stale/unknown handles and invalid
ports refuse. This is not a user syscall or Executive input-object binding.

Command completions are tracked conservatively: missing/access-invalid/mismatched outcomes,
reserved/unknown codes and stopped/aborted command-ring results forbid further command submission.
No root repair can clear this poison. A checked controller stop/reset and explicit restart can;
all unproven DMA remains pinned until that hardware proof. This prevents timeout retries from
wrapping/reusing command pointers while older commands could still complete. A known unsuccessful
terminal completion allows an explicit retry, but never authorizes DMA reclamation by itself.

`hal-xhci-transfer-fault-test` / `check-xhci-lifecycle.py --mode transfer-fault --interrupts`:

- Two physical PCI controllers, real keyboard/mouse reports and actual MSI-X. The dedicated fault
  image configures an unsupported keyboard IN endpoint 2 on its **own additional DMA ring**;
  QEMU's HID device returns STALL, xHCI writes code 6 and an actual Halted output context. No
  completion bytes are fabricated. The normal image contains none of this fault configuration.
- Hold `a`; two injected pre-submit Disable Slot refusals retain **17 pages**, including the test
  endpoint's extra ring, and neutral EOF. Only new peer mouse input permits checked slot repair;
  genuine Disable Slot returns the extra page and re-enumeration restores the ordinary 16-page
  baseline, unchanged controller/MMIO ownership and peer identity. Fresh `c` is required.
- Disconnect the held `c` keyboard. Hardware actually completes Disable Slot, but the test hook
  deliberately withholds that result from the ownership layer (a lost-proof simulation, not a
  claim that QEMU timed out). DMA stays pinned at 16, repeated repair/command reuse refuses, and
  new peer input gates whole-controller stop/reset/restart: `16 -> 8 -> 11`. Reconnect must
  publish a new input generation and deliver fresh `d` through interrupts.
- A second actual STALL has no cleanup refusal: successful automatic Disable Slot releases six
  device/test pages to the 11-page peer-plus-host baseline without binding a replacement. New
  peer input then gates explicit repair of that empty faulted root port; fresh `e`, neutral EOF,
  stale producer/reader/open/MMIO checks and final stop/removal require **DMA zero**.

The CI USB lifecycle loop includes this mode on PC and Q35. Four portable tests cover affinity,
short/malformed/error completions and command-ring reuse policy. This is bounded **root-slot**
repair, not Reset Endpoint/Set TR Dequeue Pointer/USB clear-halt recovery, arbitrary control-transfer
retries, asynchronous command cancellation, hubs or SuperSpeed. Those and the remaining firmware,
Executive/input, other-driver recovery and power/S3/SMP gates remain open. **M9 is not complete.**


Local checkpoint verification passed **39 distinct native fixtures**: the two new PC/Q35 transfer
failure fixtures plus all 37 preceding interrupt/input/lifecycle/shutdown regressions. Both
(overlapping) Rust host suites passed **801 tests** each; five negative IRQ-proof-parser tests,
host/native Clippy, formatting, default image validation (21 checks), boot/link/layout/generated
checks, **374 conformance assertions** and the unsafe audit (**257 blocks, zero missing SAFETY
comments**) passed. CI is required on the pushed revision; this does not close the remaining M9 gates.


## Speed-aware USB descriptors and native USB3 control checkpoint (2026-10-18)

The previous USB2-only early speed refusal is replaced with bounded default speed-ID1..4 handling.
EP0 starts at 8/64/512 bytes as appropriate; bMaxPacketSize0 is validated against the negotiated
speed, including USB3's exponent 9 (512 bytes). Unknown/custom/SSP speed IDs refuse before exposing
a slot. A disabled USB3 root uses a warm-reset write rather than USB2 PR; the write preserves only
power and does not replay change, link-state or other write-one action bits. **Warm reset has T1
write-rule tests, not a claim of native recovery testing**; the native USB3 devices arrive enabled.

`hid_at_speed` bounds endpoint packet size and interval, requires exactly one HID descriptor before
the endpoint, and requires exactly one immediately following USB3 interrupt companion. This profile
supports only bMaxBurst=0 and reserved bmAttributes=0, and validates nonzero bytes-per-interval against
packet capacity and boot-report length. Multiburst, streams, alternate/composite interfaces, custom
speed IDs and malformed/truncated/missing/duplicate/out-of-order descriptors refuse. Max ESIT payload
comes from that companion rather than maximum packet size. Host tests exercise both the distinct
payload and packet values. Existing callers of `hid` retain the bounded full-speed convenience API.

Executable hardware checks use `scripts/check-usb-input.py` with `hal-xhci-irq-test`:

- `--usb-version 1 --interrupts`: PC/Q35 must show actual speed 1 and EP0 8, 604 keyboard
  transitions, mouse reports, two transfer/event-ring wraps, neutral disconnect and reconnection,
  actual IRQ-admitted reports and exact DMA baselines.
- The existing default profile now additionally requires actual speed 3 and EP0 64 evidence.
- `--superspeed-control --interrupts`: PC/Q35 attach two successive real USB3 mass-storage devices.
  Each must complete Address Device and descriptor requests at actual speed 4/EP0 512, read its
  44-byte configuration, refuse the unsupported class and return to the 13-page live HID baseline.
  Fresh keyboard press AND release after each refusal prove the existing peer survives. No storage
  interface is bound or exposed as HID. The ordinary HID ring-wrap/hotplug workload still runs.

**Evidence boundary:** stock QEMU 8.2 and 11 HID devices support usb_version=1/2, not 3. Consequently
USB3 EP0/control/class refusal has T2 evidence; USB3 boot-HID companion selection and context payload
have T1 rules only, with NO claim of hardware-verified SuperSpeed HID reports, bursts, hubs, SSP, link
power transitions or S3. This does not close the general USB or other remaining M9 gates. No new PR.

Local verification: **43 distinct native fixtures passed**, including the four added speed profiles
and all 39 previous USB/PS2, interrupt, controller lifecycle, storage/network lifecycle and shutdown
cases. Both overlapping Rust suites passed **806 tests** each (USB core: 31). Host/native Clippy,
formatting, boot/Multiboot, 31 layout tests, generated files, five IRQ-proof parser tests and the
unsafe audit (257 blocks, zero missing SAFETY comments) passed. Assertion database: **379**.
Exact-revision CI is required after push; these checks do not mark M9 complete.


## Audited e1000 failed-controller recovery checkpoint (2026-10-18)

The native e1000 path now retains `Region` before any register operation can fail, independently
of its successful `Report`. All eleven pages (RX ring/eight buffers/TX ring/frame) are allocated
before any new DMA address is exposed. An incomplete batch is uniquely owned and safely released
before publication; complete RX/TX owners move to retained slots before register programming.
Consequently failures during RX-only startup, TX completion waits or ARP/IRQ validation cannot lose
DMA ownership on stack unwinding. PCI memory access is enabled with bus mastering/INTx disabled
until addresses and the local IRQ source are ready. Readbacks guard PCI programming.

Failed start attempts checked cleanup automatically. Stop first detaches this adapter's software
IRQ source and blocks NIC/PCI INTx, **never globally masking a shared IOAPIC route**. Then it disables
both engines, observes reset completion, drains ICR and validates final controller/engine/mask and
PCI bus-master/INTx readbacks before releasing anything. Failed cleanup retains Region, RX and TX
owners and mappings; failed records refuse public MAC/TX/RX, ordinary restart and removal. The
framework's explicit recovery allowlist now includes e1000 alongside xHCI, not arbitrary drivers.
A successful retry moves Failed to Stopped, reclaims DMA and unmaps MMIO; restart remains explicit.

`hal-e1000-recovery-test` plus `scripts/check-e1000-recovery.py` passed PC/Q35 with two separate
user-network backends and a deliberately shared INTx route:

- Real boot ARP/IRQ validation completes before the first injected startup failure. Automatic
  rollback and a repeated explicit cleanup refuse; 22 pages/two mappings remain, failed I/O and
  removal refuse, duplicate probing cannot bind another owner, and the peer exchanges fresh data.
- Recovery reads back reset/PCI exclusion, reclaims exactly eleven target pages and its mapping,
  rejects the old Region and detaches late software-dispatched vectors. Explicit restart restores
  the same BDF/MAC/shared vector with fresh network exchanges.
- A failed stop and repeated recovery refusal exercise the same quarantine with bus mastering
  still active; the peer's shared INTx/RX/TX remain live.
- An injected allocation refusal after three actual pages releases that unexposed batch; safe
  rollback returns to the peer-only eleven-page/one-mapping baseline. Another injection after RX
  enables but before TX programming retains **all eleven** pages until checked recovery.
- Deliberately withholding an otherwise valid PCI exclusion proof retains every page even though
  real bus mastering already reads disabled. This is NOT a naturally failing QEMU PCI write/reset.
- Eleven nonce-tagged public ARP requests and matching gateway replies are checked byte-for-byte
  in independent host pcaps. Every exchange must advance the selected adapter's acknowledged RX
  interrupt counter; other adapters cannot authorize it or carry its request. Final checked
  stop/removal yields DMA zero, allocator audit passes, and permanent shutdown freeze rejects
  recovery and transport entry.

Portable tests cover every combination of six required stop-proof conditions, all nonzero IRQ
masks, absent-device all-ones reads and unrelated control bits. Evidence is BSP/segment-zero 82540EM
on PC/Q35, not physical-board fault injection. IOAPIC routes remain firmware-owned/static/shared;
this is per-source reclamation, not a general dynamic interrupt allocator. Runtime TX-timeout
recovery, storage/virtio-net partial-start recovery, general PCI/firmware, USB/Executive and S3/power
gates remain open. No PR is created and M9 is not marked complete.

Local verification passed **68 distinct native fixtures**: all 43 preceding USB/PS2, IRQ,
lifecycle and shutdown fixtures; eight multi-adapter fixtures; fourteen poisoned-line/firmware
routing/refusal fixtures; the combined six-storage/four-NIC fixture; and two new PC/Q35 failure
recovery fixtures. Both overlapping Rust host suites passed **808 tests** each. Host/native Clippy,
formatting, boot/Multiboot, 31 layout tests, generated files, five IRQ-proof-parser tests, **384
conformance assertions** and the unsafe audit (**259 blocks**, zero missing SAFETY comments) passed.
CI is still required on the pushed revision. This checkpoint does not close the remaining M9 gates.


## Audited virtio-net failed-controller recovery checkpoint (2026-10-18)

The modern transport retains its common configuration mapping immediately and keeps loose pages
and partially configured RX/TX queues in per-handle owners. Queue shape/notify checks precede moves
out of the loose-page array; each queue is retained before fallible address/vector/enable writes.
Complete queues move into a NIC only through an infallible step. Fallible DRIVER_OK, notify, TX/ARP
and IRQ validation runs in a scope that always restores the NIC owner before returning an error.
No failed start can strand hardware-visible pages in inaccessible local variables.

PCI memory access initially excludes bus mastering and INTx. Checked stop detaches `LIVE` first,
blocks legacy INTx, disables MSI, disables and function-masks MSI-X, waits for virtio status reset
readback zero, disables/reads PCI bus mastering, and revalidates message exclusion. Only then are
complete/partial/loose DMA owners released and MMIO retired. Every failure preserves them. The
interrupt handler refuses both detached sources and unassigned primary-vector aliases before
counting or touching a queue. Reservations remain static; this is not a dynamic IRQ allocator.

Framework recovery is now audited for xHCI/e1000/virtio-net only. Failed virtio-net public MAC/TX/RX,
ordinary restart and removal refuse even when a peer is Started. Explicit recovery retries checked
cleanup, moves only that record to Stopped, frees exactly four pages/five mappings and rejects the
old Region; restart remains explicit. Permanent shutdown freeze still refuses recovery/transport.

`hal-virtio-net-recovery-test` plus `scripts/check-virtio-net-recovery.py` passed four fixtures:
PC/Q35, each with modern-only/transitional target/peer roles swapped. They cover:

- A post-ARP startup failure after real RX/TX completions, failed automatic rollback and repeated
  failed explicit cleanup: both adapters' eight total DMA pages and mappings remain owned.
- Failed stop/repeated refusal with bus mastering still active; the peer retains its own RX/TX
  interrupts and exchanges fresh traffic while the target's source is detached.
- An allocation refusal after three actual pages, with checked rollback to the peer-only four-page
  baseline. RX-queue-only startup failure instead pins all four target pages: native readbacks
  verify RX enabled, TX disabled and DRIVER_OK clear, with PCI bus mastering still off.
- Deliberately withheld otherwise valid PCI proof after real reset/master-disable: all pages stay
  pinned until a later checked recovery. This is not a real QEMU reset or PCI-write failure.
- Late software-dispatched RX/TX vectors and unused primary aliases cannot change retired/unassigned
  counters. Eleven unique public ARP exchanges per fixture require selected RX IRQ progress, while
  TX completion waits require that adapter's TX IRQ. Independent pcaps verify exact request/reply
  bytes, nonce IPs and absence of cross-adapter requests. Final checked stop/removal yields DMA zero,
  allocator audit succeeds, and frozen recovery/transport refuses.

Two portable tests cover every status byte and combination of PCI DMA, INTx and message-exclusion
proof, plus unrelated PCI bits. Scope remains BSP/segment-zero modern virtio transport, including
transitional PCI devices exposing it. Legacy-only virtio, multiqueue/offload, runtime timeout policy,
AHCI/NVMe failed-start recovery, general USB/Executive/firmware/PCI and device-power/S3 remain open.
M9 is not complete; no PR is created.

Local verification passed **78 distinct native fixtures**: all 68 preceding USB/input, hardware
lifecycle/shutdown, multi-adapter/firmware-routing and e1000 recovery fixtures, six standalone
virtio-net wire/absent/no-APIC profiles, and four new recovery profiles. Both overlapping Rust
suites passed **810 tests** each. Host/native Clippy, formatting, boot/Multiboot, 31 layout tests,
generated files, five IRQ-proof-parser tests, **389 conformance assertions** and the unsafe audit
(**260 blocks**, zero missing SAFETY comments) passed. Exact-revision CI is required after push;
these checks do not close the remaining M9 gates.


## Audited virtio-blk failed-controller recovery checkpoint (2026-10-18)

The block path now independently retains the common mapping, two loose pages, partially
programmed ring and complete disk across every fallible register/validation/I/O step. Shape
admission precedes ownership transfer/address publication: a header/data/status chain requires
at least four ring entries, the bounded ring must fit one owner page, and the notification register
must be aligned and contained in its mapping. QUEUE_ENABLE is read back. PCI bus mastering remains
off through partial queue setup; all complete owners are retained before DRIVER_OK/DMA admission.
Fallible filesystem validation restores the local disk to retained state even on error.

Healthy stop flushes first. Failed startup does **not** issue another command. Any `issue` error
closes the transport and IRQ source immediately; no later public read/write/flush or ordinary stop
flush can overwrite descriptor zero or its request buffer while a command may still be active.
The framework record is not automatically transitioned by runtime I/O failure: the caller sees an
error, and explicit stop/reset is required. There is no transparent retry or promise of preserving
an unacknowledged write. A refused stop flush detaches/masks its source and retains ownership;
explicit Failed recovery resets it without retrying that unsafe barrier.

Before release, stop/recovery detaches the source, blocks INTx, disables MSI/MSI-X and function-masks
MSI-X with readback, acknowledges virtio reset at status zero, disables/read-backs PCI bus mastering,
revalidates message controls and detaches the software block queue. Any missing proof retains all
DMA/MMIO, including loose or partially configured owners. The checked PCI/proof operations are now
shared with virtio-net; its public proof alias remains compatible. The IRQ handler uses atomic
primary identity, rejects detached/unknown vectors and unused primary aliases, and never locks the
framework/block binding tables or touches retired mappings.

Framework recovery is now audited for **xHCI/e1000/virtio-net/virtio-blk** only. Failed public I/O,
removal and ordinary restart refuse with an independent Started peer. Recovery only cleans to
Stopped; restart is explicit, stale Regions refuse, and irreversible shutdown freeze cannot thaw.

`hal-virtio-blk-recovery-test` plus `scripts/check-virtio-blk-recovery.py` passed four profiles:
PC/Q35 with modern-only and transitional target/peer roles swapped and checked against actual
PCI device IDs. Each profile boots **twice on the same unchanged disks**:

- Post-filesystem startup failure and repeated cleanup refusals retain four total pages/eight
  mappings while peer queued I/O and interrupts remain live. Duplicate probe cannot rebind a BDF.
- An actual first allocated page is reclaimed on injected second-page allocation failure before
  descriptor exposure. Recovery/restart does not leak pages or windows.
- A read-back enabled queue with DRIVER_OK clear and PCI bus mastering off retains both pages
  across repeated refusal, then checked reset reclaims exactly two target pages/four mappings.
- Deliberately withheld final PCI proof retains ownership despite actual bus-master disable.
- A deliberately withheld completion after real request publication retains owners and submits
  exactly one request: no rollback flush or descriptor/buffer reuse occurs before checked reset.
- Refused stop flush quarantines without reset/free; explicit recovery does not retry the flush.
- Each boot performs **fifteen uniquely tagged public write/flush/read exchanges** with selected
  controller IRQ progress. Media tags, separate block IDs, untouched peer LBAs and host image checks
  prove isolation. The host independently mounts each persisted NOLXFS checksum file after exit;
  both controllers verify the old file before writing on the second boot.
- Final stop/removal leaves DMA zero, rejects detached vectors and stale mappings, passes allocator
  audit, and refuses recovery/transport after freeze.

Fault hooks refuse cleanup/flush or withhold otherwise valid completion/PCI evidence. This is
**not** naturally failing reset/PCI-write hardware or physical-board verification. Static vector
reservations and BSP-only lifecycle remain; AHCI/NVMe recovery, general PCI/firmware routing,
USB/Executive and general device-power/S3 acceptance remain open. **M9 is not complete.**

Local verification passed **105 native fixture profiles**: the 99 preceding USB/input, hardware
lifecycle/shutdown, multi-adapter/firmware-routing, e1000 and virtio-net profiles, the four new
virtio-blk recovery profiles, and the added unowned-quiesce profiles. **22 further host-side
checks** independently mounted the persisted files after QEMU exited. Both overlapping Rust host
suites passed **812 tests** each. Host/native Clippy, formatting, boot/Multiboot, 31 layout tests,
generated files, five IRQ-proof-parser tests, **394 conformance assertions** and the unsafe audit
(**261 blocks**, zero missing SAFETY comments) passed. Exact-revision CI is required after push;
these checks do not close the remaining M9 gates.


## Audited AHCI and NVMe failed-controller recovery checkpoint (2026-10-18)

This supersedes the earlier statement that the storage transports had no audited recovery path.
It does not close the remaining M9 gates.

Both native storage transports now retain their register mapping, every DMA page and the
software command/queue state across every fallible step, in per-handle owners rather than
locals. Bring-up starts by disabling PCI bus mastering and message delivery for the
controller, so a partially configured controller cannot DMA before its owners exist. Bus
mastering is enabled only once the pages are owned, and the transport is marked live only when
its first command may actually be issued. A failed command quarantines the transport: the
public read/write/flush path refuses it, and no later request reuses the outstanding command,
descriptor, buffer or request page.

The checked stop is transport-specific and proven, not assumed. AHCI sets the command-list and
FIS-engine stop bits, polls their completion bits, clears the port's interrupt enable and the
HBA's MSI enable, flushes the write cache and reads the result back. NVMe writes a zero
control register, polls the controller's ready state away, issues a namespace flush and
disables plus function-masks MSI-X with readback. A missing proof keeps **every** page and
mapping pinned; only a fully proven stop releases them, and the released MMIO region refuses
later access. Recovery through the framework runs that same checked stop, moves the record from
Failed to Stopped without starting hardware, never touches a Started peer and cannot thaw the
irreversible shutdown freeze.

Framework recovery is now audited for **xHCI/e1000/virtio-net/virtio-blk/AHCI/NVMe**. Failed
public I/O, removal and ordinary restart refuse while a peer is Started. Recovery only cleans
to Stopped; an explicit restart is still required.

`hal-ahci-nvme-recovery-test` plus `scripts/check-ahci-nvme-recovery.py` passed four profiles:
AHCI and NVMe on PC and Q35, each with two real controllers on their own media. Every profile
boots **twice on unchanged media** and performs **fifteen uniquely tagged public
write/flush/read exchanges** per boot, each requiring the selected controller's own completion
interrupt to advance:

- A post-filesystem startup failure with repeated cleanup refusal keeps four total pages and
  every mapping owned; the peer keeps serving durable I/O, and a duplicate probe cannot rebind
  a BDF.
- An injected allocation failure releases the one page that was already taken before the port or
  controller named the buffer, and the rollback returns to the peer-only baseline.
- An enabled-port (AHCI) or enabled-controller (NVMe) partial startup keeps all its pages
  until the checked stop, then reclaims exactly two pages/one window (AHCI) or six pages/two
  mappings (NVMe).
- A deliberately withheld PCI exclusion proof retains every page although the hardware really
  is excluded. This is **not** a naturally failing QEMU PCI write.
- A published-command failure quarantines the controller without a rollback flush or any
  descriptor/buffer reuse before the checked stop.
- A refused stop flush quarantines without reset or free, and explicit recovery does not retry
  the unsafe barrier.
- The host independently verifies every written LBA, every untouched peer LBA and each media
  tag, and mounts the persisted NOLXFS file after each boot; the second boot verifies the old
  commit before writing again. Final DMA is zero and the frozen registry refuses recovery and
  public I/O.

Fault hooks refuse cleanup, allocation or flush, and withhold otherwise valid proof. This is
**not** naturally failing reset/PCI-write hardware or physical-board verification. Interrupt
vectors remain statically reserved, the transports stay BSP/segment-zero and expose only the
first SATA port or namespace 1 per controller. General/dynamic interrupt reclamation, USB and
input-object recovery, nonzero-segment binding, general PCI/firmware routing, Executive/user
input bindings and general device-power/S3 remain open. **M9 is not complete.**

Local verification passed **109 native fixture profiles**: the 105 preceding USB/input,
hardware lifecycle/shutdown, multi-adapter/firmware-routing, e1000, virtio-net and virtio-blk
profiles, plus the four new AHCI/NVMe recovery profiles. **38 further host-side checks**
independently mounted the persisted files after QEMU exited. Both overlapping Rust host suites
passed **825 tests** each. Host/native Clippy, formatting, boot/Multiboot, 31 layout tests,
generated files, five IRQ-proof-parser tests, the unsafe audit (**263 blocks**, zero missing
SAFETY comments) and the conformance database (**397 assertions**) passed. The full native
regression log is `build/ahci-nvme-all-native.log` (151 checks, exit zero).

CI run [39140774366](https://github.com/NeXuS4Dev/NOLX/actions/runs/39140774366) on
`f145bcd` passed **all nineteen jobs**, including the new `AHCI and NVMe checked failure
recovery` job and every preceding storage, network, USB, AML, PCI-resource, S5 and boot
regression. These checks still do not close the remaining M9 gates.


## Dynamically owned interrupt vectors checkpoint (2026-10-18)

This supersedes the earlier statement that the storage transports' interrupt vectors were a
static reservation. It does not close the remaining M9 gates.

The three native storage transports now own their completion vectors from a shared table
instead of computing one from the framework handle. `nolx-hal-core::irq_vectors` holds the
rules, host-tested: which vectors a kind of controller may own, the preferred vector the
first controller of a kind takes, the range the further controllers take from its lowest, and
the ledger entry that names a controller. `kernel/src/hal/irq_vectors.rs` keeps the table
itself: one atomic entry per vector, a compare-and-swap for the claim and for the release,
and no lock on the interrupt path.

A driver claims its vector when it starts, before the controller is told about it, so two
controllers can never share one. The vector is released only after the transport's own
checked stop has proved that the controller can no longer raise it: the port's engines
stopped, the port naming no buffer, the HBA's messages disabled and bus mastering off for
AHCI; the controller disabled, not ready, its namespace flushed and its MSI-X disabled and
function-masked for NVMe; the status reset to zero with the messages disabled and bus
mastering off for virtio-blk. A failed controller keeps its vector, because a controller that
may still raise an interrupt must keep the resource that names it. The release follows the
proof, never precedes it, and a release that does not match the owner is refused.

The vector-to-controller mapping is now the table's, not a formula. An interrupt on a vector
that no controller owns reaches no driver at all, and an interrupt on a released vector is
ignored rather than counted for a controller that no longer holds it.

`hal-ahci-nvme-recovery-test` and `hal-virtio-blk-recovery-test` prove the behaviour on
hardware. Each fixture checks that a failed controller still owns its vector, that a proven
stop frees it, that the restart takes the released vector again, that a vector no controller
owns advances no counter, and that the vector another controller released is taken by the
first one that asks again. The eight profiles (AHCI, NVMe and virtio-blk on PC and Q35) keep
their two unchanged-media boots, their fifteen tagged durable exchanges and their host-mounted
files.

The pool is bounded, and a driver that finds no free vector refuses to start instead of
sharing one: the bring-up fails with `NoVector` before the controller is told about any
vector, and the refusal leaves nothing behind. The three recovery fixtures prove it: with
every vector of the kind taken by a controller that does not exist, the start of a real
controller is refused, the refusal is reported as the missing vector rather than as a
device fault, and the controller starts again as soon as the vectors are free.

Local verification passed **128 native fixture profiles**: the 105 preceding USB/input,
hardware lifecycle/shutdown, multi-adapter/firmware-routing, e1000, virtio-net and virtio-blk
profiles, the four AHCI/NVMe recovery profiles, and the storage-file, driver-lifecycle and
storage-multi profiles of this checkpoint. **44 further host-side checks** independently
mounted the persisted files after QEMU exited. Both overlapping Rust host suites passed
**832 tests** each. Host/native Clippy, formatting, boot/Multiboot, 31 layout tests,
generated files, five IRQ-proof-parser tests, the unsafe audit (**263 blocks**, zero missing
SAFETY comments) and the conformance database (**400 assertions**) passed. The full native
regression log is `build/vectors-all-native.log` (172 checks, exit zero).

CI run [39163258921](https://github.com/NeXuS4Dev/NOLX/actions/runs/39163258921) on
`68bab9d` passed **all nineteen jobs**, including the `AHCI and NVMe checked failure
recovery` and `Virtio-blk checked failure recovery` jobs that now assert the reclamation and
the refusal of an exhausted pool. These checks still do not close the remaining M9 gates.

**Limits:** only the storage transports take part. The xHCI's per-handle vectors, the
virtio-net's receive/transmit pair and the e1000's firmware-routed INTx line are still
assigned statically, so this is a bounded step towards general interrupt-resource
reclamation, not a general dynamic allocator. The pools are fixed per driver kind and cannot
grow; the reclaim rule trusts the transport's own hardware proof; the table is BSP-local and
not SMP-safe against concurrent claims of the same vector beyond the compare-and-swap; and
the vectors themselves remain local APIC vectors, not an IOAPIC route. General reclamation,
nonzero-segment binding, USB/Executive bindings and general device-power/S3 remain open.
**M9 is not complete.**


## Shared dynamic-vector verification checkpoint (2026-10-20)

This checkpoint supersedes the earlier static-vector description, but **does not complete M9**.
The shared rules retain preferred vectors `0x50..0x55` and `0xd0`, with all kinds falling back
to the common bounded `0x60..0xdf` range while excluding every preferred slot. One kernel owner
table covers xHCI, virtio-blk, AHCI, NVMe, both virtio-net queues, and e1000 shared-GSI routes.
The managed domain is `0x50..0xdf`; central APIC dispatch EOIs an unowned vector in that domain
without calling a driver.

The current implementation claims xHCI vectors before message programming and releases them
only after the existing checked stop/bus-master proof. Virtio-net similarly claims both queue
vectors before MSI-X setup and releases the pair after checked stop. E1000 claims one vector per
GSI route; adapters sharing a GSI share that route with a reference count, and the final
reference masks/readbacks the I/O APIC entry before returning the vector. If a failed route
transaction cannot confirm its retry-mask readback (or matching owner release), the claim is
retained and a per-GSI quarantine refuses later acquisitions instead of leaking a second vector.
The e1000 recovery fixture injects a failed program readback followed by an unconfirmed remask
readback, then checks the claim remains owned and a retry makes no duplicate claim. This is
software readback fault injection, not physical-hardware-failure evidence. Recovery assertions,
network logs/checks and the xHCI interrupt-proof parser test allocated ownership rather than
static formulas. The verified revision also keeps e1000's source identity routable until both
the PCI INTx-disable bit and NIC interrupt mask read back, and makes virtio-net retry a partially
released pair without silently forgetting a vector now owned by an unexpected source.

A separate 2026-10-20 change adds a segment field to `Bdf`, checks explicit/embedded segment
agreement in ECAM and `SegmentConfig`, and routes native driver probes through mapped MCFG
allocations with full-address deduplication. The segment-zero legacy path cannot alias a nonzero
request; the generic BAR/bridge planner and ACPI INTx routing reject nonzero segments until
their topology/provider models are segment-aware. Host tests for this identity and range logic
ran in CI run [39391895780](https://github.com/NeXuS4Dev/NOLX/actions/runs/39391895780), but no
multi-segment QEMU fixture or end-to-end nonzero-segment native binding was exercised. The
nonzero-segment native-binding gate remains unverified, and this change does not complete
general multi-root enumeration or resource assignment.

CI run [39391895780](https://github.com/NeXuS4Dev/NOLX/actions/runs/39391895780) on `82b414c`
passed all 19 jobs. This includes both Rust host-test steps, kernel build and Clippy, `cargo fmt`,
unsafe audit, and QEMU checks for xHCI controller lifecycle/IRQ recovery, e1000 shared-INTx
remask quarantine and peer traffic, and virtio-net MSI-X recovery and peer traffic. The suite
also passed the three isolated storage boots, PCI resource recovery, USB/HID and AML checks.
Locally, seven xHCI parser tests and the 401-assertion conformance check passed; local
`cargo`/`rustc`/QEMU binaries remain unavailable. The QEMU fault hooks prove software handling
of injected readback failures, not physical hardware failure. The shared-vector changes are
therefore verified for the CI scenarios, but this is not general interrupt-resource or M9
completion evidence. General `_PRT`/link `_SRS` programming, native nonzero-segment binding,
missing vector-exhaustion fixtures, USB hub/SuperSpeed/hotplug, Executive/input-object,
remaining AML, power-policy/S3 and final cross-document evidence gates remain open.
**M9 remains incomplete.**


## Audited ACPI link `_SRS` reprogramming checkpoint (2026-10-20)

This supersedes the earlier statements that the `_SRS` selector, effects planner and
byte-write transaction had no production kernel backend or route-publication caller
(PCI-033/034/035). It does not complete M9.

The portable layer first learned what a disabled link looks like. SeaBIOS reports a PIRQ
link whose steering byte has its disable bit set as `_STA 0x9` (present and functioning,
not enabled) with a zero Small IRQ mask or an empty `_CRS` template; the selector
refused all three shapes, and the first CI round of this checkpoint
([39457923262](https://github.com/NeXuS4Dev/NOLX/actions/runs/39457923262) on `f5e6e13`)
showed exactly that refusal on the PC fixture. Current-template parsing now accepts the
disabled encodings (a zero-mask descriptor is patched in place; an empty template
derives the candidate from a single-descriptor `_PRS`, shrinking multi-GSI Extended IRQ
descriptors to the selected interrupt), while `_PRS` parsing stays strict. Selection and
`_SRS` planning require present+functioning rather than the enabled bit — enabling the
link is `_SRS`'s job — and publication keeps the stricter `is_usable` gate, which the
steering write itself satisfies. `plan_link_reprogramming` preflights every
`_PRT`-referenced link under the same structural limits as `discover`, commits
direct-GSI rows and earlier selections before a later link may share a GSI, and accounts
for every evaluated link as either retained or to-be-reprogrammed. Nine new host tests
pin the disabled/empty candidate construction, the 0x9 status, and the retention,
sharing, conflict and refusal rules.

The kernel runs that preflight before route publication on the APIC profile. Current
routes are judged against the real IOAPIC route table (GSIs the initialization pass
actually retained), with ISA choices mapped only through identity-preserving MADT
overrides. Each unavailable link is planned by `plan_link_srs` and applied through a
`ConfigAccess` backend that refuses every field outside the snapshot's verified PIRQ
bytes; permissions name those bytes and exactly the selected route's encoded value, so
the firmware method may choose which link byte to steer but never what to write into
it. The PIRQ snapshot becomes explicitly refreshable for this one transaction: the
postcondition refreshes it, re-evaluates `_CRS`, and requires the selected interrupt
exactly. Routes then publish only from a fresh current-route discovery, never from the
plan. Any refusal revokes every route; there is no Interrupt Line downgrade. If an
earlier link was already reprogrammed when a later one refuses, firmware steering stays
changed but nothing publishes — the same fail-closed outcome a refused current route
produced before.

The two QEMU machine profiles turned out to exercise opposite halves of the policy, and
the fixture now asserts both rather than assuming one. On PC the PIIX links are
steerable: with every PIRQ byte disabled by the `hal-pci-link-srs-test` hook and every
Interrupt Line byte poisoned, the direct/shared/bridged fixtures recover through real
`_SRS` transactions and carry captured e1000 ARP exchanges on the selected GSIs. On Q35
the APIC-mode `_PRT` references GSIA..GSIH devices whose evaluated `_CRS` reports
hardwired GSIs 16..23; the LPC PIRQ bytes only steer PIC-mode delivery there, so the
correct behavior is to retain all eight routes and write nothing, which the retained-link
lines added after run [39461714857](https://github.com/NeXuS4Dev/NOLX/actions/runs/39461714857)
made directly observable. Exact-revision CI
[39462936440](https://github.com/NeXuS4Dev/NOLX/actions/runs/39462936440) on `ee0253b`
passed **all nineteen jobs**, including the new
`scripts/check-network-multi.py --link-srs-only` step, the preceding intx-routing,
multi-adapter, storage, USB, AML, PCI-resource, S5 and boot regressions, fmt/Clippy and
the unsafe audit. PCI-036 is `tested-T1`, PCI-037 is `tested-T2`; the conformance
database has 406 assertions. Local verification in this environment was limited to the
conformance tooling and Python syntax checks, since it has no Rust or QEMU binaries.

**Limits:** segment zero and root bus zero only, ByteAcc/Preserve single-byte PCI-config
fields on the verified PIIX/ICH9 snapshot whitelist, one selected value per link, at most
64 evaluated links, APIC profile only (PIC boots keep the previous current-route
publication and refuse INTx consumers elsewhere). Renumbering MADT overrides, dependent
function resource alternatives, `_SRS` bodies outside the bounded AML subset and boards
whose link steering is not a whitelisted PIRQ byte all refuse rather than guess. This is
QEMU evidence, including deliberately disabled steering bytes, not physical-board
verification. Nonzero-segment binding, general multi-root resources, USB hubs and
endpoint recovery, Executive input bindings, remaining AML providers and device
power/S3 remain open. **M9 is not complete.**


## Documentation alignment (2026-10-21)

The last unchecked final-verification item — "Assertion database, README and roadmap agree
on the final state and cite CI evidence" — is now addressed:

- `ROADMAP.md` (M9 section) runs through the 2026-10-20 audited ACPI link `_SRS` checkpoint
  (CI run [39462936440](https://github.com/NeXuS4Dev/NOLX/actions/runs/39462936440) on
  `ee0253b`, all 19 jobs; PCI-036 `tested-T1`, PCI-037 `tested-T2`, 406 assertions) and states
  M9 remains in progress.
- `README.md` M9 status table cell and continuation now cite the same final state and list the
  remaining open gates below.
- `docs/testing/assertions/` holds 406 assertions with 0 database problems
  (`tools/conformance-report.py --check`), and `docs/testing/CONFORMANCE.md` regenerates
  byte-identical, satisfying the CI `on-disk-format` job. `drv.md` prose was aligned with its
  rows: VIO-011, VIO-012, VIO-014 and VIO-015 are `tested-T2` with on-disk checksum evidence,
  and VIO-013 and VIO-016 are `tested-T1`.
- `M9` remains **in progress**. It changes to **done** only when every gate below has
  executable evidence and all checks pass on the final revision.

Remaining open gates (unchanged from the checkpoints above):

- General `_PRT`/link `_SRS` reprogramming beyond the verified segment-zero PIIX/ICH9 ByteAcc
  profile: nonzero segments, renumbering MADT overrides, dependent function resource
  alternatives, `_SRS` bodies outside the bounded AML subset, and boards whose link steering is
  not a whitelisted PIRQ byte all refuse rather than guess.
- Native nonzero-segment PCI binding: no multi-segment QEMU/native fixture or end-to-end
  binding evidence (PCI-032 `implemented` covers host-tested segment-qualified identity only).
- End-to-end interrupt-vector exhaustion fixtures for xHCI and network sources: **added on
  2026-10-21** (checkpoint below). The xHCI boot fixture and the absent-device fixture pass
  locally; the e1000 and virtio-net phases are wired into their recovery fixtures and compile
  under their feature sets, but this sandbox's QEMU has no user-mode network backend, so their
  first execution is CI. PCI-031 stays `pending-T2` until that run passes.
- USB hubs, SuperSpeed device classes/hotplug and general endpoint recovery; SS HID selection
  remains host-tested only.
- Executive/process-handle, security/namespace, waitable and user-facing input-device
  bindings; PS/2 and input-object paths have no framework recovery contract.
- Remaining AML operation-region providers and operations; general hardware-field AML/PCI
  roots.
- Device power-state/quiesce policy and S3 save/resume; BSP device shutdown (S5) is verified,
  higher-layer/SMP orchestration is not.
- Dedicated boot tests for each new hardware path, including absent-device fallbacks, and the
  remaining allocator-exhaustion fixtures.


## Interrupt-vector exhaustion checkpoint (2026-10-21)

Executable coverage for the last two vector-exhaustion gaps (PCI-031), plus the sandbox build
repair that made running any QEMU fixture possible in this environment.

- **xHCI.** New feature `hal-xhci-exhaust-test` and `kernel/src/hal/xhci/exhaust.rs`. At the
  end of `probe`, the fixture stops the enumerated controller, asserts the checked stop
  returned eight DMA pages and the register window, takes all 128 candidates of the xHCI pool
  for a handle no device can have (`0xfe`), and requires: `start` = `OpFailed`, the bring-up
  log containing `NoVector`, state `Failed`, unchanged DMA/MMIO baselines, and every held
  claim still owned by the synthetic handle. It then recovers, releases, requires a start that
  owns a vector again with the exact baselines restored, stops, removes, and reports the final
  DMA page count as zero. `scripts/check-xhci-lifecycle.py --mode exhaust` runs it on PC and
  Q35 without QMP input. Local Q35 evidence: 128 candidates, `NoVector` refusal, reacquisition,
  final DMA 0. The lifecycle/start-fault/stop-fault/recovery/transfer-fault modes and their
  interrupt-proof counts are unchanged.
- **e1000.** `ioapic::acquire_pci_route` now returns `Result<u8, RouteError>`; only the vector
  claim can produce `RouteError::NoVector`, and the driver maps it to `Failure::NoVector`
  (`last_failure_was_no_vector` exposes the reason the way the storage drivers do). The
  recovery fixture exhausts the shared INTx pool, requires the refusal to be that case with no
  route reference, no DMA/MMIO leak and no stolen claim, then releases and restarts.
- **virtio-net.** The recovery fixture occupies both RX and TX kinds (they share one fallback
  range, so the second occupation keeps only its reserved preferred vector), requires
  `NoVector`, no DMA/MMIO/vector leak, then releases both pools and requires the exact previous
  vector pair to be reclaimed.
- **Absent devices.** `scripts/check-absent-devices.py` boots the default image with no
  storage, USB or network device and requires one refusal line per driver, `blockdev: 0
  requests dispatched, 0 completed`, no framework start line and a completed initialization.
- **Allocator exhaustion.** `kernel/src/mm/exhaust.rs` (`hal-mm-exhaust-test`) takes every free
  frame, proves the allocator refuses the next frame, an order-1 block and a reserved-frame
  free while its own free count is zero, returns every frame with the free count exact, then
  fills the kernel heap to its 16-region cap, proves every 1 MiB payload survived, refuses an
  8 MiB request and requires bytes-in-use to return to where it started.
  `scripts/check-mm-exhaust.py` runs it on PC and Q35. It found two real defects in the heap's
  over-aligned path, both fixed: the region/offset words were written at the returned pointer
  instead of in front of it (so the caller's first write destroyed them and the block could
  never be freed), and the reservation omitted those words (so the caller's last eight bytes
  landed on the next block's header).
- **Sandbox build path.** `scripts/bootstrap-rust.sh` now installs the `[all]` payload extras,
  extracts with `zstd` or the Python module and fails when no compiler appears. The
  `.nolx-deps/stubs` helpers are real `rep movsb`/`rep stosb` routines assembled from
  `kernel_stubs.S` on every build (`scripts/nolx-env.sh`); the previous bare-`ret` stubs made
  LLVM's `memset` for the frame-map fill a no-op, which left the machine with zero usable
  frames. The fallback link is `-static -no-pie`, and `boot/boot.S` enables SSE
  (`CR4.OSFXSR | CR4.OSXMMEXCPT`, CR0.EM/TS clear, MXCSR 0x1F80) before long mode because the
  host target's `core` library is compiled with it; without it the first core-library `movups`
  raised #UD. `make bootcheck` still passes.

CI evidence for this checkpoint (run
[39593547832](https://github.com/NeXuS4Dev/NOLX/actions/runs/39593547832) on revision `10d738a`,
job *Native USB HID interrupts, lifecycle and DMA safety*): the absent-device fixture, the xHCI
vector-exhaustion fixture and the allocator-exhaustion fixture all passed on PC and Q35, so
MM-035 and MM-036 are `tested-T2`. The two network recovery fixtures now run before the
multi-adapter step in their job, because that step was skipping them.

Local verification: the xHCI exhaust fixture on both PC and Q35, the absent-device fixture on
both PC and Q35, the allocator-exhaustion fixture on both PC and Q35, a mixed boot with virtio-blk/AHCI/NVMe/xHCI-keyboard (1221 block requests
dispatched and completed, all three retained reads verified), the untouched boot self-tests,
`cargo test --lib --bins` and `--tests` (832 tests), `cargo fmt --check`, host Clippy,
`make bootcheck`, `make fscheck`, `make usercheck`, the unsafe-block audit and
`tools/conformance-report.py --check` (then 406 assertions, 0 problems). Not executed here: the
e1000/virtio-net fixtures, because this sandbox's portable QEMU has no user-mode network
backend. *(Superseded: run
[39609375684](https://github.com/NeXuS4Dev/NOLX/actions/runs/39609375684) on `e16ee67` executed
both network phases and the USB/input job and promoted PCI-031 to `tested-T2`; the database now
holds 408 assertions with none waiting for a T2 result — see the stack-overflow section at the
end of this file.)*

Open after this checkpoint, unchanged: general `_PRT`/link `_SRS` reprogramming beyond the
verified segment-zero PIIX/ICH9 profile, native nonzero-segment binding, USB hubs/SuperSpeed/
hotplug and general endpoint recovery, Executive/process-handle/security/waitable input
bindings, remaining AML operation-region providers, device power policy and S3. **M9 is not
complete.**


## The seven red CI jobs: kernel-stack overflow in `spawn` (2026-10-21)

Seven jobs were failing on `8036bf7b` and on every revision after it: *Native PCI BAR and
nested bridge resource recovery*, *Multi-controller storage identity and lifecycle*, *ACPI S5,
six-driver shutdown and fail-closed DMA refusal*, *AHCI and NVMe checked failure recovery*,
*Multi-adapter network wire identity and lifecycle*, *Persistent five-driver hardware
lifecycle*, and *Virtio-blk checked failure recovery*. Run
[39406985079](https://github.com/NeXuS4Dev/NOLX/actions/runs/39406985079) on `ab2a8d05` had
zero failures, so the regression arrived with `8036bf7b`.

The fixtures failed **silently**: a boot reached its first block request and then hung, with no
panic, no exception and no output at all — which is why the CI logs looked like a timeout
rather than a fault. The cause was found by bisecting inside the failing boot with QEMU
hardware watchpoints and QMP memory dumps, and it is a stack overflow:

* `spawn_impl` runs on the **calling process's 32 KiB kernel stack**. It calls
  `AddressSpace::create_in_place`, which initialises the space field by field — except for the
  VAD tree, written as `addr_of_mut!((*space).vads).write(VadTree::new(…))`. That argument is
  materialised as a by-value temporary: `SPACE_NODES = 256` descriptors, 0x6800 bytes, about
  26 KiB of stack.
* A hardware watchpoint on the *virtual* address of the `blockdev::BINDINGS` mutex word
  reported the offending writes, both from `memcpy+0x8` called by
  `AddressSpace::create_in_place`: `rdx = 0x6800` bytes copied **upwards from 0x30 bytes above
  the top of slot 0's kernel stack**, i.e. straight into `.bss`.
* QMP dumps of the whole `BINDINGS` object taken before and after the ring-3 window show
  exactly eight bytes changed: the mutex word (`0 -> 1`) and five binding records. Every later
  block request then spins at the `lock cmpxchg` of `blockdev::device` with interrupts masked —
  the fixture's "hang". The ring-3 test itself uses a `tmpfs`, so it never touches the block
  layer: the damage is done by the *spawn*, not by anything the test asks for.
* The `.bss` neighbourhood explains why the failure looked network- and power-shaped as well:
  `virtio_*`, `pci`, `tables` and the e1000 fixture's `REFUSALS`/`MODE` statics sit in the same
  region. The previously recorded `e1000/recovery.rs` `Err(OpFailed)` panic is consistent with
  a corrupted fixture state rather than a route re-acquisition bug.

Fix (this revision):

* `KERNEL_STACK_SIZE` is 128 KiB instead of 32 KiB, so a whole `VadTree` temporary occupies a
  fifth of the stack rather than all of it.
* `KernelStack` is banded by 4 KiB of `NOLXSTAC` sentinels on **both** ends. The band has to be
  a page wide, not a few words: the observed write started 0x30 bytes above the top, which
  steps over any narrow band.
* The sentinels are checked at two points where a mistake is still attributable:
  `spawn_impl` right after `create_in_place` (the caller's stack is the one that was written
  through) and `exit_current` (the last moment the process's own stack is the live one).
* Proof the guard is live, not decoration: rebuilt with the old 32 KiB stack, the same fixture
  that used to hang now stops with
  `PANIC: panicked at kernel/src/exec/user/mod.rs: spawn from process slot 0 overflowed its
  kernel stack`. With 128 KiB and the bands the boot is clean.

Local results on this revision (QEMU 11.1.50 in this sandbox; Rust 1.97.0). The sandbox QEMU is
built without slirp, so fixtures that need `-netdev user` (e1000/virtio-net recovery,
network-multi, the NIC profiles of power-s5/power-quiesce, the mixed profile of storage-multi
and the e1000 leg of driver-lifecycle) cannot run here and stay CI-only:

| Fixture | Result |
| --- | --- |
| `check-virtio-blk-recovery.py` | **PASS**, 4 profiles × 2 unchanged-media boots, 15 durable exchanges each |
| `check-ahci-nvme-recovery.py` | **PASS**, PC and Q35, AHCI and NVMe |
| `check-storage-multi.py` | **PASS**, 9 non-NIC profiles including `virtio-blk-pc-bridge` |
| `check-driver-lifecycle.py` | **PASS**, virtio-blk, AHCI and NVMe three-cycle lifecycles |
| `check-power-s5.py` | **PASS**, PC and Q35 QMP guest shutdown |
| `check-absent-devices.py` | **PASS**, PC and Q35 |

CI **passed**: run
[39609375684](https://github.com/NeXuS4Dev/NOLX/actions/runs/39609375684) on `e16ee67` — all 19
jobs, including the seven that had been red since `8036bf7b`, the two network recovery fixtures
and the USB/input job. That run is also the first execution of the network exhaustion phases on
a revision where the `.bss` statics they rely on are intact, so PCI-031 moved from
`pending-T2` to `tested-T2`: with it, no assertion in the database is waiting for a T2 result
(408 assertions, 402 with a passing automated test, 0 problems).

The open M9 gates at the top of this file still need executable evidence, so **M9 is not
complete.**


## Executive input bindings (2026-10-21)

The input registry is a kernel-side structure: bounded, allocation-free, and reachable only by
kernel code. This checkpoint puts the layer above it in place, without pretending it is a
syscall interface.

**What now exists.** Every device the registry attaches publishes an Executive object of a new
`InputDevice` type at `\Device\InputN`, and that object carries the four things the gate asked
for:

* *Namespace.* The name is one of sixteen literals the binding owns, and the type's create hook
  refuses anything else, so "input device" cannot be claimed by an object the binding did not
  publish. `disconnect` withdraws the binding, so a stale path can never resolve to a retired
  incarnation.
* *Security.* The descriptor names SYSTEM as owner, refuses POSIX user 1001 outright (a deny ACE
  evaluated before the grants), and grants user 1000 `READ | SYNCHRONIZE`. The type's access mask
  contains no `WRITE` bit at all: producers are kernel drivers, and no handle can ever carry the
  right to inject input. There is no integrity label, because an input report is not
  lower-integrity data than the process that reads it — the self test asserts that a
  low-integrity token is still allowed, which keeps the descriptor honest instead of decorative.
* *Process handles.* `spawn` opens one handle per published device into the new process's own
  handle table, under the process token; `exit` closes them exactly as `ExitProcess` would. The
  ring-3 spawn reports the count (`init holds 2 input handle(s) from spawn`), and the object's
  reference counts return to their pre-open values when the handles close.
* *Waitable.* Readiness is per device, auto-reset and interrupt-safe: the publication path sets
  an atomic flag before the record is observable, the first waiter to see it clears it, and a
  draining reader clears it again. Two waiters therefore cannot both act on one event.

**Boot evidence** (`NOLX: input executive selftest`, PC and Q35, QEMU 11.1.50, this sandbox):

```
input: executive object \Device\Input0 published type=InputDevice access=read|synchronize
NOLX: input executive selftest: 2 object(s) under \Device\Input*, \Device\Input0 granted
  read+synchronize, user 1001 refused without a handle, write refused by the type mask,
  wait auto-reset and drain verified, 2 process handle(s) installed and released,
  references 1 -> 1
NOLX: input executive: init holds 2 input handle(s) from spawn
```

The self test is not a marker-only check: it resolves the published name, compares the granted
mask, requires the refused identity to leave an empty table, requires a write open to fail with
`UnsupportedAccess`, counts the type's open hook, drives the readiness state machine through
idle → signaled → consumed → drained, binds and releases a scratch process table, and requires
the object's reference counts to return. On a machine with no input device it asserts that
nothing was published instead. `scripts/check-absent-devices.py` still passes on both machines.

**Still open, and why.** The object, the handle and the waitable exist; a `read(2)`-shaped
syscall and a scheduler-blocking wait do not. The wait here is a ready/not-ready check, not a
blocked thread, and exposing reports to ring 3 belongs to the syscall layer rather than to this
binding. INP-004 and INP-005 are `tested-T2`; the syscall half stays an open M9 item.
