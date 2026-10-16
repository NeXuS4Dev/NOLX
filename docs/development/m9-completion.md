# M9 completion gate

M9 is **not complete**. Passing the current CI suite proves only the assertions it exercises.
This checklist tracks the remaining integration work against ROADMAP.md, without shrinking
M9 to the already implemented boot probes. Check items only with executable evidence.

## PCI and driver plumbing

- [ ] Select ECAM for driver configuration reads/writes when MCFG mappings are usable;
      serialize legacy address/data port pairs and retain legacy fallback on segment zero.
- [ ] Enumerate declared PCI segments/bus ranges without aliasing identical BDFs.
- [ ] Exercise ECAM writes with real driver bring-up and storage round trips on Q35.
- [ ] Program MSI for AHCI and acknowledge its port/controller causes before APIC EOI;
      prove per-request interrupt delivery and on-disk checksums.
- [ ] Complete network-device interrupt routing (82540EM lacks MSI; use its supported
      INTx path or explicitly add an MSI-capable adapter, rather than claiming unsupported MSI).
- [ ] Finish driver lifecycle/resource ownership beyond one-shot framework boot callbacks.

## Input

- [ ] PS/2 controller initialization, keyboard and mouse IRQ paths.
- [ ] xHCI command/event/transfer rings, slots, addressing and device enumeration.
- [ ] HID boot keyboard/mouse reports delivered to bounded input event objects.
- [ ] Inject keyboard/mouse events under QEMU and assert delivered event contents.

## Networking

- [ ] e1000 receive, descriptor recycling and interrupt acknowledgement/delivery.
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

The earlier PCI-014…018 checkpoint covers MCFG validation and read-only ECAM probing;
it does not satisfy the first three transport/integration gates above.
