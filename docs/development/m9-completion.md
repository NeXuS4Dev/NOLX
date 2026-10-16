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
- [ ] Finish driver lifecycle/resource ownership beyond one-shot framework boot callbacks.

## Input

- [x] PS/2 controller initialization, keyboard and mouse IRQ paths.
- [ ] xHCI command/event/transfer rings, slots, addressing and device enumeration.
- [ ] HID boot keyboard/mouse reports delivered to bounded input event objects.
- [x] Inject PS/2 keyboard/mouse events under QEMU and assert delivered event contents.
      USB injection remains part of the unchecked HID/xHCI gates above.

## Networking

- [x] e1000 receive, descriptor recycling and interrupt acknowledgement/delivery.
- [ ] virtio-net negotiated RX/TX queues and interrupt delivery through the driver framework.
- [ ] Verify externally supplied packets and transmitted bytes, not only synthetic host rules.
      Higher network/POSIX socket layers belong to later milestones.

## ACPI and power

- [ ] AML namespace loading/evaluation sufficient for actual firmware `_STA` and `_CRS`
      methods, with explicit unsupported-operation and resource limits.
- [ ] Consume evaluated resources/status for device discovery and routing.
- [ ] Device power-state/quiesce policy and S3 save/resume path.
- [ ] ACPI S5 entry with QEMU observing actual shutdown, not a printed hypothetical word.

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

Follow-up adds an AHCI no-local-APIC polling fallback plus a PIC/PIT disk checksum test;
its evidence is tracked separately as AHCI-013. The old PCI-014…018 checkpoint alone
was read-only; the checked transport gates above are backed by the new tests, not that
older evidence. **Do not mark M9 done from this partial gate list.**
