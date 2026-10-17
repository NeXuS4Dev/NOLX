//! Bounded polled direct-root USB2 boot-HID transport. No hubs or external DMA pointers.
use super::*;
use crate::hal::dma::Dma;
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, Ordering};
use nolx_input_core::events::{Kind, PointerEvent, Source};
use nolx_input_core::objects::{Class, DeviceId, Location};
use nolx_usb_core::device as wire;
const SLOTS: usize = 8;
const PORTS: usize = 16;
static HOSTS: [IrqMutex<Option<Host>>; driver_start::DEVICES] =
    [const { IrqMutex::new(None) }; driver_start::DEVICES];
struct Ring {
    page: Dma,
    next: u16,
    cycle: bool,
}
impl Ring {
    fn from_page(page: Dma) -> Self {
        page.write(
            255 * 16,
            &usb::link_trb(page.address(0), true, true).unwrap(),
        );
        Self {
            page,
            next: 0,
            cycle: true,
        }
    }
    fn put(&mut self, parameter: u64, status: u32, control: u32) -> u64 {
        let offset = u64::from(self.next) * 16;
        let address = self.page.address(offset);
        publish(
            &self.page,
            offset,
            &wire::trb(parameter, status, control, self.cycle),
        );
        self.next += 1;
        if self.next == 255 {
            publish(
                &self.page,
                255 * 16,
                &usb::link_trb(self.page.address(0), true, self.cycle).unwrap(),
            );
            self.next = 0;
            self.cycle = !self.cycle;
        }
        fence(Ordering::Release);
        address
    }
}
// Publish the cycle byte last: hardware may inspect a running ring before its doorbell.
fn publish(page: &Dma, offset: u64, trb: &usb::Trb) {
    page.write(offset, &trb[..12]);
    page.write(offset + 13, &trb[13..]);
    fence(Ordering::Release);
    page.write(offset + 12, &trb[12..13]);
}
struct Device {
    slot: u8,
    port: u8,
    hid: wire::Hid,
    input: Dma,
    output: Dma,
    control: Ring,
    interrupt: Ring,
    data: Dma,
    pending: u64,
    previous: usb::BootReport,
    object: Option<DeviceId>,
}
struct Host {
    regs: Region,
    op: u64,
    runtime: u64,
    doorbell: u64,
    context: u64,
    ports: u8,
    command: Ring,
    event: Dma,
    table: Dma,
    event_index: u16,
    event_cycle: bool,
    devices: [Option<Device>; SLOTS],
    blocked: [bool; PORTS],
    controller: u32,
}
fn write64(regs: &Region, offset: u64, value: u64) -> Result<(), Failure> {
    regs.write32(offset, value as u32)?;
    regs.write32(offset + 4, (value >> 32) as u32)?;
    Ok(())
}
fn dword(page: &Dma, offset: u64, value: u32) {
    page.write(offset, &value.to_le_bytes());
}
impl Host {
    fn next_event(&mut self) -> Result<Option<usb::Trb>, Failure> {
        let offset = u64::from(self.event_index) * 16;
        if self.event.read_array::<1>(offset + 12)[0] & 1 != u8::from(self.event_cycle) {
            return Ok(None);
        }
        fence(Ordering::Acquire);
        let event = self.event.read_array::<16>(offset);
        self.event_index += 1;
        if self.event_index == 256 {
            self.event_index = 0;
            self.event_cycle = !self.event_cycle;
        }
        write64(
            &self.regs,
            self.runtime + 0x18,
            self.event.address(u64::from(self.event_index) * 16) | 8,
        )?;
        Ok(Some(event))
    }
    fn dispatch(&mut self, event: usb::Trb) {
        if usb::trb_type(&event) != usb::TRB_TRANSFER_EVENT {
            return;
        }
        let slot = usb::event_slot(&event);
        let Some(device) = self.devices.iter_mut().flatten().find(|d| d.slot == slot) else {
            return;
        };
        if wire::endpoint(&event) != wire::dci(device.hid.endpoint).unwrap()
            || wire::pointer(&event) != device.pending
        {
            return;
        }
        let length = if device.hid.protocol == 1 { 8 } else { 3 };
        let residual = wire::residual(&event);
        if matches!(usb::completion_code(&event), 1 | 13)
            && residual <= length
            && length - residual == length
        {
            let report = device.data.read_array::<8>(512);
            if device.hid.protocol == 1 {
                if let Some(changes) = usb::report_events(&device.previous, &report) {
                    for key in changes.iter() {
                        if let Some(id) = device.object {
                            let _ = super::super::input::publish(id, Kind::Key(key));
                        }
                    }
                    device.previous = report;
                }
            } else if let Some(id) = device.object {
                let _ = super::super::input::publish(
                    id,
                    Kind::Pointer(PointerEvent {
                        buttons: report[0] & 7,
                        dx: i16::from(report[1] as i8),
                        dy: i16::from(report[2] as i8),
                    }),
                );
            }
        }
        if matches!(usb::completion_code(&event), 1 | 13) {
            device.pending =
                device
                    .interrupt
                    .put(device.data.address(512), length, 1 << 10 | 1 << 5);
            let _ = self.regs.write32(
                self.doorbell + u64::from(slot) * 4,
                u32::from(wire::dci(device.hid.endpoint).unwrap()),
            );
        }
    }
    fn command(&mut self, parameter: u64, kind: u8, slot: u8) -> Result<u8, Failure> {
        let pointer = self
            .command
            .put(parameter, 0, u32::from(kind) << 10 | u32::from(slot) << 24);
        self.regs.write32(self.doorbell, 0)?;
        for _ in 0..POLL_LIMIT {
            if let Some(event) = self.next_event()? {
                if usb::trb_type(&event) == usb::TRB_COMMAND_COMPLETION
                    && wire::pointer(&event) == pointer
                {
                    if kind != 9 && usb::event_slot(&event) != slot {
                        return Err(Failure::Unsupported);
                    }
                    if usb::completion_code(&event) != 1 {
                        return Err(Failure::Completion(usb::completion_code(&event)));
                    }
                    return Ok(usb::event_slot(&event));
                }
                self.dispatch(event);
            }
            core::hint::spin_loop();
        }
        Err(Failure::Timeout("command completion"))
    }
    fn control(
        &mut self,
        device: &mut Device,
        request: [u8; 8],
        length: u16,
    ) -> Result<(), Failure> {
        let input = request[0] & 128 != 0;
        if input {
            device.data.write(0, &[0; 512]);
        }
        device.control.put(
            u64::from_le_bytes(request),
            8,
            2 << 10
                | 1 << 6
                | if length == 0 {
                    0
                } else if input {
                    3 << 16
                } else {
                    2 << 16
                },
        );
        if length != 0 {
            device.control.put(
                device.data.address(0),
                u32::from(length),
                3 << 10 | if input { 1 << 16 } else { 0 },
            );
        }
        let pointer = device.control.put(
            0,
            0,
            4 << 10 | 1 << 5 | if !input || length == 0 { 1 << 16 } else { 0 },
        );
        self.regs
            .write32(self.doorbell + u64::from(device.slot) * 4, 1)?;
        for _ in 0..POLL_LIMIT {
            if let Some(event) = self.next_event()? {
                if usb::trb_type(&event) == usb::TRB_TRANSFER_EVENT
                    && usb::event_slot(&event) == device.slot
                    && wire::endpoint(&event) == 1
                {
                    if wire::pointer(&event) != pointer || usb::completion_code(&event) != 1 {
                        return Err(Failure::Completion(usb::completion_code(&event)));
                    }
                    return Ok(());
                }
                self.dispatch(event);
            }
            core::hint::spin_loop();
        }
        Err(Failure::Timeout("control transfer"))
    }
    fn descriptor(&mut self, device: &mut Device, kind: u8, length: u16) -> Result<(), Failure> {
        self.control(
            device,
            [0x80, 6, 0, kind, 0, 0, length as u8, (length >> 8) as u8],
            length,
        )
    }
    fn attach(&mut self, port: u8, console: &mut dyn HalConsole) -> Result<(), Failure> {
        let index = self
            .devices
            .iter()
            .position(Option::is_none)
            .ok_or(Failure::Unsupported)?;
        let offset = usb::port_register_offset(self.op as u8, port).unwrap();
        let mut portsc = self.regs.read32(offset)?;
        let speed = ((portsc >> 10) & 15) as u8;
        if !(1..=3).contains(&speed) {
            return Err(Failure::Unsupported);
        }
        if portsc & usb::PORTSC_ENABLED == 0 {
            self.regs.write32(offset, usb::portsc_start_reset(portsc))?;
            poll(
                || {
                    portsc = self.regs.read32(offset)?;
                    Ok(portsc & usb::PORTSC_RESET == 0 && portsc & usb::PORTSC_ENABLED != 0)
                },
                "USB2 port reset",
            )?;
        }
        self.regs.write32(offset, usb::portsc_acknowledge(portsc))?;
        // Allocate before exposing pointers. On any later failure, disable the slot before release.
        let [input, output, control, interrupt, data] = allocate_pages::<5>()?;
        let mut device = Device {
            slot: 0,
            port,
            hid: wire::Hid {
                configuration: 0,
                interface: 0,
                protocol: 0,
                endpoint: 0x81,
                packet: 8,
                interval: 1,
            },
            input,
            output,
            control: Ring::from_page(control),
            interrupt: Ring::from_page(interrupt),
            data,
            pending: 0,
            previous: [0; 8],
            object: None,
        };
        match self.command(0, 9, 0) {
            Ok(slot) if slot > 0 && usize::from(slot) <= SLOTS => device.slot = slot,
            Ok(_) => {
                release(device);
                return Err(Failure::Unsupported);
            }
            Err(error) => {
                release(device);
                return Err(error);
            }
        }
        let result = self.configure(&mut device, speed).and_then(|()| {
            let id = super::super::input::register(
                Source::UsbHid,
                if device.hid.protocol == 1 {
                    Class::Keyboard
                } else {
                    Class::Pointer
                },
                Location::Usb {
                    controller: self.controller,
                    port,
                },
                console,
            )
            .map_err(|_| Failure::Unsupported)?;
            device.object = Some(id);
            Ok(())
        });
        match result {
            Ok(()) => {
                let _ = writeln!(
                    console,
                    "usb: HID ready port={port} slot={} protocol={} endpoint={:#x}",
                    device.slot, device.hid.protocol, device.hid.endpoint
                );
                self.devices[index] = Some(device);
                let _ = writeln!(
                    console,
                    "usb: device DMA ownership live pages={}",
                    crate::hal::dma::live_pages()
                );
                Ok(())
            }
            Err(error) => {
                if self.command(0, 10, device.slot).is_ok() {
                    self.table
                        .write(u64::from(device.slot) * 8, &0u64.to_le_bytes());
                    release(device);
                } else {
                    self.devices[index] = Some(device);
                }
                Err(error)
            }
        }
    }
    fn configure(&mut self, d: &mut Device, speed: u8) -> Result<(), Failure> {
        self.table
            .write(u64::from(d.slot) * 8, &d.output.address(0).to_le_bytes());
        dword(&d.input, 4, 3);
        dword(&d.input, self.context, (u32::from(speed) << 20) | (1 << 27));
        dword(&d.input, self.context + 4, u32::from(d.port) << 16);
        let packet = if speed == 3 { 64 } else { 8 };
        dword(
            &d.input,
            self.context * 2 + 4,
            3 << 1 | 4 << 3 | packet << 16,
        );
        d.input.write(
            self.context * 2 + 8,
            &(d.control.page.address(0) | 1).to_le_bytes(),
        );
        dword(&d.input, self.context * 2 + 16, 8);
        self.command(d.input.address(0), 11, d.slot)?;
        self.descriptor(d, 1, 18)?;
        let descriptor = d.data.read_array::<18>(0);
        if descriptor[0] != 18
            || descriptor[1] != 1
            || descriptor[17] != 1
            || ![8, 16, 32, 64].contains(&descriptor[7])
        {
            return Err(Failure::Unsupported);
        }
        if u32::from(descriptor[7]) != packet {
            let ctx = d.output.read_array::<64>(self.context);
            d.input
                .write(self.context * 2, &ctx[..self.context as usize]);
            dword(&d.input, 4, 2);
            let value = u32::from_le_bytes(d.input.read_array::<4>(self.context * 2 + 4));
            dword(
                &d.input,
                self.context * 2 + 4,
                (value & 0xffff) | u32::from(descriptor[7]) << 16,
            );
            self.command(d.input.address(0), 13, d.slot)?;
        }
        self.descriptor(d, 2, 9)?;
        let header = d.data.read_array::<9>(0);
        let length = u16::from_le_bytes([header[2], header[3]]);
        if !(9..=512).contains(&length) {
            return Err(Failure::Unsupported);
        }
        self.descriptor(d, 2, length)?;
        let mut bytes = [0; 512];
        d.data.read(0, &mut bytes[..usize::from(length)]);
        d.hid = wire::hid(&bytes[..usize::from(length)]).ok_or(Failure::Unsupported)?;
        self.control(d, [0, 9, d.hid.configuration, 0, 0, 0, 0, 0], 0)?;
        self.control(d, [0x21, 11, 0, 0, d.hid.interface, 0, 0, 0], 0)?;
        let dci = wire::dci(d.hid.endpoint).unwrap();
        let interval = wire::interval(speed, d.hid.interval).ok_or(Failure::Unsupported)?;
        let slot = d.output.read_array::<64>(0);
        d.input.write(self.context, &slot[..self.context as usize]);
        let value = u32::from_le_bytes(slot[..4].try_into().unwrap());
        dword(
            &d.input,
            self.context,
            (value & !(31 << 27)) | u32::from(dci) << 27,
        );
        dword(&d.input, 4, 1 | 1u32 << dci);
        let ep = self.context * (u64::from(dci) + 1);
        dword(&d.input, ep, u32::from(interval) << 16);
        dword(
            &d.input,
            ep + 4,
            3 << 1 | 7 << 3 | u32::from(d.hid.packet) << 16,
        );
        d.input
            .write(ep + 8, &(d.interrupt.page.address(0) | 1).to_le_bytes());
        dword(
            &d.input,
            ep + 16,
            u32::from(d.hid.packet) | (u32::from(d.hid.packet) << 16),
        );
        self.command(d.input.address(0), 12, d.slot)?;
        d.pending = d.interrupt.put(
            d.data.address(512),
            if d.hid.protocol == 1 { 8 } else { 3 },
            1 << 10 | 1 << 5,
        );
        self.regs
            .write32(self.doorbell + u64::from(d.slot) * 4, u32::from(dci))?;
        Ok(())
    }
    fn scan(&mut self, console: &mut dyn HalConsole) -> Result<(), Failure> {
        for port in 1..=self.ports {
            let connected = self
                .regs
                .read32(usb::port_register_offset(self.op as u8, port).unwrap())?
                & 1
                != 0;
            if !connected {
                self.blocked[usize::from(port - 1)] = false;
                if let Some(index) = self
                    .devices
                    .iter()
                    .position(|d| d.as_ref().is_some_and(|d| d.port == port))
                {
                    let slot = self.devices[index].as_ref().unwrap().slot;
                    let device = self.devices[index].as_mut().unwrap();
                    device.pending = 0;
                    // Input records contain no DMA references. Retire the public producer even
                    // if hardware quiescence later fails; quarantined pages must stay pinned.
                    if let Some(id) = device.object.take() {
                        super::super::input::disconnect(id).expect("USB input producer lifetime");
                    }
                    self.command(0, 10, slot)?;
                    let d = self.devices[index].take().unwrap();
                    self.table.write(u64::from(slot) * 8, &0u64.to_le_bytes());
                    release(d);
                    let _ = writeln!(
                        console,
                        "usb: device DMA ownership live pages={}",
                        crate::hal::dma::live_pages()
                    );
                    let _ = writeln!(
                        console,
                        "usb: disconnected port={port} slot={slot}, DMA released"
                    );
                }
            } else if !self.blocked[usize::from(port - 1)] {
                self.blocked[usize::from(port - 1)] = true;
                if let Err(error) = self.attach(port, console) {
                    let _ = writeln!(
                        console,
                        "usb: port={port} enumeration refused: {error:?}; live DMA pages={}",
                        crate::hal::dma::live_pages()
                    );
                }
            }
        }
        Ok(())
    }
}
fn allocate_pages<const N: usize>() -> Result<[Dma; N], Failure> {
    let mut pages: [Option<Dma>; N] = core::array::from_fn(|_| None);
    for index in 0..N {
        match Dma::new() {
            Some(page) => pages[index] = Some(page),
            None => {
                for page in pages.into_iter().flatten() {
                    // SAFETY: this batch has not been exposed to hardware; allocation is transactional.
                    unsafe {
                        page.release();
                    }
                }
                return Err(Failure::Memory);
            }
        }
    }
    Ok(pages.map(Option::unwrap))
}
fn release(d: Device) {
    // SAFETY: callers either have not installed any device DMA pointer, or observed successful
    // Disable Slot completion and removed the DCBAA pointer, or proved full controller
    // halt/reset and PCI DMA exclusion. No engine can access these pages.
    unsafe {
        d.input.release();
        d.output.release();
        d.control.page.release();
        d.interrupt.page.release();
        d.data.release();
    }
}
pub(super) fn start(
    dev: usize,
    controller: u32,
    regs: Region,
    op: u64,
    ports: u8,
    console: &mut dyn HalConsole,
) -> Result<(), Failure> {
    if ports as usize > PORTS
        || regs.read32(8)? & 0xf800_0000 != 0
        || regs.read32(8)? & 0x03e0_0000 != 0
        || regs.read32(op + 8)? & 1 == 0
    {
        return Err(Failure::Unsupported);
    }
    let context = if regs.read32(0x10)? & 4 != 0 { 64 } else { 32 };
    let runtime = u64::from(regs.read32(0x18)? & !31) + 0x20;
    let doorbell = u64::from(regs.read32(0x14)? & !3);
    // Validate the entire runtime/doorbell ranges before any DMA engine is started.
    regs.read32(runtime + 0x3c)?;
    regs.read32(doorbell + SLOTS as u64 * 4)?;
    let enabled = usb::max_slots(regs.read32(4)?).min(SLOTS as u8);
    if enabled == 0 {
        return Err(Failure::Unsupported);
    }
    let mut guard = HOSTS[dev].lock();
    if guard.is_some() {
        return Err(Failure::Busy);
    }
    let [command, event, table] = allocate_pages::<3>()?;
    *guard = Some(Host {
        regs,
        op,
        runtime,
        doorbell,
        context,
        ports,
        command: Ring::from_page(command),
        event,
        table,
        event_index: 0,
        event_cycle: true,
        devices: core::array::from_fn(|_| None),
        blocked: [false; PORTS],
        controller,
    });
    let host = guard.as_mut().unwrap();
    host.table
        .write(0x100, &host.event.address(0).to_le_bytes());
    dword(&host.table, 0x108, 256);
    host.regs.write32(op + 0x38, u32::from(enabled))?;
    write64(&host.regs, op + 0x30, host.table.address(0))?;
    write64(&host.regs, op + 0x18, host.command.page.address(0) | 1)?;
    host.regs.write32(runtime, 0)?;
    host.regs.write32(runtime + 8, 1)?;
    write64(&host.regs, runtime + 0x10, host.table.address(0x100))?;
    write64(&host.regs, runtime + 0x18, host.event.address(0))?;
    fence(Ordering::Release);
    host.regs.write32(op, usb::USBCMD_RUN)?;
    poll(|| Ok(host.regs.read32(op + 4)? & 1 == 0), "run")?;
    #[cfg(feature = "hal-xhci-start-fault-test")]
    {
        static INJECT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
        if INJECT.swap(false, Ordering::AcqRel) {
            return Err(Failure::Timeout("injected post-run start failure"));
        }
    }
    host.command(0, 23, 0)?; // No-op completion proves command/event DMA before device enumeration.
    let _ = writeln!(
        console,
        "xhci: command/event rings online, context={context}, polled HID"
    );
    host.scan(console)?;
    Ok(())
}
/// Service bounded event batches from the idle consumer, never from an ISR.
pub(super) fn poll_events(dev: usize, console: &mut dyn HalConsole) {
    let mut guard = HOSTS[dev].lock();
    let Some(host) = guard.as_mut() else {
        return;
    };
    for _ in 0..256 {
        match host.next_event() {
            Ok(Some(e)) => host.dispatch(e),
            _ => break,
        }
    }
    if let Err(error) = host.scan(console) {
        let _ = writeln!(console, "usb: scan refused: {error:?}");
    }
}

pub(super) fn active(dev: usize) -> bool {
    HOSTS[dev].lock().is_some()
}
/// End software producers even when hardware cannot be stopped safely.
pub(super) fn retire(dev: usize) {
    if let Some(host) = HOSTS[dev].lock().as_mut() {
        for device in host.devices.iter_mut().flatten() {
            if let Some(id) = device.object.take() {
                let _ = super::super::input::disconnect(id);
            }
            device.pending = 0;
        }
    }
}
/// # Safety
/// Caller proved controller halt/reset and disabled/read back PCI bus mastering while excluding
/// polling and starts. No controller engine can access any host/device page being released.
pub(super) unsafe fn release_host(dev: usize) {
    if let Some(host) = HOSTS[dev].lock().take() {
        for device in host.devices.into_iter().flatten() {
            release(device);
        }
        // SAFETY: caller established DMA exclusion for this uniquely moved controller state.
        unsafe {
            host.command.page.release();
            host.event.release();
            host.table.release();
        }
    }
}
