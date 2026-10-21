//! Bounded direct-root boot-HID transport with polled control and IRQ-scheduled event drain.
//! USB3 EP0 is hardware-checked; single-burst USB3 HID selection has portable tests only.
use super::*;
use crate::hal::dma::Dma;
use crate::sync::IrqMutex;
use core::sync::atomic::{fence, Ordering};
use nolx_input_core::events::{Kind, PointerEvent, Source};
use nolx_input_core::objects::{Class, DeviceId, Location};
use nolx_usb_core::device as wire;
use nolx_usb_core::hub;
use usb::transfer::{self, Completion};
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
#[derive(Clone, Copy, Debug)]
enum Cause {
    Transfer(Completion),
    Disconnect,
    Enumeration,
    Doorbell,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PortState {
    Ready,
    Bound,
    Faulted,
}
#[derive(Clone, Copy, Debug)]
enum Health {
    Active,
    Retiring(Cause),
    Quarantined(Cause),
}
/// What a device is, as far as this driver is concerned.
///
/// A hub is not a HID device with a strange descriptor: it has no report endpoint that carries
/// input, and its ports are enumerated by class requests rather than by reports. Modelling the
/// difference as a value keeps every HID-only path from having to re-check it.
enum Role {
    Hid(wire::Hid),
    /// A hub, its descriptor and the pipe that wakes us when a port changes.
    Hub(HubRole),
}

/// What the driver keeps about a hub after enumeration.
#[derive(Clone, Copy)]
struct HubRole {
    /// The class descriptor: port count, power switching and think time.
    descriptor: hub::Hub,
    /// Device context index of the status-change endpoint.
    dci: u8,
    /// Bytes one status-change transfer carries.
    length: u32,
}

struct Device {
    health: Health,
    #[cfg(feature = "hal-xhci-transfer-fault-test")]
    fault_ring: Option<Ring>,
    slot: u8,
    /// The root port this device's tree hangs off, which is what xHCI calls the root hub port.
    port: u8,
    /// Route string below that root port: zero for a direct-root device.
    route: u32,
    /// Speed ID the controller negotiated for this device.
    speed: u8,
    role: Role,
    input: Dma,
    output: Dma,
    control: Ring,
    interrupt: Ring,
    data: Dma,
    pending: u64,
    previous: usb::BootReport,
    object: Option<DeviceId>,
}
impl Role {
    /// The boot-HID selection, or `None` for a hub.
    fn hid(&self) -> Option<&wire::Hid> {
        match self {
            Role::Hid(hid) => Some(hid),
            Role::Hub(_) => None,
        }
    }

    /// The hub, or `None` for a HID device.
    fn hub(&self) -> Option<&HubRole> {
        match self {
            Role::Hid(_) => None,
            Role::Hub(hub) => Some(hub),
        }
    }

    /// Point a HID role at another endpoint, which the fault fixtures use to ask for an
    /// endpoint the device never declared.
    #[cfg(feature = "hal-xhci-transfer-fault-test")]
    fn set_endpoint(&mut self, endpoint: u8) {
        if let Role::Hid(hid) = self {
            hid.endpoint = endpoint;
        }
    }

    /// The interrupt endpoint this role waits on: its context index and transfer length.
    ///
    /// A HID device's reports carry input; a hub's carry a port bitmap. Both arrive on an
    /// interrupt IN endpoint and both are re-armed the same way, so the difference is data,
    /// not control flow.
    fn pipe(&self) -> (u8, u32) {
        match self {
            Role::Hid(hid) => (
                wire::dci(hid.endpoint).unwrap(),
                if hid.protocol == 1 { 8 } else { 3 },
            ),
            Role::Hub(hub) => (hub.dci, hub.length),
        }
    }
}

/// Where a device is attached: the root port, the route below it, and the hub that reset it.
#[derive(Clone, Copy)]
struct Target {
    /// Root port the device's tree hangs off.
    root_port: u8,
    /// Route string below that root port; zero for a direct-root device.
    route: u32,
    /// Speed ID the controller negotiated for this device.
    speed: u8,
    /// The hub port the device hangs off, or `None` for a direct-root device.
    parent: Option<Parent>,
}

/// The hub a device hangs off, as its slot context and transaction translator need it.
#[derive(Clone, Copy)]
struct Parent {
    /// The hub's slot, which the TT field names.
    slot: u8,
    /// The hub port.
    port: u8,
    /// The hub's think time, from its descriptor.
    think_time: u8,
    /// The hub's own speed: only a high-speed hub translates for a low/full-speed device.
    speed: u8,
}

impl Device {
    /// Whether this device is a hub whose ports are enumerated.
    fn is_hub(&self) -> bool {
        matches!(self.role, Role::Hub(_))
    }

    fn retire(&mut self, cause: Cause) {
        self.pending = 0;
        if let Some(id) = self.object.take() {
            let _ = super::super::input::disconnect(id);
        }
        self.health = Health::Retiring(cause);
    }
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
    blocked: [PortState; PORTS],
    controller: u32,
    reports: usize,
    commands_usable: bool,
    /// A transfer event that arrived for a slot whose device was briefly out of the table.
    ///
    /// A hub class request takes the hub out of `devices` so the transfer machinery can borrow
    /// the controller and the device separately. The hub's own status-change completion can
    /// arrive in that window; recording it here lets the request put the pipe back instead of
    /// leaving it armed nowhere, which would silently turn hotplug into 100 ms polling.
    orphan: Option<(u8, u8)>,
    #[cfg(feature = "hal-xhci-transfer-fault-test")]
    owner: usize,
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
            usb::interrupt::dequeue(self.event.address(0), self.event_index, false)
                .ok_or(Failure::Unsupported)?,
        )?;
        Ok(Some(event))
    }
    fn dispatch(&mut self, event: usb::Trb) {
        if usb::trb_type(&event) != usb::TRB_TRANSFER_EVENT {
            return;
        }
        let slot = usb::event_slot(&event);
        let Some(device) = self.devices.iter_mut().flatten().find(|d| d.slot == slot) else {
            self.orphan = Some((slot, wire::endpoint(&event)));
            return;
        };
        if !matches!(device.health, Health::Active) {
            return;
        }
        let hid = device.role.hid().copied();
        let (dci, length) = device.role.pipe();
        let outcome = transfer::classify(
            transfer::Pending {
                slot,
                endpoint: dci,
                pointer: device.pending,
                length,
            },
            &event,
        );
        match outcome {
            Completion::Unrelated => return,
            Completion::Failed(_) | Completion::InvalidResidual => {
                device.retire(Cause::Transfer(outcome));
                return;
            }
            _ => {}
        }
        if outcome == Completion::Report {
            self.reports = self.reports.saturating_add(1);
        }
        if let Some(hid) = hid.filter(|_| outcome == Completion::Report) {
            let report = device.data.read_array::<8>(512);
            if hid.protocol == 1 {
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
        // A hub's status-change report says only "some port changed": the ports themselves are
        // read by `scan`, which the idle consumer runs right after this. Re-arming here is what
        // makes the next change wake the controller again.
        if matches!(outcome, Completion::Report | Completion::Short) {
            device.pending =
                device
                    .interrupt
                    .put(device.data.address(512), length, 1 << 10 | 1 << 5);
            if self
                .regs
                .write32(self.doorbell + u64::from(slot) * 4, u32::from(dci))
                .is_err()
            {
                device.retire(Cause::Doorbell);
            }
        }
    }
    fn command(&mut self, parameter: u64, kind: u8, slot: u8) -> Result<u8, Failure> {
        if !self.commands_usable {
            return Err(Failure::CommandUnavailable);
        }
        #[cfg(feature = "hal-xhci-transfer-fault-test")]
        if kind == 10 && super::faults::refuse_disable(self.owner) {
            return Err(Failure::Completion(5));
        }
        let result = self.run_command(parameter, kind, slot);
        #[cfg(feature = "hal-xhci-transfer-fault-test")]
        let result = super::faults::command_result(self.owner, kind, result);
        let reusable = match &result {
            Ok(_) => true,
            Err(Failure::Completion(code)) => transfer::command_reusable(*code),
            _ => false,
        };
        self.commands_usable &= reusable;
        result
    }
    fn run_command(&mut self, parameter: u64, kind: u8, slot: u8) -> Result<u8, Failure> {
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
    /// Where a device lives, as its slot context must describe it.
    ///
    /// A direct-root device is described by the root port alone; a device behind a hub adds the
    /// route string, and a low/full-speed one also needs the transaction translator of the
    /// high-speed hub it hangs off.
    fn attach(&mut self, port: u8, console: &mut dyn HalConsole) -> Result<(), Failure> {
        let offset = usb::port_register_offset(self.op as u8, port).unwrap();
        let mut portsc = self.regs.read32(offset)?;
        let speed = ((portsc >> 10) & 15) as u8;
        if wire::initial_ep0_packet(speed).is_none() {
            return Err(Failure::Unsupported);
        }
        if portsc & usb::PORTSC_ENABLED == 0 {
            let reset = if speed == 4 {
                usb::PORTSC_WARM_RESET
            } else {
                usb::PORTSC_RESET
            };
            self.regs.write32(
                offset,
                usb::portsc_reset_for_speed(portsc, speed).ok_or(Failure::Unsupported)?,
            )?;
            poll(
                || {
                    portsc = self.regs.read32(offset)?;
                    Ok(portsc & reset == 0 && portsc & usb::PORTSC_ENABLED != 0)
                },
                "root port reset",
            )?;
        }
        self.regs.write32(offset, usb::portsc_acknowledge(portsc))?;
        self.attach_at(
            Target {
                root_port: port,
                route: 0,
                speed,
                parent: None,
            },
            console,
        )
    }
    /// Address and configure one device, wherever it is attached.
    ///
    /// The parent's reset happened before this call: a root port is reset here only when the
    /// caller is the root scanner, and a hub port is reset by whoever saw the connection. What
    /// this function owns is the slot, the context, and the decision what the device is.
    fn attach_at(&mut self, target: Target, console: &mut dyn HalConsole) -> Result<(), Failure> {
        let index = self
            .devices
            .iter()
            .position(Option::is_none)
            .ok_or(Failure::Unsupported)?;
        // Allocate before exposing pointers. On any later failure, disable the slot before release.
        let [input, output, control, interrupt, data] = allocate_pages::<5>()?;
        let mut device = Device {
            health: Health::Active,
            #[cfg(feature = "hal-xhci-transfer-fault-test")]
            fault_ring: None,
            slot: 0,
            port: target.root_port,
            route: target.route,
            speed: target.speed,
            role: Role::Hid(wire::Hid {
                configuration: 0,
                interface: 0,
                protocol: 0,
                endpoint: 0x81,
                packet: 8,
                payload: 8,
                interval: 1,
            }),
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
        let result = self
            .configure(&mut device, target, console)
            .and_then(|()| self.publish(&mut device, console));
        match result {
            Ok(()) => {
                if let Some(hid) = device.role.hid() {
                    let _ = writeln!(
                        console,
                        "usb: HID ready root-port={} route={:#x} slot={} protocol={} endpoint={:#x}",
                        device.port, device.route, device.slot, hid.protocol, hid.endpoint
                    );
                } else if let Some(hub) = device.role.hub() {
                    let _ = writeln!(
                        console,
                        "usb: hub ready root-port={} route={:#x} slot={} ports={} \
                         per-port-power={} think-time={} power-on-delay={}ms",
                        device.port,
                        device.route,
                        device.slot,
                        hub.descriptor.ports,
                        hub.descriptor.per_port_power_switching(),
                        hub.descriptor.tt_think_time(),
                        u16::from(hub.descriptor.power_on_delay) * 2
                    );
                }
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
                    device.retire(Cause::Enumeration);
                    device.health = Health::Quarantined(Cause::Enumeration);
                    self.devices[index] = Some(device);
                }
                Err(error)
            }
        }
    }

    /// Publish the device's Executive/input object, if it is an input device.
    ///
    /// A hub is not an input producer: it has no reports to publish, and creating an
    /// `InputDevice` object for it would let a program open a device that never yields a record.
    fn publish(
        &mut self,
        device: &mut Device,
        console: &mut dyn HalConsole,
    ) -> Result<(), Failure> {
        let Some(hid) = device.role.hid().copied() else {
            return Ok(());
        };
        let id = super::super::input::register(
            Source::UsbHid,
            if hid.protocol == 1 {
                Class::Keyboard
            } else {
                Class::Pointer
            },
            Location::Usb {
                controller: self.controller,
                port: device.port,
                route: device.route,
            },
            console,
        )
        .map_err(|_| Failure::Unsupported)?;
        device.object = Some(id);
        Ok(())
    }
    fn configure(
        &mut self,
        d: &mut Device,
        target: Target,
        console: &mut dyn HalConsole,
    ) -> Result<(), Failure> {
        let speed = target.speed;
        self.table
            .write(u64::from(d.slot) * 8, &d.output.address(0).to_le_bytes());
        dword(&d.input, 4, 3);
        // Slot context dword 0: the route string, the speed, whether this is a hub, and how many
        // contexts follow. A device behind a hub must carry its route here or the controller
        // addresses the hub's port instead of the device.
        dword(
            &d.input,
            self.context,
            hub::slot_context0(target.route, speed, 1, None).ok_or(Failure::Unsupported)?,
        );
        dword(
            &d.input,
            self.context + 4,
            hub::slot_context1(target.root_port, None).ok_or(Failure::Unsupported)?,
        );
        // A low/full-speed device behind a high-speed hub is addressed through that hub's
        // transaction translator; a direct-root device and a high-speed device leave this zero.
        let tt = target.parent.and_then(|parent| {
            (parent.speed == 3 && matches!(speed, 1 | 2)).then_some(hub::Tt {
                hub_slot: parent.slot,
                port: parent.port,
                think_time: parent.think_time,
            })
        });
        dword(&d.input, self.context + 8, hub::slot_context2(tt));
        let packet = u32::from(wire::initial_ep0_packet(speed).ok_or(Failure::Unsupported)?);
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
            || (speed == 4
                && !(0x0300..=0x0320).contains(&u16::from_le_bytes([descriptor[2], descriptor[3]])))
        {
            return Err(Failure::Unsupported);
        }
        let declared_packet =
            u32::from(wire::ep0_packet(speed, descriptor[7]).ok_or(Failure::Unsupported)?);
        if declared_packet != packet {
            let ctx = d.output.read_array::<64>(self.context);
            d.input
                .write(self.context * 2, &ctx[..self.context as usize]);
            dword(&d.input, 4, 2);
            let value = u32::from_le_bytes(d.input.read_array::<4>(self.context * 2 + 4));
            dword(
                &d.input,
                self.context * 2 + 4,
                (value & 0xffff) | declared_packet << 16,
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
        let _ = writeln!(
            console,
            "usb: descriptors root-port={} route={:#x} speed={speed} ep0={declared_packet} \
             config-bytes={length}",
            d.port, d.route
        );
        if hub::is_hub(&descriptor) {
            return self.configure_hub(d, &bytes[..usize::from(length)], speed, console);
        }
        let hid =
            wire::hid_at_speed(&bytes[..usize::from(length)], speed).ok_or(Failure::Unsupported)?;
        d.role = Role::Hid(hid);
        self.control(d, [0, 9, hid.configuration, 0, 0, 0, 0, 0], 0)?;
        self.control(d, [0x21, 11, 0, 0, hid.interface, 0, 0, 0], 0)?;
        let dci = wire::dci(hid.endpoint).unwrap();
        let interval = wire::interval(speed, hid.interval).ok_or(Failure::Unsupported)?;
        self.interrupt_endpoint(d, dci, interval, hid.packet, hid.payload)
    }

    /// Program one interrupt IN endpoint, configure it, and arm its first transfer.
    ///
    /// A HID device and a hub differ only in what the bytes mean, so both use this path: the
    /// endpoint context carries the ring, the packet size and the payload capacity, and the
    /// doorbell makes the controller poll it.
    fn interrupt_endpoint(
        &mut self,
        d: &mut Device,
        dci: u8,
        interval: u8,
        packet: u16,
        payload: u16,
    ) -> Result<(), Failure> {
        let length = u32::from(payload);
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
        dword(&d.input, ep + 4, 3 << 1 | 7 << 3 | u32::from(packet) << 16);
        d.input
            .write(ep + 8, &(d.interrupt.page.address(0) | 1).to_le_bytes());
        dword(
            &d.input,
            ep + 16,
            u32::from(packet) | u32::from(payload) << 16,
        );
        self.command(d.input.address(0), 12, d.slot)?;
        d.pending = d
            .interrupt
            .put(d.data.address(512), length, 1 << 10 | 1 << 5);
        self.regs
            .write32(self.doorbell + u64::from(d.slot) * 4, u32::from(dci))?;
        Ok(())
    }
    /// Configure a hub: select its configuration, read the class descriptor, and arm the
    /// status-change endpoint.
    ///
    /// Arming that endpoint is what makes hotplug work at all: a device plugged into a hub
    /// raises nothing the controller can see, so a driver that only looked at ports when some
    /// other transfer interrupted it would never notice the device. The endpoint says *that* a
    /// port changed; the port requests in `cascade` say *what* changed.
    fn configure_hub(
        &mut self,
        d: &mut Device,
        configuration: &[u8],
        speed: u8,
        console: &mut dyn HalConsole,
    ) -> Result<(), Failure> {
        let value = *configuration.get(5).ok_or(Failure::Unsupported)?;
        if value == 0 {
            return Err(Failure::Unsupported);
        }
        // The hub's own configuration value comes from its configuration descriptor.
        self.control(d, [0, 9, value, 0, 0, 0, 0, 0], 0)?;
        // The hub descriptor is a class descriptor on the device: type 0x29, nine bytes. A
        // standard-typed request for it stalls, because 0x29 only exists in the class table.
        self.control(d, hub::get_hub_descriptor(9), 9)?;
        let raw = d.data.read_array::<9>(0);
        let descriptor = hub::descriptor(&raw).ok_or(Failure::Unsupported)?;
        let _ = writeln!(
            console,
            "usb: hub descriptors root-port={} route={:#x} ports={} removable={:#06x} \
             characteristics={:#06x}",
            d.port,
            d.route,
            descriptor.ports,
            u16::from_le_bytes(descriptor.removable),
            descriptor.characteristics
        );
        let pipe = hub::status_pipe(configuration).ok_or(Failure::Unsupported)?;
        let dci = wire::dci(pipe.endpoint).ok_or(Failure::Unsupported)?;
        let interval = wire::interval(speed, pipe.interval).ok_or(Failure::Unsupported)?;
        let length = hub::status_length(descriptor.ports, pipe.packet);
        d.role = Role::Hub(HubRole {
            descriptor,
            dci,
            length: u32::from(length),
        });
        self.interrupt_endpoint(d, dci, interval, pipe.packet, length)?;
        let _ = writeln!(
            console,
            "usb: hub status endpoint={:#04x} dci={} packet={} length={} interval={}",
            pipe.endpoint, dci, pipe.packet, length, pipe.interval
        );
        Ok(())
    }

    fn cleanup(&mut self, index: usize, console: &mut dyn HalConsole) -> Result<(), Failure> {
        let d = self.devices[index].as_ref().ok_or(Failure::Unsupported)?;
        let cause = match d.health {
            Health::Retiring(c) | Health::Quarantined(c) => c,
            Health::Active => return Err(Failure::Busy),
        };
        let (slot, port, route) = (d.slot, d.port, d.route);
        if let Cause::Transfer(outcome) = cause {
            let _ = writeln!(console, "usb: transfer fault port={port} slot={slot} outcome={outcome:?}; input retired, no rearm");
        }
        if let Err(error) = self.command(0, 10, slot) {
            self.devices[index].as_mut().unwrap().health = Health::Quarantined(cause);
            let _ = writeln!(console, "usb: slot quarantine port={port} slot={slot} cause={cause:?} cleanup={error:?}; DMA retained, explicit repair required");
            return Err(error);
        }
        let d = self.devices[index].take().unwrap();
        self.table.write(u64::from(slot) * 8, &0u64.to_le_bytes());
        release(d);
        self.blocked[usize::from(port - 1)] = if matches!(cause, Cause::Disconnect) {
            PortState::Ready
        } else {
            PortState::Faulted
        };
        let _ = writeln!(
            console,
            "usb: device DMA ownership live pages={}",
            crate::hal::dma::live_pages()
        );
        let _ = writeln!(
            console,
            "usb: disconnected port={port} route={route:#x} slot={slot}, DMA released"
        );
        Ok(())
    }
    fn repair(&mut self, port: u8, console: &mut dyn HalConsole) -> Result<(), Failure> {
        if port == 0 || port > self.ports {
            return Err(Failure::Unsupported);
        }
        if !self.commands_usable {
            return Err(Failure::CommandUnavailable);
        }
        if let Some(index) = self
            .devices
            .iter()
            .position(|d| d.as_ref().is_some_and(|d| d.port == port))
        {
            if !matches!(
                self.devices[index].as_ref().unwrap().health,
                Health::Quarantined(_)
            ) {
                return Err(Failure::Busy);
            }
            self.cleanup(index, console)?;
        } else if self.blocked[usize::from(port - 1)] != PortState::Faulted {
            return Err(Failure::Busy);
        }
        if self
            .regs
            .read32(usb::port_register_offset(self.op as u8, port).unwrap())?
            & 1
            != 0
        {
            self.blocked[usize::from(port - 1)] = PortState::Bound;
            let result = self.attach(port, console);
            if result.is_err() {
                self.blocked[usize::from(port - 1)] = PortState::Faulted;
            }
            result
        } else {
            self.blocked[usize::from(port - 1)] = PortState::Ready;
            Ok(())
        }
    }
    /// One class request to a hub port.
    ///
    /// The hub is taken out of the table for the duration: `control` needs `&mut self` for the
    /// event ring, and a hub has no armed endpoint, so no completion for it can be pending.
    fn hub_request(
        &mut self,
        index: usize,
        request: [u8; 8],
        length: u16,
    ) -> Result<[u8; 4], Failure> {
        let mut device = self.devices[index].take().ok_or(Failure::Unsupported)?;
        let (slot, dci) = (device.slot, device.role.pipe().0);
        self.orphan = None;
        let result = match self.control(&mut device, request, length) {
            Ok(()) => Ok(device.data.read_array::<4>(0)),
            Err(error) => Err(error),
        };
        let swallowed = self
            .orphan
            .take()
            .is_some_and(|(s, e)| s == slot && e == dci);
        self.devices[index] = Some(device);
        if swallowed {
            // A status-change completion was consumed while the hub was out of the table. Its
            // transfer is gone, so put a fresh one on the ring; otherwise the endpoint would
            // never complete again and hotplug would depend on the idle backstop alone.
            self.rearm(index);
        }
        result
    }

    /// Arm the device's interrupt endpoint again, if it is still active.
    fn rearm(&mut self, index: usize) {
        let Some(mut device) = self.devices[index].take() else {
            return;
        };
        if matches!(device.health, Health::Active) {
            let (dci, length) = device.role.pipe();
            device.pending =
                device
                    .interrupt
                    .put(device.data.address(512), length, 1 << 10 | 1 << 5);
            let doorbell = self.doorbell + u64::from(device.slot) * 4;
            if self.regs.write32(doorbell, u32::from(dci)).is_err() {
                device.retire(Cause::Doorbell);
            }
        }
        self.devices[index] = Some(device);
    }

    /// Poll one hub's ports: acknowledge changes, and attach or retire what the ports report.
    ///
    /// The hub's status-change endpoint wakes the controller, and `scan` then reads every
    /// port: the endpoint says *that* something changed, the class requests say *what*. Reading
    /// all ports is deliberate, because a status bitmap can name a port whose change bits were
    /// already acknowledged by an earlier pass.
    fn cascade(&mut self, index: usize, console: &mut dyn HalConsole) {
        let Some(device) = self.devices[index].as_ref() else {
            return;
        };
        let Some(descriptor) = device.role.hub().map(|hub| hub.descriptor) else {
            return;
        };
        if !matches!(device.health, Health::Active) {
            return;
        }
        let (root_port, route, slot, hub_speed) =
            (device.port, device.route, device.slot, device.speed);
        let think_time = descriptor.tt_think_time();
        for port in 1..=descriptor.ports {
            let Some(request) = hub::get_port_status(port) else {
                continue;
            };
            let Ok(raw) = self.hub_request(index, request, 4) else {
                // One refused request must not stop the other ports from being serviced.
                continue;
            };
            let mut status = hub::PortStatus::from_bytes(raw);
            if let Some(changes) = hub::port_changes(status.change) {
                for feature in changes {
                    if feature == 0 {
                        break;
                    }
                    if let Some(request) = hub::clear_port_feature(port, feature) {
                        let _ = self.hub_request(index, request, 0);
                    }
                }
            }
            let Some(child_route) = hub::route_push(route, port) else {
                continue;
            };
            let present = self
                .devices
                .iter()
                .flatten()
                .any(|d| d.port == root_port && d.route == child_route);
            if !status.connected() {
                if present {
                    let _ = writeln!(
                        console,
                        "usb: hub port disconnected root-port={root_port} route={child_route:#x}"
                    );
                    self.retire_subtree(root_port, child_route);
                }
                continue;
            }
            if present {
                continue;
            }
            if !self.devices.iter().any(Option::is_none) {
                // No slot for another device. Report once per port, not once per tick.
                continue;
            }
            // Power the port when the hub switches power per port and has not done so yet.
            if !status.powered() && descriptor.per_port_power_switching() {
                if let Some(request) = hub::set_port_feature(port, hub::FEATURE_PORT_POWER) {
                    if self.hub_request(index, request, 0).is_err() {
                        continue;
                    }
                }
                let powered = poll(
                    || {
                        let Some(request) = hub::get_port_status(port) else {
                            return Ok(false);
                        };
                        let raw = self.hub_request(index, request, 4)?;
                        Ok(hub::PortStatus::from_bytes(raw).powered())
                    },
                    "hub port power",
                );
                if powered.is_err() {
                    let _ = writeln!(
                        console,
                        "usb: hub port={port} route={child_route:#x} never reported power"
                    );
                    continue;
                }
            }
            // Reset the port: the device behind a hub is only addressable once its hub port has
            // been reset, and the reset is what gives the port its speed.
            if let Some(request) = hub::set_port_feature(port, hub::FEATURE_PORT_RESET) {
                if self.hub_request(index, request, 0).is_err() {
                    continue;
                }
            }
            let done = poll(
                || {
                    let Some(request) = hub::get_port_status(port) else {
                        return Ok(false);
                    };
                    let raw = self.hub_request(index, request, 4)?;
                    let status = hub::PortStatus::from_bytes(raw);
                    Ok(status.change & hub::C_PORT_RESET != 0 || status.enabled())
                },
                "hub port reset",
            );
            if done.is_err() {
                let _ = writeln!(
                    console,
                    "usb: hub port={port} route={child_route:#x} reset timed out"
                );
                continue;
            }
            if let Some(request) = hub::get_port_status(port) {
                if let Ok(raw) = self.hub_request(index, request, 4) {
                    status = hub::PortStatus::from_bytes(raw);
                }
            }
            for feature in [hub::FEATURE_C_PORT_RESET, hub::FEATURE_C_PORT_ENABLE] {
                if let Some(request) = hub::clear_port_feature(port, feature) {
                    let _ = self.hub_request(index, request, 0);
                }
            }
            let Some(speed) = status.speed() else {
                let _ = writeln!(
                    console,
                    "usb: hub port={port} route={child_route:#x} reports contradictory speed bits"
                );
                continue;
            };
            let target = Target {
                root_port,
                route: child_route,
                speed,
                parent: Some(Parent {
                    slot,
                    port,
                    think_time,
                    speed: hub_speed,
                }),
            };
            if let Err(error) = self.attach_at(target, console) {
                let _ = writeln!(
                    console,
                    "usb: route={child_route:#x} enumeration behind hub refused: {error:?}; \
                     live DMA pages={}",
                    crate::hal::dma::live_pages()
                );
            }
        }
    }

    /// Mark every device at or below `route` on `root_port` for removal.
    fn retire_subtree(&mut self, root_port: u8, route: u32) {
        for device in self.devices.iter_mut().flatten() {
            if device.port == root_port
                && hub::route_under(device.route, route)
                && matches!(device.health, Health::Active)
            {
                device.retire(Cause::Disconnect);
            }
        }
    }

    /// Retire devices whose attachment point is gone: a root port that disconnected, or a hub
    /// that is not active any more.
    fn retire_detached(&mut self) {
        for index in 0..SLOTS {
            let Some(device) = self.devices[index].as_ref() else {
                continue;
            };
            if !matches!(device.health, Health::Active) || device.route == 0 {
                continue;
            }
            let (root_port, route) = (device.port, device.route);
            let parent_route = hub::route_parent(route);
            let parent_ok = parent_route.is_some_and(|parent_route| {
                self.devices.iter().flatten().any(|parent| {
                    parent.port == root_port
                        && parent.route == parent_route
                        && matches!(parent.health, Health::Active)
                })
            });
            if !parent_ok {
                self.devices[index]
                    .as_mut()
                    .unwrap()
                    .retire(Cause::Disconnect);
            }
        }
    }

    /// Disable and release every retiring device, leaves first.
    ///
    /// A hub's children are separate slots, so the order matters only for the log and for the
    /// DMA counter: releasing a hub before its children would still be correct, because each
    /// child's slot is its own. Doing it leaf-first keeps the counter monotonic in the log.
    fn sweep(&mut self, console: &mut dyn HalConsole) {
        for _ in 0..SLOTS {
            let next = (0..SLOTS).find(|&index| {
                let Some(device) = self.devices[index].as_ref() else {
                    return false;
                };
                // Only a *retiring* device is swept. A quarantined one is pinned on purpose:
                // its Disable Slot was never proven, so retrying here would release it behind
                // the repair path's back and let a fault fixture (or a real stuck device)
                // pretend the cleanup succeeded.
                if !matches!(device.health, Health::Retiring(_)) {
                    return false;
                }
                // A device that still has a retiring child *in this table* is not a leaf: that
                // child has not been released yet, and a hub must not be torn down while a slot
                // below it is still being disabled.
                !self
                    .devices
                    .iter()
                    .flatten()
                    .enumerate()
                    .any(|(other, child)| {
                        other != index
                            && child.route != device.route
                            && child.port == device.port
                            && matches!(child.health, Health::Retiring(_))
                            && hub::route_under(child.route, device.route)
                    })
            });
            let Some(index) = next else {
                break;
            };
            let _ = self.cleanup(index, console);
        }
    }

    fn scan(&mut self, console: &mut dyn HalConsole) -> Result<(), Failure> {
        // 1. A root port that lost its device takes everything behind it with it.
        for port in 1..=self.ports {
            let connected = self
                .regs
                .read32(usb::port_register_offset(self.op as u8, port).unwrap())?
                & 1
                != 0;
            if !connected {
                self.retire_subtree(port, 0);
            }
        }
        // 2. A device whose hub is gone is detached even if its own hub port still reads
        //    "connected": the hub can be removed from the controller's side first.
        self.retire_detached();
        // 3. Poll every active hub's ports.
        for index in 0..SLOTS {
            if self.devices[index]
                .as_ref()
                .is_some_and(|device| device.is_hub() && matches!(device.health, Health::Active))
            {
                self.cascade(index, console);
            }
        }
        // 4. Release what is retiring, now that nothing has to ask it anything.
        self.sweep(console);
        // 5. Attach devices on root ports that are free.
        for port in 1..=self.ports {
            let connected = self
                .regs
                .read32(usb::port_register_offset(self.op as u8, port).unwrap())?
                & 1
                != 0;
            if !connected {
                if !self.devices.iter().flatten().any(|d| d.port == port) {
                    self.blocked[usize::from(port - 1)] = PortState::Ready;
                }
            } else if self.blocked[usize::from(port - 1)] == PortState::Ready
                && !self
                    .devices
                    .iter()
                    .flatten()
                    .any(|d| d.port == port && d.route == 0)
            {
                self.blocked[usize::from(port - 1)] = PortState::Bound;
                if let Err(error) = self.attach(port, console) {
                    self.blocked[usize::from(port - 1)] = PortState::Faulted;
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
        #[cfg(feature = "hal-xhci-transfer-fault-test")]
        if let Some(ring) = d.fault_ring {
            ring.page.release();
        }
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
        blocked: [PortState::Ready; PORTS],
        controller,
        reports: 0,
        commands_usable: true,
        orphan: None,
        #[cfg(feature = "hal-xhci-transfer-fault-test")]
        owner: dev,
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
    #[cfg(all(
        feature = "hal-xhci-start-fault-test",
        not(feature = "hal-xhci-irq-start-fault-test")
    ))]
    {
        static INJECT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
        if INJECT.swap(false, Ordering::AcqRel) {
            return Err(Failure::Timeout("injected post-run start failure"));
        }
    }
    #[cfg(feature = "hal-xhci-recovery-test")]
    if super::recovery::fail_start(dev) {
        return Err(Failure::Timeout("injected quarantined post-run failure"));
    }
    host.command(0, 23, 0)?; // No-op completion proves command/event DMA before device enumeration.
    let _ = writeln!(
        console,
        "xhci: command/event rings online, context={context}, polled startup; hub-aware enumeration pending"
    );
    host.scan(console)?;
    Ok(())
}
/// Service bounded event batches from the idle consumer, never from an ISR.
pub(super) fn poll_events(dev: usize, console: &mut dyn HalConsole) -> (bool, usize) {
    let mut guard = HOSTS[dev].lock();
    let Some(host) = guard.as_mut() else {
        return (false, 0);
    };
    let before = host.reports;
    for _ in 0..256 {
        match host.next_event() {
            Ok(Some(e)) => host.dispatch(e),
            _ => break,
        }
    }
    for index in 0..SLOTS {
        if host.devices[index]
            .as_ref()
            .is_some_and(|d| matches!(d.health, Health::Retiring(_)))
        {
            let _ = host.cleanup(index, console);
        }
    }
    if let Err(error) = host.scan(console) {
        let _ = writeln!(console, "usb: scan refused: {error:?}");
    }
    let more = host
        .event
        .read_array::<1>(u64::from(host.event_index) * 16 + 12)[0]
        & 1
        == u8::from(host.event_cycle);
    (more, host.reports.saturating_sub(before))
}

/// Service the ports when the controller scheduled no interrupt work.
///
/// The hub's status-change endpoint is the primary hotplug wakeup, but a single completion can
/// be lost: a controller that latches a change while its interrupter is masked, or a hub whose
/// status transfer the guest already read. Hotplug must not depend on one wakeup, so the idle
/// consumer also reads the ports, at most once every tenth of a second. That is a handful of
/// class requests per second and no transfer traffic, and it is what keeps a device plugged into
/// a hub from waiting for some unrelated device to interrupt.
pub(super) fn poll_hubs(dev: usize, console: &mut dyn HalConsole) {
    static LAST: [core::sync::atomic::AtomicU64; driver_start::DEVICES] =
        [const { core::sync::atomic::AtomicU64::new(0) }; driver_start::DEVICES];
    /// Ticks between backstop scans: at 100 Hz this is 100 ms.
    const INTERVAL: u64 = 10;
    let tick = crate::hal::timer::ticks();
    let previous = LAST[dev].load(Ordering::Relaxed);
    if tick < previous + INTERVAL {
        return;
    }
    LAST[dev].store(tick, Ordering::Relaxed);
    let mut guard = HOSTS[dev].lock();
    let Some(host) = guard.as_mut() else {
        return;
    };
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

/// Whether the event ring holds an event the consumer has not read yet.
///
/// The producer owns the next TRB while its cycle bit equals the consumer's; reading that bit
/// is the only way to see an event without taking it. [`finish`] asks this *after* re-enabling
/// the interrupter, which is what makes a completion that lands in the re-enable window visible:
/// the controller writes events with interrupts masked there, so the event itself is the only
/// trace that anything happened.
pub(super) fn events_pending(dev: usize) -> bool {
    let guard = HOSTS[dev].lock();
    let Some(host) = guard.as_ref() else {
        return false;
    };
    host.event
        .read_array::<1>(u64::from(host.event_index) * 16 + 12)[0]
        & 1
        == u8::from(host.event_cycle)
}

pub(super) fn finish_events(dev: usize) -> Result<(), Failure> {
    if let Some(host) = HOSTS[dev].lock().as_ref() {
        write64(
            &host.regs,
            host.runtime + 0x18,
            usb::interrupt::dequeue(host.event.address(0), host.event_index, true)
                .ok_or(Failure::Unsupported)?,
        )?;
    }
    Ok(())
}
pub(super) fn interrupt_probe(dev: usize) -> Result<(), Failure> {
    finish_events(dev)?;
    HOSTS[dev]
        .lock()
        .as_mut()
        .ok_or(Failure::NoController)?
        .command(0, 23, 0)?;
    finish_events(dev)
}

pub(super) fn repair_port(
    dev: usize,
    port: u8,
    console: &mut dyn HalConsole,
) -> Result<(), Failure> {
    HOSTS[dev]
        .lock()
        .as_mut()
        .ok_or(Failure::NoController)?
        .repair(port, console)
}

#[cfg(feature = "hal-xhci-transfer-fault-test")]
pub(super) fn quarantined_test(dev: usize, port: u8, stalled: bool, usable: bool) {
    let host = HOSTS[dev].lock();
    let host = host.as_ref().unwrap();
    let d = host
        .devices
        .iter()
        .flatten()
        .find(|d| d.port == port)
        .unwrap();
    assert!(matches!(d.health, Health::Quarantined(_)));
    assert_eq!(d.pending, 0);
    assert!(d.object.is_none());
    assert_eq!(host.commands_usable, usable);
    if stalled {
        assert_eq!(
            d.output.read_array::<1>(host.context * 5)[0] & 7,
            2,
            "real hardware endpoint must be Halted"
        );
    }
}
#[cfg(feature = "hal-xhci-transfer-fault-test")]
pub(super) fn stall_test(dev: usize, port: u8) -> Result<(), Failure> {
    let mut guard = HOSTS[dev].lock();
    let host = guard.as_mut().ok_or(Failure::NoController)?;
    let index = host
        .devices
        .iter()
        .position(|d| d.as_ref().is_some_and(|d| d.port == port))
        .ok_or(Failure::Unsupported)?;
    let mut d = host.devices[index].take().unwrap();
    // Dedicated fault image only: submit to unsupported IN endpoint 2. QEMU's HID device
    // returns STALL, and xHCI writes a real code-6 completion and Halted output context.
    // Both endpoints' rings/buffers remain owned until Disable Slot or whole-controller stop.
    let result = (|| {
        d.input.write(0, &[0; 4096]);
        let slot = d.output.read_array::<64>(0);
        d.input.write(host.context, &slot[..host.context as usize]);
        let value = u32::from_le_bytes(slot[..4].try_into().unwrap());
        dword(&d.input, host.context, (value & !(31 << 27)) | 5 << 27);
        dword(&d.input, 4, 1 | 1 << 5);
        let old_ep = d.output.read_array::<64>(host.context * 3);
        let ep = host.context * 6;
        d.input.write(ep, &old_ep[..host.context as usize]);
        let value = u32::from_le_bytes(old_ep[..4].try_into().unwrap());
        dword(&d.input, ep, value & !7);
        d.fault_ring = Some(Ring::from_page(Dma::new().ok_or(Failure::Memory)?));
        let ring = d.fault_ring.as_mut().unwrap();
        let pending = ring.put(d.data.address(512), 8, 1 << 10 | 1 << 5);
        let cycle = ring
            .page
            .read_array::<1>(pending - ring.page.address(0) + 12)[0]
            & 1;
        d.input
            .write(ep + 8, &(pending | u64::from(cycle)).to_le_bytes());
        host.command(d.input.address(0), 12, d.slot)?;
        d.role.set_endpoint(0x82);
        d.pending = pending;
        host.regs
            .write32(host.doorbell + u64::from(d.slot) * 4, 5)?;
        Ok(())
    })();
    host.devices[index] = Some(d);
    result
}

#[cfg(feature = "hal-xhci-transfer-fault-test")]
pub(super) fn retired_test(dev: usize, port: u8) {
    let host = HOSTS[dev].lock();
    let host = host.as_ref().unwrap();
    assert!(!host.devices.iter().flatten().any(|d| d.port == port));
    assert!(host.blocked[usize::from(port - 1)] == PortState::Faulted);
    assert!(host.commands_usable);
}
