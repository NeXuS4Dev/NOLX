//! Device power states: `_ADR`, `_PSC`, `_PRW`, and the `_PS0`/`_PS3` transitions.
//!
//! A device's power state belongs to OSPM, not to its driver: firmware declares the current state
//! with `_PSC`, the transition to a low-power state with `_PS3` and the return to D0 with `_PS0`,
//! and the wake capability with `_PRW`. `_ADR` is how a PCI function finds the namespace node
//! that describes it. All four are ordinary AML members, so everything here runs through the
//! bounded evaluator rather than a byte scan.
//!
//! The rules are deliberately narrow. `_PSC` must be an integer in 0..=3, because ACPI defines
//! only D0..D3 for these methods. `_PRW` must be the two-element package (`GPE bit index`,
//! `lowest sleep state`) the specification defines; the extended form whose first element names
//! a GPE block device is refused rather than read as a bit index, since this kernel has no GPE
//! block device model. A missing member is always distinguishable from a failed one.

extern crate alloc;

use alloc::vec::Vec;

use crate::aml::{Error, Namespace, Path, Value};

/// A device power state, D0 (fully on) to D3hot (lowest state reachable without removing power).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum DState {
    /// Fully on.
    D0,
    /// A vendor-defined intermediate state.
    D1,
    /// A deeper vendor-defined state.
    D2,
    /// Lowest power state with the device still on its bus.
    D3Hot,
}

impl DState {
    /// Decode the integer `_PSC` or a power resource's `_ON` reports.
    ///
    /// Values above 3 are refused: ACPI's `_PSC` models D0..D3 only, and a firmware that
    /// reports something else is not something to guess at.
    pub fn parse(value: u64) -> Result<Self, Error> {
        match value {
            0 => Ok(Self::D0),
            1 => Ok(Self::D1),
            2 => Ok(Self::D2),
            3 => Ok(Self::D3Hot),
            _ => Err(Error::Type),
        }
    }

    /// The name the diagnostics use.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::D0 => "D0",
            Self::D1 => "D1",
            Self::D2 => "D2",
            Self::D3Hot => "D3hot",
        }
    }

    /// Whether the device is in a low-power state that needs `_PS0` to return from.
    #[must_use]
    pub const fn is_low_power(self) -> bool {
        !matches!(self, Self::D0)
    }

    /// The member that enters this state from D0: `_PS3` for the low-power states.
    #[must_use]
    pub const fn enter_member(self) -> Option<[u8; 4]> {
        match self {
            Self::D0 => None,
            _ => Some(*b"_PS3"),
        }
    }
}

/// A device's declared wake capability: the GPE bit that fires and the deepest sleep state it
/// can wake the system from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WakeSource {
    /// Index of the wake event inside the GPE block `_PRW` implies.
    pub gpe: u32,
    /// The lowest (deepest) system sleep state the device can wake from; 3 is S3.
    pub lowest_sleep_state: u8,
}

impl WakeSource {
    /// Whether this source can wake the machine from the given system sleep state.
    ///
    /// ACPI encodes the deepest state the source can wake from, so a source that names S3 can
    /// also wake from S1 and S2, and a source that names only S0 cannot wake a sleeping system.
    #[must_use]
    pub const fn wakes_from(self, state: u8) -> bool {
        state <= self.lowest_sleep_state && state > 0
    }
}

/// Decode a `_PRW` package.
///
/// The supported shape is the specification's two-element package: an integer GPE bit index and
/// an integer lowest wakeable sleep state. The extended form, whose first element is a package
/// naming a GPE block device, is refused: this kernel has no block-device GPE model, and reading
/// its first element as a bit index would invent a wake source that does not exist.
pub fn parse_prw(value: &Value) -> Result<WakeSource, Error> {
    let Value::Package(elements) = value else {
        return Err(Error::Type);
    };
    if elements.len() != 2 {
        return Err(Error::Type);
    }
    let gpe = elements[0].integer()?;
    let lowest = elements[1].integer()?;
    let gpe = u32::try_from(gpe).map_err(|_| Error::Type)?;
    let lowest_sleep_state = u8::try_from(lowest).map_err(|_| Error::Type)?;
    // The specification's sleep states are S0..S4; anything else is not a wake depth.
    if lowest_sleep_state > 4 {
        return Err(Error::Type);
    }
    Ok(WakeSource {
        gpe,
        lowest_sleep_state,
    })
}

/// Read a device's `_ADR`: the bus address of the function the namespace node describes.
///
/// A PCI `_ADR` is the device number in bits 31:16 and the function in 15:0. An absent member is
/// `Ok(None)`; a present member that is not an integer is refused.
pub fn device_address(namespace: &Namespace<'_>, path: &Path) -> Result<Option<u64>, Error> {
    match namespace.device_value(path, *b"_ADR")? {
        None => Ok(None),
        Some(value) => Ok(Some(value.integer()?)),
    }
}

/// Split a PCI `_ADR` into its device and function numbers.
#[must_use]
pub fn adr_parts(adr: u64) -> (u8, u8) {
    (((adr >> 16) & 0xff) as u8, (adr & 0xffff) as u8)
}

/// Read a device's declared current power state (`_PSC`).
///
/// An absent member is `Ok(None)`: firmware that declares no `_PSC` imposes no state of its own,
/// and the device is left in whatever state its driver established. A failing or malformed
/// member is an error, never an assumed D0.
pub fn current_state(namespace: &Namespace<'_>, path: &Path) -> Result<Option<DState>, Error> {
    match namespace.device_value(path, *b"_PSC")? {
        None => Ok(None),
        Some(value) => DState::parse(value.integer()?).map(Some),
    }
}

/// Read a device's declared wake source (`_PRW`).
pub fn wake_source(namespace: &Namespace<'_>, path: &Path) -> Result<Option<WakeSource>, Error> {
    match namespace.device_value(path, *b"_PRW")? {
        None => Ok(None),
        Some(value) => parse_prw(&value).map(Some),
    }
}

/// Which of the device power methods a namespace node actually declares.
///
/// This is a static question — it never invokes a method — so a policy can decide what it can
/// ask of a device before evaluating anything.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PowerMethods {
    /// The node declares `_PS0` (return to D0).
    pub ps0: bool,
    /// The node declares `_PS3` (enter the low-power state).
    pub ps3: bool,
    /// The node declares `_PSC` (report the current state).
    pub psc: bool,
    /// The node declares `_PRW` (wake capability).
    pub prw: bool,
}

impl PowerMethods {
    /// Whether the node declares anything this module models.
    #[must_use]
    pub const fn any(self) -> bool {
        self.ps0 || self.ps3 || self.psc || self.prw
    }

    /// Whether the node can actually be put into a low-power state by firmware.
    #[must_use]
    pub const fn can_suspend(self) -> bool {
        self.ps3
    }
}

/// Whether a device node declares the given member as a method or a data object.
///
/// Both are accepted: `_PS0` is normally a method, but a namespace may also carry a plain named
/// object, and the evaluator resolves either form the same way.
#[must_use]
pub fn declares(namespace: &Namespace<'_>, path: &Path, member: [u8; 4]) -> bool {
    path.child(member)
        .is_ok_and(|child| namespace.contains(&child))
}

/// Collect the power methods one device node declares.
#[must_use]
pub fn power_methods(namespace: &Namespace<'_>, path: &Path) -> PowerMethods {
    PowerMethods {
        ps0: declares(namespace, path, *b"_PS0"),
        ps3: declares(namespace, path, *b"_PS3"),
        psc: declares(namespace, path, *b"_PSC"),
        prw: declares(namespace, path, *b"_PRW"),
    }
}

/// One device's contribution to a power transition plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PowerTarget {
    /// The namespace node.
    pub path: Path,
    /// The state the node reports now, when it reports one.
    pub current: Option<DState>,
    /// Which power methods it declares.
    pub methods: PowerMethods,
    /// Its declared wake source, when it declares one.
    pub wake: Option<WakeSource>,
}

/// Every device node that declares at least one power method, sorted by namespace path so a
/// transition plan is deterministic and a log can be compared across boots.
#[must_use]
pub fn power_targets(namespace: &Namespace<'_>) -> Vec<PowerTarget> {
    let mut targets = Vec::new();
    for path in namespace.devices() {
        let methods = power_methods(namespace, path);
        if !methods.any() {
            continue;
        }
        targets.push(PowerTarget {
            path: path.clone(),
            current: current_state(namespace, path).ok().flatten(),
            methods,
            wake: wake_source(namespace, path).ok().flatten(),
        });
    }
    targets.sort_by(|a, b| a.path.segments().cmp(b.path.segments()));
    targets
}

#[cfg(test)]
mod tests;
