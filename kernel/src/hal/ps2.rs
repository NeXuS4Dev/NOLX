//! i8042 keyboard/auxiliary initialization and interrupt-driven bounded input delivery.
//! Device bytes are decoded in the ISR; console output stays in the idle consumer.

use super::{apic, inb, outb, pic};
use crate::sync::IrqMutex;
use nolx_hal_core::console::HalConsole;
use nolx_input_core::{
    self as input,
    events::{Kind, MouseDecoder, Source},
    objects::{Class, DeviceId, Location},
    Decoder,
};

const DATA: u16 = input::DATA_PORT;
const STATUS: u16 = input::STATUS_PORT;
const LIMIT: u32 = 1_000_000;

struct State {
    keyboard: Decoder,
    mouse: MouseDecoder,
    keyboard_id: Option<DeviceId>,
    mouse_id: Option<DeviceId>,
}
static STATE: IrqMutex<State> = IrqMutex::new(State {
    keyboard: Decoder::new(),
    mouse: MouseDecoder::new(),
    keyboard_id: None,
    mouse_id: None,
});

fn status() -> u8 {
    // SAFETY: the i8042 status port is architected on the supported PC platforms.
    unsafe { inb(STATUS) }
}
fn read_data() -> u8 {
    // SAFETY: callers check output-full before consuming the i8042 data port.
    unsafe { inb(DATA) }
}
fn write(port: u16, byte: u8) -> Result<(), ()> {
    for _ in 0..LIMIT {
        if !input::input_full(status()) {
            // SAFETY: callers name only i8042 command/data ports; input-full was clear.
            unsafe { outb(port, byte) };
            return Ok(());
        }
        core::hint::spin_loop();
    }
    Err(())
}
fn response(auxiliary: bool) -> Result<u8, ()> {
    for _ in 0..LIMIT {
        let state = status();
        if input::output_full(state) {
            let byte = read_data();
            if input::transfer_failed(state) {
                return Err(());
            }
            if (state & 0x20 != 0) == auxiliary {
                return Ok(byte);
            }
        }
        core::hint::spin_loop();
    }
    Err(())
}
fn command(byte: u8) -> Result<(), ()> {
    write(STATUS, byte)
}
fn configuration(byte: u8) -> Result<(), ()> {
    command(input::CMD_WRITE_CONFIG)?;
    write(DATA, byte)
}
fn device_command(auxiliary: bool, byte: u8) -> Result<(), ()> {
    for _ in 0..3 {
        if auxiliary {
            command(0xd4)?;
        }
        write(DATA, byte)?;
        match response(auxiliary)? {
            input::DEVICE_ACK => return Ok(()),
            input::DEVICE_RESEND => continue,
            _ => return Err(()),
        }
    }
    Err(())
}

fn initialize() -> Result<bool, ()> {
    command(input::CMD_DISABLE_KEYBOARD)?;
    command(0xa7)?;
    for _ in 0..64 {
        if !input::output_full(status()) {
            break;
        }
        let _ = read_data();
    }
    command(input::CMD_READ_CONFIG)?;
    let config = response(false)?;
    configuration((config | input::CONFIG_TRANSLATION) & !3)?;
    command(input::CMD_ENABLE_KEYBOARD)?;
    command(0xa8)?;
    // Restore a standard three-byte mouse protocol, without wheel extensions.
    let mouse = device_command(true, 0xf6)
        .and_then(|()| device_command(true, 0xf4))
        .is_ok();
    device_command(false, 0xf4)?;
    let mut config = input::keyboard_config(config) & !2;
    if mouse {
        config = (config | 2) & !0x20;
    } else {
        config |= 0x20;
    }
    configuration(config)?;
    if apic::is_enabled() {
        if !apic::unmask_legacy_irq(1) {
            return Err(());
        }
        if mouse && !apic::unmask_legacy_irq(12) {
            apic::mask_legacy_irq(1);
            return Err(());
        }
    } else {
        pic::unmask_irq(1);
        if mouse {
            pic::unmask_irq(12);
        }
    }
    Ok(mouse)
}

/// Initialize both ports before CPU interrupts are enabled; absent devices are nonfatal.
pub fn init(console: &mut dyn HalConsole) {
    match initialize() {
        Ok(mouse) => {
            let keyboard = super::input::register(
                Source::Ps2Keyboard,
                Class::Keyboard,
                Location::Ps2Keyboard,
                console,
            );
            if let Ok(keyboard) = keyboard {
                let auxiliary = if mouse {
                    super::input::register(
                        Source::Ps2Mouse,
                        Class::Pointer,
                        Location::Ps2Mouse,
                        console,
                    )
                    .ok()
                } else {
                    None
                };
                let mut state = STATE.lock();
                state.keyboard_id = Some(keyboard);
                state.mouse_id = auxiliary;
                drop(state);
                if auxiliary.is_some() {
                    let _ = writeln!(console, "input: PS/2 keyboard IRQ1, mouse IRQ12 ready");
                } else {
                    let _ = writeln!(
                        console,
                        "input: PS/2 keyboard IRQ1 ready; no auxiliary mouse"
                    );
                }
            } else {
                let _ = writeln!(console, "input: PS/2 input-object registration refused");
            }
        }
        Err(()) => {
            let _ = writeln!(console, "input: PS/2 initialization refused or timed out");
        }
    }
}

/// Drain a bounded number of controller bytes before the caller sends PIC/APIC EOI.
pub fn on_interrupt() {
    let mut state = STATE.lock();
    for _ in 0..64 {
        let flags = status();
        if !input::output_full(flags) {
            break;
        }
        let byte = read_data();
        if input::transfer_failed(flags) {
            state.keyboard = Decoder::new();
            state.mouse.reset();
            continue;
        }
        let (id, event) = if flags & 0x20 != 0 {
            (state.mouse_id, state.mouse.feed(byte).map(Kind::Pointer))
        } else {
            (state.keyboard_id, state.keyboard.feed(byte).map(Kind::Key))
        };
        if let (Some(id), Some(event)) = (id, event) {
            let _ = super::input::publish(id, event);
        }
    }
}

/// Irreversibly retire legacy input producers and mask their interrupt delivery for shutdown.
pub fn quiesce() {
    let _interrupts = super::cpu::InterruptGuard::disable();
    apic::mask_legacy_irq(1);
    apic::mask_legacy_irq(12);
    pic::mask_irq(1);
    pic::mask_irq(12);
    let mut state = STATE.lock();
    for id in [state.keyboard_id.take(), state.mouse_id.take()]
        .into_iter()
        .flatten()
    {
        let _ = super::input::disconnect(id);
    }
}
