# Assertions: x86-64 HAL bring-up

Source: NOLX M2 hardware-abstraction contract and the Intel 64 architecture's descriptor,
exception-frame, ACPI, APIC/HPET, and 8254/8259 programming rules. These are platform assertions, not POSIX
claims; they share the assertion database so progress is test-backed and generated.

Rows are pipe-delimited: `ID | specification reference | status | test`.
Statuses: `unimplemented`, `implemented`, `tested-T1`, `pending-T2`, `tested-T2`, `tested-T3`.

HAL-001 | x86-64 IDT: a long-mode interrupt gate is 16 bytes and preserves all 64 handler-address bits | tested-T1 | nolx-hal-core idt::tests::gate_layout_is_sixteen_bytes; idt::tests::gate_splits_and_reassembles_a_canonical_handler_address
HAL-002 | x86-64 IDT: the interrupt-gate type, DPL, and IST fields are encoded in their architectural bit positions | tested-T1 | nolx-hal-core idt::tests::gate_encodes_the_interrupt_type_privilege_and_ist
HAL-003 | x86-64 exceptions: error-code vectors are identified so stubs normalize their stack frames | tested-T1 | nolx-hal-core vectors::tests::hardware_error_code_exceptions_are_exactly_the_known_vectors
HAL-004 | 8259 PIC: sixteen IRQ lines map bijectively to vectors 32 through 47 | tested-T1 | nolx-hal-core pic::tests::every_legacy_irq_maps_to_a_distinct_vector_and_back; pic::tests::vectors_outside_the_pic_ranges_are_not_irq_vectors
HAL-005 | 8254 PIT: the programmed divisor is near 100 Hz and converts ticks to a saturating monotonic nanosecond value | tested-T1 | nolx-hal-core time::tests::divisor_is_in_the_pit_16_bit_range_and_near_100_hz; time::tests::monotonic_clock_starts_at_zero_and_tracks_the_programmed_rate; time::tests::conversion_saturates_instead_of_wrapping
HAL-006 | Periodic clock: at least 100 timer interrupts arrive and the kernel reports the nominal 100 Hz rate | tested-T2 | .github/workflows/ci.yml boot-smoke timer marker; scripts/check-hal-pit-fallback.sh verifies the PIT/PIC route (CI run 38086516018, revision dc2d8d1)
HAL-007 | x86-64 #PF: the exception report includes vector, error code, RIP, RSP, SS, CR2, and saved registers, then halts | tested-T2 | scripts/check-hal-faults.sh hal-fault-test-pf (CI run 38086516018, revision dc2d8d1)
HAL-008 | x86-64 #GP: the exception report includes vector, error code, RIP, RSP, SS, and saved registers, then halts | tested-T2 | scripts/check-hal-faults.sh hal-fault-test-gp (CI run 38086516018, revision dc2d8d1)
HAL-009 | BSP local area: GS-relative access reads the initialized processor number zero | tested-T2 | .github/workflows/ci.yml boot-smoke serial marker `gs-cpu=0` (CI run 38086516018, revision dc2d8d1)
HAL-010 | GDT: the available 64-bit TSS descriptor encodes the full base, limit, and access byte | tested-T1 | nolx-hal-core gdt::tests::available_tss_descriptor_encodes_base_limit_and_access_byte
HAL-011 | Local APIC/x2APIC: calibrated local timer, TPR IPL masking, device-vector dispatch, and EOI are integrated | tested-T2 | .github/workflows/ci.yml boot-smoke APIC/calibration markers and 100-tick timer assertion (CI run 38086516018, revision dc2d8d1)
HAL-012 | Wall clock: a hardware real-time source is converted to the NOLX wall-clock epoch and advanced from its monotonic anchor | tested-T2 | .github/workflows/ci.yml boot-smoke serial marker `wall-clock: CMOS RTC UTC epoch=` (CI run 38086516018, revision dc2d8d1)

HAL-013 | x86-64 trap frame: Rust offsets match the register-save order and unconditional long-mode RSP/SS frame | tested-T1 | nolx-hal-core interrupt::tests::interrupt_frame_matches_the_assembly_push_and_cpu_frame_layout
HAL-014 | x86-64 ISR table: assembly exports exactly 256 vector stubs and 256 handler pointers | tested-T1 | scripts/check-boot-layer.sh interrupt-stub count and table-size check
HAL-015 | HAL console: the object-safe formatting and flush contract is usable without depending on a concrete output device | tested-T1 | nolx-hal-core console::tests::hal_console_is_object_safe_for_formatted_output_and_flush
HAL-016 | RTC: binary/BCD and 12/24-hour fields are validated, leap dates decoded, and civil time converted to Unix seconds | tested-T1 | nolx-hal-core rtc::tests::bcd_12_hour_time_decodes_noon_and_midnight; rtc::tests::binary_24_hour_epoch_and_year_pivot_are_correct; rtc::tests::leap_days_and_month_lengths_are_validated; rtc::tests::invalid_bcd_fields_and_update_inhibit_are_rejected; rtc::tests::epoch_conversion_accounts_for_leap_days_and_time_of_day
HAL-017 | Wall clock: monotonic nanoseconds advance a validated Unix-epoch anchor without wrapping | tested-T1 | nolx-hal-core time::tests::realtime_clock_advances_from_its_epoch_anchor_and_saturates
HAL-018 | ACPI MADT: legacy/x2APIC processor entries, local NMI routes, I/O APIC routes, ISA overrides, and I/O APIC NMI sources decode from length-checked records | implemented | crates/nolx-hal-core/src/acpi.rs Madt::parse and acpi::tests::madt_decodes_legacy_x2apic_ioapic_overrides_and_address_override
HAL-019 | APIC arithmetic: x2APIC MSRs, compressed TPR priority classes, timer divisors, and HPET-based counter calibration are checked | implemented | crates/nolx-hal-core/src/apic.rs tests
HAL-020 | HPET clocksource: 32-bit MMIO reads and wrap extension produce monotonic nanoseconds | implemented | kernel/src/hal/hpet.rs counter, maintain_counter_epoch, and monotonic_ns
HAL-021 | I/O APIC routes: GSI windows and ISA overrides are validated, redirections start masked, and NMI sources use MADT polarity/trigger flags | implemented | kernel/src/hal/ioapic.rs init and unmask_legacy_irq
HAL-022 | ACPI physical discovery: a valid firmware root, MADT, HPET counter, and usable local APIC are found during QEMU boot | tested-T2 | .github/workflows/ci.yml boot-smoke ACPI/HPET/APIC markers (CI run 38086516018, revision dc2d8d1)
HAL-023 | Legacy timer fallback: with local APIC support hidden, PIC EOI and PIT IRQ0 continue to deliver 100 timer ticks | tested-T2 | scripts/check-hal-pit-fallback.sh (CI run 38086516018, revision dc2d8d1)
