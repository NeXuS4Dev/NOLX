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
- [ ] Complete network-device interrupt routing (82540EM lacks MSI; use its supported
      INTx path or explicitly add an MSI-capable adapter, rather than claiming unsupported MSI).
- [ ] Assign unconfigured BARs and bridge resource windows without overlaps, rather than
      depending exclusively on firmware-assigned addresses.
- [ ] Bind supported devices beyond the current one-device, segment-zero boot probes.
- [ ] Finish driver lifecycle/resource ownership beyond one-shot framework boot callbacks.

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
- [ ] HID boot keyboard/mouse reports delivered to bounded input event objects.
- [ ] USB disconnect/hotplug cleanup and endpoint/ring ownership; do not leak controller
      resources or leave DMA active after device removal.
- [x] Inject PS/2 keyboard/mouse events under QEMU and assert delivered event contents.
      USB injection remains part of the unchecked HID/xHCI gates above.

## Networking

- [x] e1000 receive, descriptor recycling and interrupt acknowledgement/delivery.
- [x] virtio-net negotiated RX/TX queues and MSI-X interrupt delivery through the driver
      framework; retain DMA/ring state and expose bounded receive/send frame operations.
- [x] Verify externally supplied packets and transmitted bytes, not only synthetic host rules.
      Higher network/POSIX socket layers belong to later milestones.

## ACPI and power

- [ ] AML namespace loading/evaluation sufficient for actual firmware `_STA` and `_CRS`
      methods, with explicit unsupported-operation and resource limits.
- [ ] Consume evaluated resources/status for device discovery and routing.
- [ ] Device power-state/quiesce policy and S3 save/resume path.
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
one-device boot drivers still bind segment zero. e1000 INTx routing currently consumes
firmware-assigned legacy GSIs (IRQ3…15 except IRQ12), not AML PCI link-device routing.
Network reachability verification currently uses QEMU's user-network gateway as a boot
self-test peer; this is not a configurable IP/socket stack. Input FIFO delivery exists,
but HID enumeration and public input-device objects remain open. Driver registries still
use one-shot boot callbacks rather than a persistent system-wide lifecycle.

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
