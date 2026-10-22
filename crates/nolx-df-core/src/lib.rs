//! The NOLX driver framework (`nolx-df`), decided without touching hardware.
//!
//! A driver is a set of match rules plus a dispatch table of three functions: `probe`, `start`,
//! and `stop`. A device is a record of its identity, its lifecycle state, and the driver bound to
//! it. The registry matches devices to drivers, moves each device through its lifecycle, and
//! calls the dispatch table. A third-party driver is therefore a device object and a dispatch
//! table, registered with the framework, and not a patch to the kernel.
//!
//! Lifecycle:
//!
//! ```text
//! Discovered --bind--> Bound --start--> Started --stop--> Stopped --start--> Started
//!     |                  |                 \                  |
//!     +-- (no driver) ---+                  +-- failure --> Failed
//! Failed --recover (checked stop succeeds)--> Stopped
//! any state except Started --remove--> Removed
//! ```
//!
//! Scope: match rules, binding, the lifecycle, and capacity. Interrupts, DMA, power policy, and
//! the calls into the kernel are the driver's business and sit outside this crate. A bounded
//! shutdown transaction supplies platform flush/release callbacks and permanently freezes admission.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

/// The identity of a device as the bus reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceId {
    /// Vendor ID.
    pub vendor: u16,
    /// Device ID.
    pub device: u16,
    /// Base class code.
    pub class: u8,
    /// Subclass code.
    pub subclass: u8,
    /// Programming interface.
    pub prog_if: u8,
    /// PCI segment group number. `0` for legacy single-segment systems.
    pub segment: u16,
}

/// A match rule. A field set to `None` matches any value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Match {
    /// The vendor, or any.
    pub vendor: Option<u16>,
    /// The device, or any.
    pub device: Option<u16>,
    /// The base class, or any.
    pub class: Option<u8>,
    /// The subclass, or any.
    pub subclass: Option<u8>,
    /// The programming interface, or any.
    pub prog_if: Option<u8>,
    /// The PCI segment, or any.
    pub segment: Option<u16>,
}

impl Match {
    /// A rule for one vendor and device on any segment.
    #[must_use]
    pub const fn by_ids(vendor: u16, device: u16) -> Self {
        Self {
            vendor: Some(vendor),
            device: Some(device),
            class: None,
            subclass: None,
            prog_if: None,
            segment: None,
        }
    }

    /// A rule for one vendor, device, and segment.
    #[must_use]
    pub const fn by_ids_segment(vendor: u16, device: u16, segment: u16) -> Self {
        Self {
            vendor: Some(vendor),
            device: Some(device),
            class: None,
            subclass: None,
            prog_if: None,
            segment: Some(segment),
        }
    }

    /// A rule for one class, subclass, and optionally a programming interface on any segment.
    #[must_use]
    pub const fn by_class(class: u8, subclass: u8, prog_if: Option<u8>) -> Self {
        Self {
            vendor: None,
            device: None,
            class: Some(class),
            subclass: Some(subclass),
            prog_if,
            segment: None,
        }
    }

    /// A rule for one class, subclass, optionally a programming interface, and segment.
    #[must_use]
    pub const fn by_class_segment(
        class: u8,
        subclass: u8,
        prog_if: Option<u8>,
        segment: u16,
    ) -> Self {
        Self {
            vendor: None,
            device: None,
            class: Some(class),
            subclass: Some(subclass),
            prog_if,
            segment: Some(segment),
        }
    }

    /// Whether every field this rule sets equals the device's field.
    #[must_use]
    pub fn matches(&self, id: &DeviceId) -> bool {
        field_matches(self.vendor, id.vendor)
            && field_matches(self.device, id.device)
            && field_matches(self.class, id.class)
            && field_matches(self.subclass, id.subclass)
            && field_matches(self.prog_if, id.prog_if)
            && field_matches(self.segment, id.segment)
    }
}

fn field_matches<T: PartialEq>(rule: Option<T>, value: T) -> bool {
    rule.is_none_or(|expected| expected == value)
}

/// The three operations a driver provides. Each takes the device handle the registry assigned.
#[derive(Clone, Copy)]
pub struct Dispatch {
    /// Whether the driver accepts the device. Called only for devices a rule matches.
    pub probe: fn(&DeviceId) -> bool,
    /// Bring the device up. Returns `false` on failure.
    pub start: fn(usize) -> bool,
    /// Quiesce the device. Returns `false` on failure.
    pub stop: fn(usize) -> bool,
}

/// A registered driver.
#[derive(Clone, Copy)]
pub struct Driver {
    /// A name for diagnostics.
    pub name: &'static str,
    /// The rules that decide which devices this driver is offered.
    pub matches: &'static [Match],
    /// The operations.
    pub ops: Dispatch,
}

/// Where a device is in its lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Found on the bus, with no driver bound.
    Discovered,
    /// A driver accepted the device; it is not running.
    Bound,
    /// The driver started the device.
    Started,
    /// The driver stopped the device; it can be started again.
    Stopped,
    /// A start or stop failed. Checked recovery may quiesce it; otherwise resources stay owned.
    Failed,
    /// Removed from the framework. Terminal.
    Removed,
}

/// Why a framework operation was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The registry has no room for another driver or device.
    Full,
    /// No registered driver matched and accepted the device.
    NoDriver,
    /// The device is not in a state that allows the operation.
    BadState,
    /// The device is started, so it must be stopped before removal.
    Busy,
    /// The handle does not name a device.
    UnknownDevice,
    /// The driver's start or stop operation reported failure.
    OpFailed,
}

/// A device record in the registry.
#[derive(Clone, Copy, Debug)]
struct Device {
    id: DeviceId,
    state: State,
    driver: Option<usize>,
}

/// A fixed-capacity registry of drivers and devices. `D` is the driver capacity and `N` the device
/// capacity. Shutdown permanently freezes mutations and transport admission, including on failure.
pub struct Registry<const D: usize, const N: usize> {
    drivers: [Option<Driver>; D],
    devices: [Option<Device>; N],
    frozen: bool,
}

impl<const D: usize, const N: usize> Default for Registry<D, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const D: usize, const N: usize> Registry<D, N> {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            drivers: [None; D],
            devices: [None; N],
            frozen: false,
        }
    }

    /// Register a driver. Returns its index. Drivers are offered devices in registration order.
    ///
    /// # Errors
    /// [`Error::Full`] when the registry has no free driver slot.
    pub fn add_driver(&mut self, driver: Driver) -> Result<usize, Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let index = self
            .drivers
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        self.drivers[index] = Some(driver);
        Ok(index)
    }

    /// Look up a registered driver's stable slot by name.
    #[must_use]
    pub fn find_driver(&self, name: &str) -> Option<usize> {
        self.drivers
            .iter()
            .position(|slot| slot.is_some_and(|driver| driver.name == name))
    }

    /// The bound driver's diagnostic name, retained across stop and removal.
    pub fn driver_name(&self, dev: usize) -> Result<&'static str, Error> {
        let index = self.device(dev)?.driver.ok_or(Error::NoDriver)?;
        self.drivers
            .get(index)
            .and_then(Option::as_ref)
            .map(|driver| driver.name)
            .ok_or(Error::NoDriver)
    }

    /// Record a discovered device. Returns its handle.
    ///
    /// # Errors
    /// [`Error::Full`] when the registry has no free device slot.
    pub fn add_device(&mut self, id: DeviceId) -> Result<usize, Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let index = self
            .devices
            .iter()
            .position(Option::is_none)
            .ok_or(Error::Full)?;
        self.devices[index] = Some(Device {
            id,
            state: State::Discovered,
            driver: None,
        });
        Ok(index)
    }

    /// The lifecycle state of a device.
    ///
    /// # Errors
    /// [`Error::UnknownDevice`] for a handle that names no device.
    pub fn state(&self, dev: usize) -> Result<State, Error> {
        Ok(self.device(dev)?.state)
    }

    /// The index of the driver bound to a device, if any.
    ///
    /// # Errors
    /// [`Error::UnknownDevice`] for a handle that names no device.
    pub fn driver_of(&self, dev: usize) -> Result<Option<usize>, Error> {
        Ok(self.device(dev)?.driver)
    }

    /// Offer a discovered device to the registered drivers, in order. The first driver with a
    /// matching rule whose probe accepts the device is bound to it.
    ///
    /// # Errors
    /// [`Error::BadState`] unless the device is `Discovered`; [`Error::NoDriver`] when no driver
    /// accepts it, in which case the device stays `Discovered`.
    pub fn bind(&mut self, dev: usize) -> Result<usize, Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let device = *self.device(dev)?;
        if device.state != State::Discovered {
            return Err(Error::BadState);
        }
        let accepted = self.drivers.iter().enumerate().find_map(|(index, slot)| {
            let driver = (*slot)?;
            let offered = driver.matches.iter().any(|rule| rule.matches(&device.id));
            (offered && (driver.ops.probe)(&device.id)).then_some(index)
        });
        let index = accepted.ok_or(Error::NoDriver)?;
        let record = self.device_mut(dev)?;
        record.state = State::Bound;
        record.driver = Some(index);
        Ok(index)
    }

    /// Start a bound or stopped device.
    ///
    /// # Errors
    /// [`Error::BadState`] unless the device is `Bound` or `Stopped`; [`Error::OpFailed`] when the
    /// driver's start fails, in which case the device becomes `Failed`.
    pub fn start(&mut self, dev: usize) -> Result<(), Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let (state, driver) = {
            let record = self.device(dev)?;
            (record.state, record.driver)
        };
        if !matches!(state, State::Bound | State::Stopped) {
            return Err(Error::BadState);
        }
        let ops = self.ops_for(driver)?;
        let next = if (ops.start)(dev) {
            State::Started
        } else {
            State::Failed
        };
        self.device_mut(dev)?.state = next;
        if next == State::Started {
            Ok(())
        } else {
            Err(Error::OpFailed)
        }
    }

    /// Stop a started device.
    ///
    /// # Errors
    /// [`Error::BadState`] unless the device is `Started`; [`Error::OpFailed`] when the driver's
    /// stop fails, in which case the device becomes `Failed`.
    pub fn stop(&mut self, dev: usize) -> Result<(), Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        self.stop_inner(dev)
    }

    fn stop_inner(&mut self, dev: usize) -> Result<(), Error> {
        let (state, driver) = {
            let record = self.device(dev)?;
            (record.state, record.driver)
        };
        if state != State::Started {
            return Err(Error::BadState);
        }
        let ops = self.ops_for(driver)?;
        let next = if (ops.stop)(dev) {
            State::Stopped
        } else {
            State::Failed
        };
        self.device_mut(dev)?.state = next;
        if next == State::Stopped {
            Ok(())
        } else {
            Err(Error::OpFailed)
        }
    }

    /// Retry the original driver's stop callback for a failed device, never its start callback.
    /// Success moves only this record to Stopped; failure preserves Failed and ownership. The
    /// platform must admit this operation only for drivers whose stop handles partial startup.
    /// It must release platform resources only after this operation returns success. Shutdown
    /// freeze forbids recovery even after a failed shutdown; this is not an admission thaw.
    pub fn recover(&mut self, dev: usize) -> Result<(), Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let record = *self.device(dev)?;
        if record.state != State::Failed {
            return Err(Error::BadState);
        }
        let ops = self.ops_for(record.driver)?;
        if !(ops.stop)(dev) {
            return Err(Error::OpFailed);
        }
        self.device_mut(dev)?.state = State::Stopped;
        Ok(())
    }

    /// Remove a device from the framework. A started device must be stopped first.
    ///
    /// # Errors
    /// [`Error::Busy`] for a started device; [`Error::BadState`] for a device already removed.
    pub fn remove(&mut self, dev: usize) -> Result<(), Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        let state = self.device(dev)?.state;
        match state {
            State::Started => Err(Error::Busy),
            State::Removed => Err(Error::BadState),
            _ => {
                self.device_mut(dev)?.state = State::Removed;
                Ok(())
            }
        }
    }

    /// Admission is permanently closed after a shutdown attempt, successful or not.
    #[must_use]
    pub const fn frozen(&self) -> bool {
        self.frozen
    }

    /// Freeze admission, flush every started device, then stop in reverse discovery order.
    /// The caller holds exclusive registry ownership across this operation. `flush` must reject
    /// unknown/unsupported drivers; `after_stop` releases platform resources only after success.
    /// Failure is fail-closed: no restart, retry, binding or removal may reopen admission. Earlier
    /// successful stops stay stopped; failed callbacks retain their own unsafe-to-free resources.
    /// Failed records refuse preflight even if their hardware might already have rolled back.
    pub fn quiesce(
        &mut self,
        mut flush: impl FnMut(usize, &'static str) -> bool,
        mut after_stop: impl FnMut(usize),
    ) -> Result<usize, Error> {
        if self.frozen {
            return Err(Error::Busy);
        }
        self.frozen = true;
        if self
            .devices
            .iter()
            .flatten()
            .any(|d| d.state == State::Failed)
        {
            return Err(Error::BadState);
        }
        // No device is stopped until all durable barriers succeed.
        for dev in 0..N {
            if self.state(dev) == Ok(State::Started) && !flush(dev, self.driver_name(dev)?) {
                return Err(Error::OpFailed);
            }
        }
        let mut stopped = 0;
        for dev in (0..N).rev() {
            if self.state(dev) == Ok(State::Started) {
                self.stop_inner(dev)?;
                after_stop(dev);
                stopped += 1;
            }
        }
        Ok(stopped)
    }

    fn device(&self, dev: usize) -> Result<&Device, Error> {
        self.devices
            .get(dev)
            .and_then(Option::as_ref)
            .ok_or(Error::UnknownDevice)
    }

    fn device_mut(&mut self, dev: usize) -> Result<&mut Device, Error> {
        self.devices
            .get_mut(dev)
            .and_then(Option::as_mut)
            .ok_or(Error::UnknownDevice)
    }

    fn ops_for(&self, driver: Option<usize>) -> Result<Dispatch, Error> {
        let index = driver.ok_or(Error::BadState)?;
        self.drivers
            .get(index)
            .and_then(Option::as_ref)
            .map(|driver| driver.ops)
            .ok_or(Error::BadState)
    }
}

#[cfg(test)]
mod tests;
