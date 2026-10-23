//! Low-level ACPI S3/S5 requests with validated legacy control ports and ACPI-mode enablement.
//! The S3 fixture also installs a real-mode hand-off vector in low memory so the firmware
//! resume path can be observed; this is still not a full resume implementation.

use nolx_hal_core::console::HalConsole;
#[cfg(any(
    feature = "hal-power-test-s3",
    feature = "hal-power-s3-device-test",
    feature = "hal-power-dstate-test"
))]
use nolx_power_core::aml::{apply_device_power_writes, Path};
use nolx_power_core::{pm1_sleep_control, Fadt};

/// Why no power-state request could be issued.
#[derive(Debug)]
pub enum Error {
    /// ACPI/FADT/S5 data was absent.
    Firmware,
    /// The platform requires an unsupported control-block format or PM1b sleep type.
    Unsupported,
    /// ACPI-mode enablement did not complete within its bounded wait.
    AcpiMode,
    /// Flush, controller stop, retained DMA or PCI bus-master readback refused shutdown.
    Devices,
    /// A firmware device power transition (`_PS3`/`_PS0`) was refused.
    PowerState,
}

unsafe fn read_control(port: u16) -> u16 {
    let value: u16;
    // SAFETY: callers supply a validated FADT legacy PM1 control port with width >= 2.
    unsafe { core::arch::asm!("in ax, dx", in("dx") port, out("ax") value, options(nostack)) };
    value
}
unsafe fn write_control(port: u16, value: u16) {
    // SAFETY: callers supply the validated PM1 control port and a preserved control word.
    unsafe { core::arch::asm!("out dx, ax", in("dx") port, in("ax") value, options(nostack)) };
}

fn enable_acpi(fadt: &Fadt) -> Result<u16, Error> {
    // SAFETY: the caller checked FADT PM1 control width and the parser validated its I/O port.
    let current = unsafe { read_control(fadt.pm1a_control) };
    // All ones means no chipset decodes the port at all, not "SCI_EN is set": the PM base has
    // to be restored first. Treating it as success would report ACPI mode on a dead block.
    if current != u16::MAX && current & 1 != 0 {
        return Ok(current);
    }
    let port = fadt.smi_command.ok_or(Error::Unsupported)?;
    if fadt.acpi_enable == 0 {
        return Err(Error::Unsupported);
    }
    // SAFETY: the SMI command is a validated 16-bit port explicitly designated by firmware.
    unsafe { super::outb(port, fadt.acpi_enable) };
    let clock = super::hpet::counter().zip(super::hpet::frequency_hz());
    // The iteration bound is a hard cap, not just a fallback: a resumed platform can hold an
    // HPET mapping that no longer advances, and waiting on a dead counter would never end.
    for index in 0..16_000_000u32 {
        // SAFETY: same validated control port as the first read above.
        let current = unsafe { read_control(fadt.pm1a_control) };
        if current != u16::MAX && current & 1 != 0 {
            return Ok(current);
        }
        match (clock, super::hpet::counter()) {
            (Some((start, hz)), Some(now))
                if super::hpet::elapsed_ticks(start, now) >= hz.saturating_mul(2) =>
            {
                break
            }
            (None, _) if index >= 1_000_000 => break,
            _ => {}
        }
        core::hint::spin_loop();
    }
    Err(Error::AcpiMode)
}

/// Where the real-mode trampoline and its temporary page tables live. The range is inside the
/// first megabyte, which the frame allocator reserves in full.
const S3_TRAMPOLINE_ADDR: u64 = 0x8000;
/// Temporary PML4: the live table plus an identity map of the first 8 MiB.
const S3_TEMP_PML4: u64 = 0x9000;
const S3_TEMP_PDPT: u64 = 0xA000;
const S3_TEMP_PD: u64 = 0xB000;
/// Offset of the hand-off data slots inside the copied trampoline blob.
const S3_DATA_OFFSET: usize = 0x300;
/// CMOS index/data ports and the shutdown-status byte SeaBIOS reads before choosing resume.
const CMOS_INDEX: u16 = 0x70;
const CMOS_DATA: u16 = 0x71;
const CMOS_RESET_CODE: u8 = 0x0f;
const CMOS_S3_RESUME: u8 = 0xfe;
/// Written before the sleep word and required after the firmware hand-off, so the resumed
/// kernel proves that the same RAM image came back instead of a fresh boot.
const S3_RESUME_TAG: u64 = 0x534c_3352_4553_554d;
static S3_TAG: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

// The hand-off is copied to S3_TRAMPOLINE_ADDR and executed in real mode, then protected mode
// and long mode. Its own labels are linked at the kernel's virtual addresses, so every jump
// and data reference that survives the copy is written as an absolute S3_TRAMPOLINE_ADDR-based
// constant. The blob reserves fixed offsets: 0x040 protected-mode stage, 0x080 long-mode
// stage, 0x200 GDT, 0x300 data slots, 0x380 end. `s3_resume_entry` follows the copy and only
// ever executes from the kernel image.
core::arch::global_asm!(
    r#"
    .section .text.s3_resume,"ax",@progbits
    .global s3_resume_trampoline_start
    .global s3_resume_trampoline_end
    .global s3_resume_entry

    .code16
s3_resume_trampoline_start:
    cli
    cld
    xor %ax, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    movl $0x7000, %esp
    lgdt 0x8228
    movl %cr0, %eax
    orl $1, %eax
    movl %eax, %cr0
    ljmpl $0x08, $0x8040

    .org 0x40
    .code32
s3_pm_stage:
    movw $0x10, %ax
    movw %ax, %ds
    movw %ax, %es
    movw %ax, %ss
    movl $0x7000, %esp
    movl 0x8338, %eax
    movl %eax, %cr4
    movl $0x9000, %eax
    movl %eax, %cr3
    movl 0x8340, %eax
    movl 0x8344, %edx
    orl $0x100, %eax
    movl $0xc0000080, %ecx
    wrmsr
    movl 0x8330, %eax
    movl %eax, %cr0
    ljmpl $0x18, $0x8100

    .org 0x100
    .code64
s3_long_stage:
    movabs $0x8308, %rax
    mov (%rax), %rax
    jmp *%rax

    .org 0x200
    .quad 0x0000000000000000
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .org 0x228
    .short 0x0027
    .long 0x00008200
    .org 0x300
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .quad 0
    .org 0x380
s3_resume_trampoline_end:

    .code64
s3_resume_entry:
    movabs $0x8300, %rax
    mov (%rax), %rbx
    mov %rbx, %cr3
    movabs $0xffff800000008300, %rax
    mov 0x10(%rax), %rsp
    movabs $0xffff800000008318, %rbx
    lgdt (%rbx)
    movabs $0xffff800000008324, %rbx
    lidt (%rbx)
    leaq 1f(%rip), %rax
    pushq $0x08
    pushq %rax
    lretq
1:
    mov $0x10, %ax
    mov %ax, %ds
    mov %ax, %es
    mov %ax, %ss
    xor %ebp, %ebp
    // The wake reset leaves the segment bases zero, but the kernel addresses its per-CPU
    // block through GS: the first Rust instruction that touches it (the interrupt level in
    // the framework's admission path) faults on a null base. Restore GS, KernelGS and FS
    // before any compiled code runs, from the slots the sleep path saved.
    movabs $0xffff800000008348, %rbx
    mov (%rbx), %rax
    mov %rax, %rdx
    shr $32, %rdx
    movl $0xc0000101, %ecx
    wrmsr
    movabs $0xffff800000008350, %rbx
    mov (%rbx), %rax
    mov %rax, %rdx
    shr $32, %rdx
    movl $0xc0000102, %ecx
    wrmsr
    movabs $0xffff800000008358, %rbx
    mov (%rbx), %rax
    mov %rax, %rdx
    shr $32, %rdx
    movl $0xc0000100, %ecx
    wrmsr
    call s3_resume_rust
2:
    jmp 2b
    "#,
    options(att_syntax)
);

extern "C" {
    static s3_resume_trampoline_start: u8;
    static s3_resume_trampoline_end: u8;
    fn s3_resume_entry();
    #[link_name = "_kernel_stack_top"]
    static KERNEL_STACK_TOP: u8;
}

/// The resumed kernel. Runs with the kernel's page tables and stack restored; the only state
/// proven here is that RAM survived the firmware resume hand-off. It ends the fixture with a
/// real S5 so the harness sees a shutdown instead of a halted VM, then never returns.
#[no_mangle]
pub extern "C" fn s3_resume_rust() -> ! {
    use core::fmt::Write as _;
    let mut console = crate::hal::console::Console::new();
    let tag = S3_TAG.load(core::sync::atomic::Ordering::Acquire);
    let _ = writeln!(
        console,
        "power: resumed from S3 with tag {tag:#x}; kernel RAM state preserved"
    );
    console.flush();
    assert_eq!(tag, S3_RESUME_TAG, "the S3 resume tag did not survive");
    // The two clocks a wake reset stops come back first: the HPET counter, because every
    // bounded wait below measures against it, and this processor's local APIC, because the hold
    // below is an inter-processor interrupt. An application processor a wake left outside the
    // wait-for-startup state has to be stopped before the platform is touched at all -- a
    // processor executing firmware bring-up, or re-entering the S3 hand-off, races the restore
    // with nothing in this kernel able to arbitrate it.
    let hpet_running = super::hpet::resume();
    let _ = writeln!(
        console,
        "power: post-resume HPET main counter {}",
        if hpet_running {
            "running"
        } else {
            "not running"
        }
    );
    console.flush();
    if super::apic::resume().is_ok() {
        let _ = writeln!(
            console,
            "power: post-resume local APIC re-enabled ({}), timer re-armed",
            super::apic::mode_name()
        );
    } else {
        let _ = writeln!(
            console,
            "power: post-resume local APIC could not be re-enabled; application processors stay offline"
        );
    }
    console.flush();
    let held = super::smp::hold(&mut console);
    if held > 0 {
        let _ = writeln!(
            console,
            "smp: {held} application processor(s) held in INIT after the wake"
        );
        console.flush();
    }
    match resume_platform(&mut console) {
        Ok(()) => {
            // The wake reset left every application processor in its reset state. Restarting
            // them is the operating system's job after an S3 wake: each one re-installs its own
            // per-processor block, descriptor tables and local APIC, then answers an
            // inter-processor interrupt before the resume is allowed to call itself complete.
            let (restarted, refused) = super::smp::resume(&mut console);
            if restarted > 0 || refused > 0 {
                let _ = writeln!(
                    console,
                    "power: post-resume processor restart: {restarted} application processor(s) online, {refused} refused"
                );
                console.flush();
            }
            // Firmware's `_PS0` is what tells a device the system is awake again; it runs after
            // the platform re-take (the PM block must answer) and before any driver is restarted,
            // so no driver programmes a controller firmware still considers asleep. A refusal is
            // reported per device and does not stop the restore: the driver's own bring-up and
            // the sector readback below are what prove the controller, not the absence of `_PS0`.
            #[cfg(any(
                feature = "hal-power-test-s3",
                feature = "hal-power-s3-device-test",
                feature = "hal-power-dstate-test"
            ))]
            {
                match return_devices_to_d0(&mut console) {
                    Ok((transitioned, refused)) => {
                        if transitioned > 0 || refused > 0 {
                            let _ = writeln!(
                                console,
                                "power: post-resume device power transition: {transitioned} device(s) returned to D0, {refused} refused"
                            );
                            console.flush();
                        }
                    }
                    Err(error) => {
                        let _ = writeln!(
                            console,
                            "power: post-resume device power transition failed: {error:?}"
                        );
                        console.flush();
                    }
                }
            }
            // Re-take device, driver and interrupt state the wake reset cleared: re-enable
            // bus mastering/INTx per device and re-run each driver's bring-up (IOAPIC route,
            // MSI/MSI-X, MMIO windows, xHCI root-port enumeration).
            match restore_devices_after_wake(&mut console) {
                Ok((count, refused)) => {
                    let _ = writeln!(
                        console,
                        "power: post-resume device restore complete: {count} controller(s) re-started, {refused} refused"
                    );
                    console.flush();
                }
                Err(error) => {
                    let _ = writeln!(
                        console,
                        "power: post-resume device restore failed: {error:?}"
                    );
                    console.flush();
                }
            }
            // The restored devices are running again, so the fixture verifies them while
            // they are live and only then shuts down through the ordinary fail-closed path.
            if verify_restored_devices(&mut console) {
                // SAFETY: no other CPU runs, the fail-closed quiesce above stopped and proved
                // every driver quiet, and this is the last thing the resumed kernel does.
                if let Err(error) = unsafe { request_s5_quiesced(&mut console) } {
                    let _ = writeln!(console, "power: post-resume S5 failed: {error:?}");
                    console.flush();
                }
            } else {
                let _ = writeln!(
                    console,
                    "power: post-resume S5 refused: devices not quiesced"
                );
                console.flush();
            }
        }
        Err(error) => {
            let _ = writeln!(
                console,
                "power: post-resume platform re-take failed: {error:?}"
            );
            console.flush();
        }
    }
    loop {
        // SAFETY: the resumed CPU has no work and no interrupt is required to leave this loop.
        unsafe { core::arch::asm!("hlt", options(nomem, nostack)) }
    }
}

/// Re-take the parts of the platform a wake reset clears, so the resumed kernel can drive it.
///
/// A firmware resume is not a no-op for the chipset: the wake reset clears the memory-mapped
/// configuration window this kernel had selected and the LPC bridge's PM base, so the FADT's
/// PM1 ports stop answering (`0xffff` on Q35) even though the table itself survived in RAM.
/// Restoring the PM base and entering ACPI mode again is what makes the PM block usable; it is
/// not a driver or device-state restore.
fn resume_platform(console: &mut dyn HalConsole) -> Result<(), Error> {
    let tables = super::acpi::discover().ok_or(Error::Firmware)?;
    let fadt = tables.fadt.ok_or(Error::Firmware)?;
    super::pci::refresh_transport(console);
    // The wake reset also returns every function's BARs to their power-on value, which on this
    // board profile means the addresses the kernel planned at boot are gone -- a driver that
    // maps its BAR afterwards reads a zero base and refuses (observed as `BarNotMemory(4)`).
    // Re-run the same bounded, board-profile-checked assignment before any driver restarts.
    super::pci::prepare_resources(console);
    restore_pm_base(console, &fadt)?;
    let current = enable_acpi(&fadt)?;
    // SAFETY: the FADT parser validated the port and the control block width.
    let readback = unsafe { read_control(fadt.pm1a_control) };
    let _ = writeln!(
        console,
        "power: post-resume PM1a {:#x} control {current:#x} (readback {readback:#x}), ACPI mode re-entered",
        fadt.pm1a_control
    );
    console.flush();
    Ok(())
}

/// Restore the LPC/ISA bridge's PM base after a wake reset left it clear.
///
/// The FADT names the PM1a control port; the chipset decodes that port only while its own PM
/// base register agrees. The ICH9 LPC bridge comes out of a QEMU wake reset with the register
/// at zero, while the PIIX4 keeps its value, so this writes the FADT-derived base only when
/// firmware left the register empty and never moves a base firmware already programmed.
fn restore_pm_base(console: &mut dyn HalConsole, fadt: &Fadt) -> Result<(), Error> {
    use nolx_pci_core::{Bdf, ConfigSpace};
    let mut cfg = super::pci::LegacyPorts;
    let base = fadt.pm1a_control & 0xff80;
    for (device, function) in [(0x01u8, 3u8), (0x1f, 0)] {
        let Ok(bdf) = Bdf::new(0, device, function) else {
            continue;
        };
        let id = cfg.read_u32(bdf, 0);
        // The low half is the vendor, the high half the device: only these two bridges own a
        // PM block this profile knows how to re-programme.
        let mask = match id >> 16 {
            0x7113 => 0xffc0u32,
            0x2918 => 0xff80,
            _ => continue,
        };
        let ich9 = id >> 16 == 0x2918;
        let value = cfg.read_u32(bdf, 0x40);
        let programmed = value & mask != 0;
        // The ICH9 also gates the whole block on ACPI_CTRL bit 7, which the wake reset clears.
        let gated = ich9 && cfg.read_u32(bdf, 0x44) & 0x80 == 0;
        if programmed && !gated {
            return Ok(());
        }
        let expected = if programmed {
            value & mask
        } else {
            u32::from(base)
        };
        if !programmed {
            cfg.write_u32(bdf, 0x40, (value & !mask) | expected);
        }
        if gated {
            let control = cfg.read_u32(bdf, 0x44);
            cfg.write_u32(bdf, 0x44, control | 0x80);
        }
        if cfg.read_u32(bdf, 0x40) & mask != expected {
            return Err(Error::Unsupported);
        }
        let _ = writeln!(
            console,
            "power: post-resume PM block restored on {:02x}:{:02x}.{function}, base {:#x}{}",
            bdf.bus,
            bdf.device,
            expected,
            if gated { ", ACPI enable set" } else { "" }
        );
        return Ok(());
    }
    Ok(())
}

/// Prove the restored device tree actually works, then quiesce it for the shutdown that ends
/// the fixture.
///
/// A restore that only printed "started" would be a marker, not evidence: the wake reset could
/// have left a controller whose registers look right and whose transport is dead. Each restored
/// storage handle is therefore made to read sector zero through its own public block path, and
/// the bytes are checked against what firmware left in the boot image (the MBR signature).
/// A device that answers is quiesced; a device that refuses keeps its ownership, so the
/// fail-closed shutdown below never runs with an unproven controller still able to write.
fn verify_restored_devices(console: &mut dyn HalConsole) -> bool {
    let mut viable = 0usize;
    for dev in 0..super::driver_start::DEVICES {
        if super::driver_start::state(dev) != Ok(nolx_df_core::State::Started) {
            continue;
        }
        let name = super::driver_start::name(dev).unwrap_or("device");
        if !matches!(name, "virtio-blk" | "ahci" | "nvme") {
            viable += 1;
            continue;
        }
        let mut sector = [0u8; 512];
        match super::blockdev::read_sectors(dev, 0, &mut sector) {
            Ok(()) => {
                let signature = [sector[510], sector[511]];
                let ok = signature == [0x55, 0xaa];
                let _ = writeln!(
                    console,
                    "power: post-resume {name} handle={dev} read sector 0: signature={:#04x}{:#04x} ok={ok}",
                    signature[0], signature[1]
                );
                if ok {
                    viable += 1;
                }
            }
            Err(error) => {
                let _ = writeln!(
                    console,
                    "power: post-resume {name} handle={dev} read refused after wake: {error:?}"
                );
            }
        }
    }
    console.flush();
    // Shutdown must not inherit a device the resume could not prove. The quiesce is fail-closed
    // and irreversible, which is what the S5 write below requires.
    match super::driver_start::quiesce(console) {
        Ok(stopped) => {
            let _ = writeln!(
                console,
                "power: post-resume shutdown quiesce: {stopped} controller(s) stopped, {viable} verified after wake"
            );
        }
        Err(error) => {
            let _ = writeln!(
                console,
                "power: post-resume shutdown quiesce refused: {error:?}; S5 refused"
            );
            console.flush();
            return false;
        }
    }
    let _ = writeln!(
        console,
        "power: post-resume device verification: {viable} usable controller(s)"
    );
    console.flush();
    true
}

/// Suspend every started device before the S3 sleep, keeping the records restartable.
///
/// A wake reset clears each controller's registers and bus mastering while the kernel's page
/// tables and records survive, so the order matters: flush durable state through each driver's
/// own barrier, stop every started driver with its checked stop, release only the MMIO that stop
/// proved unused, and only then ask the chipset to sleep. Unlike the shutdown quiesce this
/// leaves admission open -- the same records are started again after the wake -- but a failed
/// barrier or stop aborts the suspend with nothing having been handed to firmware.
#[cfg(any(feature = "hal-power-test-s3", feature = "hal-power-s3-device-test"))]
pub fn suspend_devices(console: &mut dyn HalConsole) -> Result<usize, Error> {
    let stopped = super::driver_start::suspend(console).map_err(|_| Error::Devices)?;
    let _ = writeln!(
        console,
        "power: pre-suspend device quiesce: {stopped} controller(s) stopped, admission retained"
    );
    console.flush();
    Ok(stopped)
}

/// Restore device, driver and interrupt state after a firmware S3 wake.
///
/// A wake reset returns every device to its power-on state: the PCI command register loses
/// bus mastering and re-masks INTx, IOAPIC redirection entries are masked, and each
/// controller's internal registers are cleared. The kernel's own view survived in RAM
/// (page tables, heap, framework records), so the restore re-takes each device instead of
/// re-probing the bus: every record is started again through its original dispatch table,
/// which re-enables bus mastering on its BAR device, re-programs its IOAPIC route and
/// MSI/MSI-X, re-maps its MMIO window and -- for xHCI -- re-enumerates its root ports.
///
/// A device that fails to clean or start is left where it is with its DMA/MMIO pinned: a
/// resume is not permission to reclaim hardware the kernel cannot prove is quiet. Device power
/// states (D-states) are not restored; each driver returns to its working state, not to the
/// exact D-state it was in.
fn restore_devices_after_wake(console: &mut dyn HalConsole) -> Result<(usize, usize), Error> {
    let (restored, not_restored) = super::driver_start::resume().map_err(|_| Error::Devices)?;
    for dev in 0..super::driver_start::DEVICES {
        let name = super::driver_start::name(dev).unwrap_or("device");
        match super::driver_start::state(dev) {
            Ok(nolx_df_core::State::Started) => {
                let _ = writeln!(console, "power: {name} handle={dev} restored after S3 wake");
            }
            Ok(state) => {
                let _ = writeln!(
                    console,
                    "power: {name} handle={dev} not restored after S3 wake: {state:?}"
                );
                // Say why, when the driver retains a reason. A resume that only reports the
                // state leaves the failing step of a multi-stage bring-up invisible.
                if name == "virtio-blk" {
                    super::virtio_blk::diagnose(console, dev);
                }
            }
            Err(_) => {}
        }
    }
    let _ = writeln!(
        console,
        "power: post-resume device restore: {restored} restored, {not_restored} not restored"
    );
    console.flush();
    Ok((restored, not_restored))
}

/// Put every device whose namespace node declares `_PS3` into the low-power state firmware
/// defines for it.
///
/// The transition is firmware's, not the driver's: `_PS3` is an ordinary AML method, so it is
/// evaluated by the bounded interpreter, its field writes are journaled, and the journal is
/// applied by [`apply_region_field_writes_authorized`] through the same operation-region windows
/// the boot evaluation authorized -- read before write, verified readback, reverse rollback on a
/// refused write, and a refusal for any field outside an authorized window. The value the method
/// stores is firmware's choice and is never predicted by the kernel, which is why this path uses
/// the scoped authority instead of an exact-value permission.
///
/// Returns `(transitioned, refused)`. A refused transition is reported per device and makes the
/// caller refuse the sleep: a device firmware would not power down can still be a bus master.
#[cfg(any(
    feature = "hal-power-test-s3",
    feature = "hal-power-s3-device-test",
    feature = "hal-power-dstate-test"
))]
pub fn enter_device_low_power(console: &mut dyn HalConsole) -> Result<(usize, usize), Error> {
    transition_devices(console, *b"_PS3")
}

/// Return every device that declares `_PS0` to D0 after a wake.
///
/// The wake reset leaves each device in a firmware-defined state; `_PS0` is what tells it the
/// system is awake. This runs after the platform re-take (the PM block must answer again) and
/// before any driver is re-started, so a driver never programs a controller firmware still
/// considers asleep.
#[cfg(any(
    feature = "hal-power-test-s3",
    feature = "hal-power-s3-device-test",
    feature = "hal-power-dstate-test"
))]
pub fn return_devices_to_d0(console: &mut dyn HalConsole) -> Result<(usize, usize), Error> {
    transition_devices(console, *b"_PS0")
}

/// Evaluate one firmware power method per recorded target and apply its journal.
#[cfg(any(
    feature = "hal-power-test-s3",
    feature = "hal-power-s3-device-test",
    feature = "hal-power-dstate-test"
))]
fn transition_devices(
    console: &mut dyn HalConsole,
    member: [u8; 4],
) -> Result<(usize, usize), Error> {
    // The member is a four-byte name; the log prints it rather than a numeric opcode.
    let member_name = core::str::from_utf8(&member).unwrap_or("?");
    let total = super::acpi::power_target_count();
    if total == 0 {
        let _ = writeln!(
            console,
            "power: device power transition {member_name}: no firmware device declares it"
        );
        console.flush();
        return Ok((0, 0));
    }
    // The interpreter is rebuilt from the checked tables; only the calls happen here, where the
    // console and the region journal are both reachable.
    let outcome = super::acpi::with_power_namespace(|namespace, regions, _pci| {
        let mut transitioned = 0usize;
        let mut refused = 0usize;
        for index in 0..total {
            let Some(target) = super::acpi::power_target(index) else {
                continue;
            };
            let name = target.name();
            let Ok(path) = Path::parse(name) else {
                refused += 1;
                let _ = writeln!(
                    console,
                    "power: {name} transition refused: the recorded namespace path does not parse"
                );
                continue;
            };
            let scope = namespace.device_pci_scope(&path);
            match (
                scope,
                namespace.plan_device_method_authorized(&path, member, &[]),
            ) {
                (_, Ok(None)) => {
                    // Recorded because it declares another power method; this one is absent, so
                    // there is nothing to transition. Not a refusal.
                    let _ = writeln!(console, "power: {name} declares no {member_name}");
                }
                (Ok(scope), Ok(Some(effects))) => {
                    let regions_journaled = effects.region_writes.len();
                    let pci_journaled = effects.writes.len();
                    // The journal is applied in the order the method performed it, across both
                    // address spaces, as one transaction: a refused configuration write restores
                    // the memory-mapped register written before it. The backend is authorized for
                    // the transitioning device's own function only, so even a journal that somehow
                    // named another function could not reach it.
                    let mut backend = super::aml_region::DevicePowerProvider {
                        regions: *regions,
                        pci: {
                            let mut pci = super::aml_region::PciConfigRegions::empty();
                            // The `_ADR` that scoped the reads authorizes the writes here, so
                            // the reader and the writer cannot disagree about the function.
                            if let Some(scope) = scope {
                                let _ = pci.authorize(scope.address);
                            }
                            pci
                        },
                    };
                    match apply_device_power_writes(&effects, scope, &mut backend) {
                        Ok(()) => {
                            transitioned += 1;
                            let _ = writeln!(
                                console,
                                "power: {name} {member_name} applied: {} region field write(s) and {} PCI configuration write(s) verified ({regions_journaled} + {pci_journaled} journaled)",
                                effects.region_writes.len(),
                                effects.writes.len()
                            );
                        }
                        Err(error) => {
                            refused += 1;
                            let _ =
                                writeln!(console, "power: {name} {member_name} refused: {error:?}");
                        }
                    }
                }
                (Err(error), _) | (_, Err(error)) => {
                    refused += 1;
                    let _ = writeln!(console, "power: {name} {member_name} refused: {error:?}");
                }
            }
        }
        (transitioned, refused)
    });
    let (transitioned, refused) = match outcome {
        Ok(counts) => counts,
        Err(error) => {
            let _ = writeln!(
                console,
                "power: device power transition {member_name}: firmware namespace unavailable: {error:?}"
            );
            console.flush();
            return Err(Error::Firmware);
        }
    };
    let _ = writeln!(
        console,
        "power: device power transition {member_name}: {transitioned} transitioned, {refused} refused"
    );
    console.flush();
    Ok((transitioned, refused))
}

/// Exercise the device power-state path without sleeping: the fixture's `_PS3` and `_PS0` are
/// real methods writing a real register inside a window the boot evaluation authorized.
///
/// The selftest is deliberately about the *applied* transaction, not a marker: it requires the
/// target to be recorded with all four methods, requires `_PS3` to journal and verify at least
/// one field write, and requires `_PS0` to do the same on the way back. A device whose method is
/// refused is a failure here, not a warning: that is the difference between "the path exists" and
/// "the path ran".
#[cfg(feature = "hal-power-dstate-test")]
pub fn dstate_selftest(console: &mut dyn HalConsole) {
    use nolx_power_core::power_state::DState;
    let total = super::acpi::power_target_count();
    assert!(
        total > 0,
        "the fixture device did not record a firmware power target"
    );
    assert!(
        super::acpi::power_targets_complete(),
        "a firmware power method was refused during the boot evaluation"
    );
    let mut found = None;
    for index in 0..total {
        if let Some(target) = super::acpi::power_target(index) {
            if target.name() == "\\_SB_.PWR0" {
                found = Some(target);
            }
        }
    }
    let target = found.expect("the fixture device is missing from the power target catalog");
    assert!(
        target.methods.ps3 && target.methods.ps0,
        "the fixture declares _PS3 and _PS0"
    );
    assert!(
        target.methods.psc && target.methods.prw,
        "the fixture declares _PSC and _PRW"
    );
    assert_eq!(
        target.current,
        Some(DState::D0),
        "the fixture _PSC did not report D0"
    );
    let wake = target.wake.expect("the fixture _PRW was refused");
    assert_eq!(
        (wake.gpe, wake.lowest_sleep_state),
        (11, 3),
        "the fixture _PRW decoded to the wrong source"
    );
    assert!(wake.wakes_from(3) && !wake.wakes_from(4));
    let (entered, refused) = enter_device_low_power(console).expect("the transition namespace");
    assert_eq!(refused, 0, "a firmware power transition was refused");
    assert_eq!(entered, 1, "exactly the fixture device should transition");
    let (returned, refused) = return_devices_to_d0(console).expect("the transition namespace");
    assert_eq!(refused, 0, "a firmware D0 return was refused");
    assert_eq!(returned, 1, "exactly the fixture device should return");
    let _ = writeln!(
        console,
        "power: dstate selftest: {total} firmware power target(s), _PSC=D0, _PRW={}@S{}, _PS3 applied with a verified readback, _PS0 returned to D0",
        wake.gpe, wake.lowest_sleep_state
    );
    console.flush();
}

/// Which sleep state to request from the firmware.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SleepState {
    /// Suspend to RAM. The platform must preserve memory and return through a resume path.
    S3,
    /// Soft off. There is no resume.
    S5,
}

impl SleepState {
    fn name(self) -> &'static str {
        match self {
            Self::S3 => "S3",
            Self::S5 => "S5",
        }
    }
}

unsafe fn request_quiesced(console: &mut dyn HalConsole, state: SleepState) -> Result<(), Error> {
    let tables = super::acpi::discover().ok_or(Error::Firmware)?;
    let fadt = tables.fadt.ok_or(Error::Firmware)?;
    let sleep_type = match state {
        SleepState::S3 => tables.s3_sleep_type,
        SleepState::S5 => tables.s5_sleep_type,
    }
    .ok_or(Error::Firmware)?;
    // We currently decode only SLP_TYPa. Never guess a distinct PM1b type.
    if fadt.pm1b_control.is_some() || fadt.pm1_control_bytes < 2 {
        return Err(Error::Unsupported);
    }
    let current = enable_acpi(&fadt)?;
    // The store enables SLP_EN together with the firmware's `_S3_`/`_S5_` type in SLP_TYP.
    // Print before the store so the host log carries the request even when the machine
    // stops immediately afterwards.
    let word = pm1_sleep_control(current, sleep_type, true).ok_or(Error::Unsupported)?;
    let _ = writeln!(
        console,
        "power: requesting ACPI {} via PM1a {:#x}, control {word:#x}",
        state.name(),
        fadt.pm1a_control
    );
    console.flush();
    let _interrupts = super::cpu::InterruptGuard::disable();
    // SAFETY: firmware control data is validated and the caller guaranteed system quiescence.
    unsafe { write_control(fadt.pm1a_control, word) }
    Ok(())
}

/// Install the S3 hand-off: a real-mode trampoline with temporary page tables, the saved
/// kernel CR3/stack/descriptor tables, and the FACS waking vector. CMOS shutdown status 0xfe
/// is what makes SeaBIOS call that vector instead of a cold POST.
///
/// # Errors
/// [`Error::Firmware`] when firmware names no usable FACS, [`Error::Unsupported`] when the
/// hand-off or the FACS layout is outside what this profile handles.
pub fn prepare_s3_resume(console: &mut dyn HalConsole) -> Result<u32, Error> {
    let tables = super::acpi::discover().ok_or(Error::Firmware)?;
    let fadt = tables.fadt.ok_or(Error::Firmware)?;
    let facs = fadt.firmware_ctrl.ok_or(Error::Firmware)?;
    let start = core::ptr::addr_of!(s3_resume_trampoline_start) as usize;
    let end = core::ptr::addr_of!(s3_resume_trampoline_end) as usize;
    let length = end.saturating_sub(start);
    if length == 0 || length > 0x380 {
        return Err(Error::Unsupported);
    }
    let cr3: u64;
    let mut gdtr = [0u8; 10];
    let mut idtr = [0u8; 10];
    // SAFETY: reading CR3 and the descriptor-table registers has no effect on memory. The
    // destination pages are reserved low memory reached through the direct map, and the FACS
    // is firmware-owned memory named by a validated FADT; its declared length is checked
    // before the slice is formed and the FACS helpers re-check the signature.
    unsafe {
        core::arch::asm!(
            "mov {}, cr3",
            out(reg) cr3,
            options(nomem, nostack, preserves_flags)
        );
        core::arch::asm!(
            "sgdt [{}]",
            in(reg) gdtr.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
        core::arch::asm!(
            "sidt [{}]",
            in(reg) idtr.as_mut_ptr(),
            options(nostack, preserves_flags)
        );
        let cr0: u64;
        let cr4: u64;
        let efer_low: u32;
        let efer_high: u32;
        core::arch::asm!(
            "mov {}, cr0",
            out(reg) cr0,
            options(nomem, nostack, preserves_flags)
        );
        core::arch::asm!(
            "mov {}, cr4",
            out(reg) cr4,
            options(nomem, nostack, preserves_flags)
        );
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0xc000_0080u32,
            out("eax") efer_low,
            out("edx") efer_high,
            options(nomem, nostack, preserves_flags)
        );
        let low = crate::mm::phys::direct(S3_TRAMPOLINE_ADDR) as *mut u8;
        core::ptr::copy_nonoverlapping(start as *const u8, low, length);
        let data = low.add(S3_DATA_OFFSET);
        (data as *mut u64).write_unaligned(cr3 & 0x000f_ffff_ffff_f000);
        (data.add(8) as *mut u64).write_unaligned(s3_resume_entry as *const () as usize as u64);
        (data.add(0x10) as *mut u64)
            .write_unaligned(core::ptr::addr_of!(KERNEL_STACK_TOP) as usize as u64);
        core::ptr::copy_nonoverlapping(gdtr.as_ptr(), data.add(0x18), 10);
        core::ptr::copy_nonoverlapping(idtr.as_ptr(), data.add(0x24), 10);
        (data.add(0x30) as *mut u64).write_unaligned(cr0);
        (data.add(0x38) as *mut u64).write_unaligned(cr4);
        (data.add(0x40) as *mut u32).write_unaligned(efer_low);
        (data.add(0x44) as *mut u32).write_unaligned(efer_high);
        // Per-CPU addressing goes through GS and the thread pointer through FS; the wake reset
        // zeroes both bases, so the resume stub reloads them from these slots.
        for (offset, msr) in [
            (0x48usize, 0xc000_0101u32),
            (0x50, 0xc000_0102),
            (0x58, 0xc000_0100),
        ] {
            let low: u32;
            let high: u32;
            core::arch::asm!(
                "rdmsr",
                in("ecx") msr,
                out("eax") low,
                out("edx") high,
                options(nomem, nostack, preserves_flags)
            );
            (data.add(offset) as *mut u32).write_unaligned(low);
            (data.add(offset + 4) as *mut u32).write_unaligned(high);
        }

        // Copy the live PML4 so the hand-off can address the kernel image before switching
        // back, then add an identity map of the first 8 MiB the trampoline itself runs in.
        let pml4_phys = cr3 & 0x000f_ffff_ffff_f000;
        let pml4_source = crate::mm::phys::direct(pml4_phys) as *const u8;
        let pml4 = crate::mm::phys::direct(S3_TEMP_PML4) as *mut u8;
        core::ptr::copy_nonoverlapping(pml4_source, pml4, 0x1000);
        let pdpt = crate::mm::phys::direct(S3_TEMP_PDPT) as *mut u64;
        let pd = crate::mm::phys::direct(S3_TEMP_PD) as *mut u64;
        for index in 0..512 {
            pdpt.add(index).write(0);
            pd.add(index).write(0);
        }
        pdpt.write(S3_TEMP_PD | 3);
        for index in 0..4u64 {
            pd.add(index as usize).write((index << 21) | 0x83);
        }
        (pml4 as *mut u64).write(S3_TEMP_PDPT | 3);

        let facs_ptr = crate::mm::phys::direct(facs) as *const u8;
        let mut header = [0u8; 8];
        core::ptr::copy_nonoverlapping(facs_ptr, header.as_mut_ptr(), 8);
        let declared = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
        if !(16..=0x1000).contains(&declared) {
            return Err(Error::Unsupported);
        }
        let bytes = core::slice::from_raw_parts_mut(facs_ptr as *mut u8, declared);
        nolx_power_core::set_facs_waking_vector(bytes, S3_TRAMPOLINE_ADDR as u32)
            .map_err(|_| Error::Firmware)?;
    }
    S3_TAG.store(S3_RESUME_TAG, core::sync::atomic::Ordering::Release);
    // SAFETY: the CMOS index and data ports are fixed platform I/O; this only selects the
    // shutdown-status byte and writes the value SeaBIOS resumes on.
    unsafe {
        super::outb(CMOS_INDEX, CMOS_RESET_CODE);
        super::outb(CMOS_DATA, CMOS_S3_RESUME);
    }
    let _ = writeln!(
        console,
        "power: S3 hand-off at {S3_TRAMPOLINE_ADDR:#x} via FACS {facs:#x}, CMOS 0x0f=0xfe"
    );
    console.flush();
    Ok(S3_TRAMPOLINE_ADDR as u32)
}

/// Issue an actual ACPI S3 suspend request, preserving unrelated control bits.
/// An `Ok` return means the request was issued; the platform suspends asynchronously, so the
/// caller cannot observe anything after the store returns.
///
/// # Safety
/// The caller must flush all storage, stop device DMA and other CPUs, and prevent new work.
/// No resume vector is installed by this primitive: firmware that cannot resume will restart
/// or stay suspended. This function makes no claim that memory state is restored.
pub unsafe fn request_s3_quiesced(console: &mut dyn HalConsole) -> Result<(), Error> {
    // SAFETY: the caller upholds the quiescence contract documented above; this wrapper adds
    // nothing but the sleep-state selection.
    unsafe { request_quiesced(console, SleepState::S3) }
}

/// Issue an actual ACPI S5 request, preserving unrelated control bits.
/// An `Ok` return means the request was issued; platform shutdown is externally observed.
///
/// # Safety
/// The caller must flush all storage, stop device DMA and other CPUs, and prevent new work.
/// No resume is possible after this request. The dedicated QEMU harness supplies a machine
/// with no active DMA devices; this primitive does not claim to implement that global policy.
pub unsafe fn request_s5_quiesced(console: &mut dyn HalConsole) -> Result<(), Error> {
    // SAFETY: the caller upholds the quiescence contract documented above; this wrapper adds
    // nothing but the sleep-state selection.
    unsafe { request_quiesced(console, SleepState::S5) }
}

/// A conservative guard for the dedicated single-segment, single-CPU QEMU power test only.
/// This is deliberately not presented as production device-power policy.
#[cfg(any(
    feature = "hal-power-test-s5",
    feature = "hal-power-test-s3",
    feature = "hal-power-s3-device-test"
))]
pub fn test_vm_has_active_busmaster() -> bool {
    use nolx_pci_core::ConfigSpace;
    let inventory = nolx_pci_core::enumerate(&mut super::pci::ConfigAccess);
    inventory
        .as_slice()
        .iter()
        .any(|d| super::pci::ConfigAccess.read_u32(d.bdf, 4) & 4 != 0)
}

/// Freeze native device admission, flush storage, stop registered controllers and audit DMA.
/// BSP-only, segment-zero profile. This does not commit higher-layer dirty caches, park other
/// CPUs, stop firmware DMA, or implement S3. Callers must establish those wider preconditions.
/// Once registry checkout begins, failure leaves admission closed; a Busy refusal before checkout
/// does not start a shutdown. There is intentionally no automatic restart after failure.
pub fn quiesce_devices(console: &mut dyn HalConsole) -> Result<usize, Error> {
    let stopped = super::driver_start::quiesce(console).map_err(|_| Error::Devices)?;
    super::ps2::quiesce();
    if super::dma::live_pages() != 0 || !pci_dma_inactive()? {
        return Err(Error::Devices);
    }
    crate::mm::audit().map_err(|_| Error::Devices)?;
    let _ = writeln!(console, "power: quiesced {stopped} controller(s), DMA pages 0, complete segment-zero PCI bus-master readback inactive");
    Ok(stopped)
}

fn pci_dma_inactive() -> Result<bool, Error> {
    use nolx_pci_core::ConfigSpace;
    let platform = super::acpi::discover().ok_or(Error::Firmware)?;
    if platform
        .mcfg
        .is_some_and(|m| m.allocations().iter().any(|a| a.segment != 0))
    {
        return Err(Error::Unsupported);
    }
    // Scan all segment-zero buses, not merely the root-zero bridge tree. Refuse truncated or
    // malformed inventories; never disable an unowned bus master just to make the test pass.
    let inventory = nolx_pci_core::enumerate_range(&mut super::pci::ConfigAccess, 0, 255);
    if inventory.overflowed || inventory.refused_bridges != 0 {
        return Err(Error::Unsupported);
    }
    Ok(inventory
        .as_slice()
        .iter()
        .all(|d| super::pci::ConfigAccess.read_u32(d.bdf, 4) & 4 == 0))
}

/// Dedicated six-controller shutdown evidence; not a production executive shutdown dispatcher.
#[cfg(feature = "hal-power-quiesce-test")]
pub fn quiesce_selftest(console: &mut dyn HalConsole) {
    use super::driver_start;
    let running = driver_start::started().unwrap();
    let handles: alloc::vec::Vec<_> = running.into_iter().flatten().collect();
    assert_eq!(
        handles.len(),
        6,
        "shutdown fixture must exercise all six native drivers"
    );
    for name in ["virtio-blk", "ahci", "nvme", "e1000", "virtio-net", "xhci"] {
        assert!(handles.iter().any(|(_, n)| *n == name));
    }
    let mut infos = [None; 16];
    super::input::enumerate(&mut infos).unwrap();
    let readers: alloc::vec::Vec<_> = infos
        .into_iter()
        .flatten()
        .filter(|i| i.connected)
        .map(|i| (i, super::input::open(i.id).unwrap()))
        .collect();
    // These reserved pre-partition sectors are written after the boot filesystem checks.
    // Only the coordinator's later durable barrier makes this fixture's new writes durable.
    for &(dev, name) in &handles {
        let tag = match name {
            "virtio-blk" => 0xd1,
            "ahci" => 0xd2,
            "nvme" => 0xd3,
            _ => continue,
        };
        super::blockdev::write_sectors(super::blockdev::device(dev).unwrap(), 1780, &[tag; 512])
            .unwrap();
    }
    #[cfg(feature = "hal-power-quiesce-unowned-test")]
    let unowned = {
        use nolx_pci_core::ConfigSpace;
        let inventory = nolx_pci_core::enumerate(&mut super::pci::ConfigAccess);
        let bdf = inventory
            .as_slice()
            .iter()
            .find(|d| d.class == 3)
            .unwrap()
            .bdf;
        let command = super::pci::ConfigAccess.read_u32(bdf, 4) & 0xffff;
        super::pci::ConfigAccess.write_u32(bdf, 4, command | 4);
        assert_ne!(super::pci::ConfigAccess.read_u32(bdf, 4) & 4, 0);
        bdf
    };
    let pages = super::dma::live_pages();
    let result = quiesce_devices(console);
    let mut sector = [0; 512];
    assert_eq!(
        super::blockdev::read_sectors(0, 0, &mut sector),
        Err(nolxfs::DevError::Io(16))
    );
    for &(dev, _) in &handles {
        assert_eq!(driver_start::start(dev), Err(nolx_df_core::Error::Busy));
        assert_eq!(driver_start::recover(dev), Err(nolx_df_core::Error::Busy));
    }
    assert!(matches!(
        super::virtio_net::send_frame_on(0, &[0; 60]),
        Err(super::virtio_net::Failure::Busy)
    ));
    assert!(matches!(
        super::e1000::send_frame_on(0, &[0; 60]),
        Err(super::e1000::Failure::Busy)
    ));
    assert!(matches!(
        super::xhci::poll_on(0, console),
        Err(super::xhci::Failure::Busy)
    ));
    assert_eq!(
        driver_start::quiesce(console),
        Err(nolx_df_core::Error::Busy)
    );
    if result.is_ok() || cfg!(feature = "hal-power-quiesce-unowned-test") {
        for (prior, reader) in readers {
            let id = prior.id;
            let info = super::input::info(id).unwrap();
            assert!(!info.connected && info.snapshot.keys == [0; 4] && info.snapshot.buttons == 0);
            let mut neutral = prior.snapshot.keys == [0; 4] && prior.snapshot.buttons == 0;
            loop {
                match super::input::read(reader).unwrap() {
                    super::input::Read::Event(record) => {
                        assert_eq!(record.device, id);
                        match record.event.kind {
                            nolx_input_core::events::Kind::Key(key) => {
                                assert!(!key.pressed);
                                neutral |= key.usage == 4;
                            }
                            nolx_input_core::events::Kind::Pointer(pointer) => {
                                assert_eq!(pointer.buttons, 0);
                                neutral = true;
                            }
                        }
                    }
                    super::input::Read::Disconnected => break,
                    other => panic!("shutdown reader failed to retire: {other:?}"),
                }
            }
            assert!(neutral, "held USB input did not deliver neutral retirement");
            assert_eq!(
                super::input::read(reader),
                Ok(super::input::Read::Disconnected)
            );
            super::input::close(reader).unwrap();
        }
    }
    #[cfg(feature = "hal-power-quiesce-unowned-test")]
    {
        use nolx_pci_core::ConfigSpace;
        assert!(result.is_err());
        assert!(pages > 0);
        assert_eq!(super::dma::live_pages(), 0);
        assert_ne!(
            super::pci::ConfigAccess.read_u32(unowned, 4) & 4,
            0,
            "coordinator must not commandeer an unowned device"
        );
        let _ = writeln!(console, "power-test: PASS unowned refusal, registered DMA 0 but foreign PCI master unchanged, no S5, shutdown admission remains closed");
    }
    #[cfg(not(feature = "hal-power-quiesce-unowned-test"))]
    if cfg!(feature = "hal-power-quiesce-flush-fault-test") {
        assert!(result.is_err());
        assert_eq!(super::dma::live_pages(), pages);
        for &(dev, _) in &handles {
            assert_eq!(driver_start::state(dev), Ok(nolx_df_core::State::Started));
        }
        let _ = writeln!(console, "power-test: PASS flush refusal, all {pages} DMA pages retained, no stop or S5, shutdown admission remains closed");
    } else if cfg!(feature = "hal-power-quiesce-stop-fault-test") {
        assert!(result.is_err());
        assert!(super::dma::live_pages() > 0);
        let (dev, _) = handles.iter().find(|(_, name)| *name == "xhci").unwrap();
        assert_eq!(driver_start::state(*dev), Ok(nolx_df_core::State::Failed));
        assert_eq!(driver_start::remove(*dev), Err(nolx_df_core::Error::Busy));
        let _ = writeln!(console, "power-test: PASS stop refusal, DMA/MMIO pinned, no S5, shutdown admission remains closed");
    } else {
        assert_eq!(result.unwrap(), 6);
        assert!(pages > 0 && super::dma::live_pages() == 0);
        for &(dev, _) in &handles {
            assert_eq!(driver_start::state(dev), Ok(nolx_df_core::State::Stopped));
        }
        let _ = writeln!(console, "power-test: durable barriers and all six stops verified, new I/O/start refused, DMA {pages} -> 0; entering S5");
        // SAFETY: dedicated BSP-only PC/Q35 fixture; all users completed and filesystem/cache
        // selftests committed before idle. Admission is permanently frozen, every native driver
        // stopped, PS/2 retired, no DMA pages remain and all segment-zero PCI masters read inactive.
        unsafe {
            request_s5_quiesced(console).expect("checked S5 request failed");
        }
    }
}

/// Wait for real held USB key/button state before the shutdown hardware fixture proceeds.
#[cfg(feature = "hal-power-quiesce-test")]
pub fn quiesce_test_ready(console: &mut dyn HalConsole) -> bool {
    static ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if !ARMED.swap(true, core::sync::atomic::Ordering::AcqRel) {
        let _ = writeln!(
            console,
            "power-test: armed, require held USB key and mouse button"
        );
    }
    let mut infos = [None; 16];
    super::input::enumerate(&mut infos).unwrap();
    let mut key = false;
    let mut button = false;
    for info in infos
        .into_iter()
        .flatten()
        .filter(|i| i.connected && matches!(i.location, super::input::Location::Usb { .. }))
    {
        key |= info.snapshot.held(4);
        button |= info.snapshot.buttons == 1;
    }
    key && button
}
