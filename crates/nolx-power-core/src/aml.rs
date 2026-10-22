//! A bounded, side-effect-free AML namespace and method evaluator.
//!
//! This deliberately does not emulate hardware fields, synchronization or arbitrary AML.
//! Such declarations can be loaded, but evaluation refuses their use. Tables load atomically;
//! unknown module-level instructions never authorize a partially scanned namespace.

extern crate alloc;
use alloc::{vec, vec::Vec};
use core::fmt;

/// Maximum objects, including predefined scopes, in one namespace.
pub const MAX_OBJECTS: usize = 2048;
/// Maximum namespace/parser/evaluator nesting depth.
pub const MAX_DEPTH: usize = 32;
/// Maximum instructions per evaluation (shared across calls and loops).
pub const MAX_STEPS: usize = 10_000;
/// Maximum bytes in a buffer or string value.
pub const MAX_VALUE_BYTES: usize = 16_384;
/// Maximum elements of a package.
pub const MAX_PACKAGE: usize = 256;
/// Aggregate owned data retained by a namespace (apart from bounded names/objects).
pub const MAX_NAMESPACE_BYTES: usize = 256 * 1024;
const MAX_DATA_BYTES: usize = 64 * 1024;

/// A malformed, unsupported or resource-limited AML operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Error {
    /// Input ends inside an operand.
    Truncated,
    /// Invalid package length/boundary.
    Package,
    /// Invalid name or parent traversal above root.
    Name,
    /// Namespace object is absent.
    Missing,
    /// Duplicate definition.
    Duplicate,
    /// Wrong operand/result type.
    Type,
    /// Unsupported opcode or opaque hardware object; extended opcodes start with 0x5b.
    Unsupported(u16),
    /// Depth, object count or value size limit.
    Limit,
    /// Instruction budget exhausted, including recursive calls/loops.
    Steps,
    /// A method fell through without returning a value.
    NoReturn,
    /// Only local/argument targets and NullName are supported; global writes are refused.
    Target,
    /// A field write was not covered by an explicit caller-supplied permission.
    WriteDenied,
}

/// An absolute, validated AML namespace path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path(Vec<[u8; 4]>);
impl Path {
    /// Root namespace.
    pub fn root() -> Self {
        Self(Vec::new())
    }
    /// Parse an absolute display path such as `\\_SB_.PCI0` (four-byte segments).
    pub fn parse(text: &str) -> Result<Self, Error> {
        let tail = text.strip_prefix('\\').ok_or(Error::Name)?;
        let mut out = Self::root();
        if tail.is_empty() {
            return Ok(out);
        }
        for part in tail.split('.') {
            let segment: [u8; 4] = part.as_bytes().try_into().map_err(|_| Error::Name)?;
            check_segment(&segment)?;
            if out.0.len() == MAX_DEPTH {
                return Err(Error::Limit);
            }
            out.0.push(segment);
        }
        Ok(out)
    }
    /// Enclosing namespace path; the root is its own parent.
    pub fn parent(&self) -> Self {
        let mut p = self.clone();
        p.0.pop();
        p
    }
    fn child(&self, segment: [u8; 4]) -> Result<Self, Error> {
        if self.0.len() == MAX_DEPTH {
            return Err(Error::Limit);
        }
        let mut p = self.clone();
        p.0.push(segment);
        Ok(p)
    }
    /// Check if this path starts with the given prefix.
    pub fn starts_with(&self, prefix: &Path) -> bool {
        if prefix.0.len() > self.0.len() {
            return false;
        }
        self.0[..prefix.0.len()] == prefix.0
    }
}
impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\\")?;
        for (i, segment) in self.0.iter().enumerate() {
            if i != 0 {
                f.write_str(".")?;
            }
            for byte in segment {
                write!(f, "{}", char::from(*byte))?;
            }
        }
        Ok(())
    }
}
fn check_segment(s: &[u8; 4]) -> Result<(), Error> {
    if !(s[0] == b'_' || s[0].is_ascii_uppercase())
        || !s[1..]
            .iter()
            .all(|b| *b == b'_' || b.is_ascii_uppercase() || b.is_ascii_digit())
    {
        return Err(Error::Name);
    }
    Ok(())
}

/// A pure AML result. References inside packages are retained as namespace paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    /// Unsigned AML integer, stored in u64 and masked to the selected 32/64-bit model.
    Integer(u64),
    /// Owned resource/data bytes.
    Buffer(Vec<u8>),
    /// NUL-terminated AML string, without the terminator.
    String(Vec<u8>),
    /// Bounded list of values.
    Package(Vec<Value>),
    /// A named object reference, resolved when evaluated as a term argument.
    Reference(Path),
    /// Forward data/package reference, retaining its original namespace search context.
    DeferredReference(NameReference),
    /// Uninitialized local or unused package element.
    Uninitialized,
}
impl Value {
    /// Require an integer rather than silently coercing another type.
    pub fn integer(&self) -> Result<u64, Error> {
        if let Self::Integer(v) = self {
            Ok(*v)
        } else {
            Err(Error::Type)
        }
    }
}
/// A bounded PCI configuration field address. `FieldReader` is read-only; staged write intents
/// are data only and are never applied by the AML evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciField {
    /// PCI device address encoded as device << 16 | function, on root bus zero.
    pub address: u32,
    /// Byte offset within legacy configuration space.
    pub offset: u16,
    /// Number of bytes requested (the supported snapshot subset is byte access).
    pub bytes: u8,
}
/// Read-only, explicitly authorized PCI field snapshots. Other operation regions stay opaque.
pub trait FieldReader {
    /// Return a snapshot value, or refuse a location not covered by the provider.
    fn read(&self, field: PciField) -> Result<u64, Error>;
}
/// One operation-region space this crate can hand to a caller's provider.
///
/// `PciConfig` is absent on purpose: a PCI configuration field has its own address type and its
/// own journal, so the two never share a provider. `EmbeddedControl` is modelled as an address
/// space and a provider may serve it, but the kernel has no embedded controller and refuses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionSpace {
    /// Physical memory, behind a device's memory-mapped registers.
    SystemMemory,
    /// I/O ports.
    SystemIo,
    /// Embedded controller data space.
    EmbeddedControl,
}

/// A field inside a firmware-declared operation region, handed to a caller's provider.
///
/// The provider is asked for naturally aligned `access`-byte units; the field's own masking and
/// shifting stay in this crate, so a provider cannot widen a field by accident.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionField {
    /// Which space the region names.
    pub space: RegionSpace,
    /// Absolute byte address of the first unit that holds the field.
    pub address: u64,
    /// Field width in bits, at most 64.
    pub bits: u8,
    /// How far the field starts into the first unit, in bits.
    pub shift: u8,
    /// Unit width in bytes: 1, 2, 4 or 8.
    pub access: u8,
}

/// Caller-supplied reads of operation-region fields, in units the provider chooses to trust.
///
/// A provider must return the little-endian value of `bytes` at an address it is willing to
/// touch; this crate has already bounded the address to the region the firmware declared, so a
/// provider only has to decide whether *that* region is one it will read. A read that cannot be
/// confirmed is an error, never a fabricated value.
pub trait RegionFieldReader {
    /// Read one naturally aligned unit, or refuse the space or the address.
    fn read(&self, space: RegionSpace, address: u64, bytes: u8) -> Result<u64, Error>;
}

/// One operation-region field value read during method evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionFieldRead {
    /// The field, as the provider saw it.
    pub field: RegionField,
    /// The extracted field value, already shifted and masked.
    pub value: u64,
}

/// One root-bus-zero PCI field value read while planning a method.
///
/// Shadow reads from fields already written in the AML method are not included; they are satisfied
/// from the ordered write journal instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFieldRead {
    /// Exact field read from the caller's snapshot.
    pub field: PciField,
    /// The byte value observed during planning.
    pub value: u8,
}
/// One whitelisted byte write produced by bounded method planning; never applied by the AML evaluator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFieldWrite {
    /// Exact root-bus-zero PCI configuration byte.
    pub field: PciField,
    /// Value to write to that byte, after AML field-width truncation.
    pub value: u8,
}
/// Caller-declared allowed byte values for one exact PCI configuration field.
///
/// Permissions are data rather than callbacks: planning cannot trigger hidden hardware effects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PciFieldWritePermission {
    /// Exact field authorized by this permission.
    pub field: PciField,
    values: [u64; 4],
}
impl PciFieldWritePermission {
    /// Authorize the listed values for one field. An empty list denies every write.
    pub fn from_values(field: PciField, allowed_values: &[u8]) -> Result<Self, Error> {
        if allowed_values.len() > 256 {
            return Err(Error::Limit);
        }
        let mut values = [0u64; 4];
        for value in allowed_values {
            let value = usize::from(*value);
            values[value / 64] |= 1u64 << (value % 64);
        }
        Ok(Self { field, values })
    }
    fn allows(self, value: u8) -> bool {
        let value = usize::from(value);
        self.values[value / 64] & (1u64 << (value % 64)) != 0
    }
}
/// Result of software-only AML method execution with authorized PCI field effects journaled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodEffects {
    /// Returned AML value, if the method executed a Return opcode.
    pub result: Option<Value>,
    /// Unique PCI bytes read from the caller's snapshot during method evaluation.
    pub reads: Vec<PciFieldRead>,
    /// Ordered field-write intents. Hardware is untouched until a separate transaction applies them.
    pub writes: Vec<PciFieldWrite>,
    /// Operation-region field values read during evaluation, in the order they were read.
    ///
    /// Region reads have no journal of their own because nothing here writes to a region: the
    /// list exists so a caller can see which hardware a method looked at before it trusted the
    /// result.
    pub region_reads: Vec<RegionFieldRead>,
    /// Ordered region field-write intents, exactly like `writes` but for operation regions.
    pub region_writes: Vec<RegionFieldWrite>,
}
const MAX_FIELD_WRITE_PERMISSIONS: usize = 32;
const MAX_FIELD_READS: usize = 64;
const MAX_FIELD_WRITES: usize = 64;

/// Byte-granular PCI configuration access used by a caller-owned write transaction.
///
/// Implementations must restrict access to the field supplied on each call and serialize the
/// whole transaction against other configuration-space users. `Err` means the access could not
/// be confirmed; the transaction will attempt verified rollback of every attempted write.
pub trait PciFieldWriteBackend {
    /// Transport failure type.
    type Error;
    /// Read one exact PCI configuration byte.
    fn read_byte(&mut self, field: PciField) -> Result<u8, Self::Error>;
    /// Write one exact PCI configuration byte.
    fn write_byte(&mut self, field: PciField, value: u8) -> Result<(), Self::Error>;
}

/// Failure while applying a bounded PCI field-write journal transactionally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PciFieldWriteTransactionError<BackendError, PostconditionError> {
    /// The read, write, or permission list exceeded the bounded transaction capacity.
    Limit,
    /// A field is not an aligned root-bus-zero byte within the supported legacy configuration space.
    InvalidField,
    /// The same field was listed more than once in the snapshot or permission set.
    DuplicateField,
    /// A staged write does not match an exact field/value permission.
    PermissionDenied,
    /// A snapshot field could not be re-read before any writes began.
    SnapshotRead(BackendError),
    /// A planning snapshot no longer matches hardware before any write began.
    StaleSnapshot,
    /// A target field changed between preflight and its application; prior writes were rolled back.
    ChangedDuringApply,
    /// A byte could not be read before a write.
    Read(BackendError),
    /// A byte write reported a transport failure.
    Write(BackendError),
    /// The write completed but its byte readback could not be obtained.
    Readback(BackendError),
    /// A write's byte readback did not match the requested value.
    ReadbackMismatch,
    /// Caller verification (for example, re-evaluated `_CRS`) refused the transaction.
    Postcondition(PostconditionError),
    /// At least one rollback byte could not be restored and verified.
    RollbackFailed,
}

fn valid_transaction_field(field: PciField) -> bool {
    field.bytes == 1 && field.offset < 256 && field.address & !0x1f0007 == 0
}

fn rollback_field_writes<B: PciFieldWriteBackend>(
    backend: &mut B,
    previous: &[PciFieldWrite],
) -> bool {
    let mut restored = true;
    for write in previous.iter().rev() {
        let _ = backend.write_byte(write.field, write.value);
        if !matches!(backend.read_byte(write.field), Ok(value) if value == write.value) {
            restored = false;
        }
    }
    restored
}

/// Apply authorized PCI byte writes, verify each readback, and run a caller postcondition.
///
/// Every write is authorized and every field is validated before the first backend operation.
/// The transaction first rechecks the fields read by AML against the planning snapshot, then
/// writes in AML order. A write error, readback mismatch, stale snapshot, or failed postcondition
/// restores attempted writes in reverse order and verifies each restoration. A failed rollback is
/// reported separately because the caller must quarantine the device or halt. The postcondition
/// is a separate callback so it cannot use the backend to bypass the exact write permissions; it
/// may refresh a read-only field snapshot, re-evaluate `_CRS`, and publish only after those checks.
pub fn apply_pci_field_writes<B, F, PostconditionError>(
    effects: &MethodEffects,
    permissions: &[PciFieldWritePermission],
    backend: &mut B,
    postcondition: F,
) -> Result<(), PciFieldWriteTransactionError<B::Error, PostconditionError>>
where
    B: PciFieldWriteBackend,
    F: FnOnce() -> Result<(), PostconditionError>,
{
    if effects.reads.len() > MAX_FIELD_READS
        || effects.writes.len() > MAX_FIELD_WRITES
        || permissions.len() > MAX_FIELD_WRITE_PERMISSIONS
    {
        return Err(PciFieldWriteTransactionError::Limit);
    }
    for (index, read) in effects.reads.iter().enumerate() {
        if !valid_transaction_field(read.field) {
            return Err(PciFieldWriteTransactionError::InvalidField);
        }
        if effects.reads[..index]
            .iter()
            .any(|previous| previous.field == read.field)
        {
            return Err(PciFieldWriteTransactionError::DuplicateField);
        }
    }
    for (index, permission) in permissions.iter().enumerate() {
        if !valid_transaction_field(permission.field) {
            return Err(PciFieldWriteTransactionError::InvalidField);
        }
        if permissions[..index]
            .iter()
            .any(|previous| previous.field == permission.field)
        {
            return Err(PciFieldWriteTransactionError::DuplicateField);
        }
    }
    for write in &effects.writes {
        if !valid_transaction_field(write.field) {
            return Err(PciFieldWriteTransactionError::InvalidField);
        }
        if !permissions
            .iter()
            .any(|permission| permission.field == write.field && permission.allows(write.value))
        {
            return Err(PciFieldWriteTransactionError::PermissionDenied);
        }
    }

    for read in &effects.reads {
        let value = backend
            .read_byte(read.field)
            .map_err(PciFieldWriteTransactionError::SnapshotRead)?;
        if value != read.value {
            return Err(PciFieldWriteTransactionError::StaleSnapshot);
        }
    }

    let mut previous = Vec::with_capacity(effects.writes.len());
    for (index, write) in effects.writes.iter().enumerate() {
        let before = match backend.read_byte(write.field) {
            Ok(value) => value,
            Err(error) => {
                if !rollback_field_writes(backend, &previous) {
                    return Err(PciFieldWriteTransactionError::RollbackFailed);
                }
                return Err(PciFieldWriteTransactionError::Read(error));
            }
        };
        let expected = effects.writes[..index]
            .iter()
            .rev()
            .find(|prior| prior.field == write.field)
            .map(|prior| prior.value)
            .or_else(|| {
                effects
                    .reads
                    .iter()
                    .find(|read| read.field == write.field)
                    .map(|read| read.value)
            });
        if expected.is_some_and(|expected| expected != before) {
            if !rollback_field_writes(backend, &previous) {
                return Err(PciFieldWriteTransactionError::RollbackFailed);
            }
            return Err(PciFieldWriteTransactionError::ChangedDuringApply);
        }
        // Record before attempting the write: a backend may report failure after the device
        // accepted the byte, so rollback must include the uncertain operation itself.
        previous.push(PciFieldWrite {
            field: write.field,
            value: before,
        });
        if let Err(error) = backend.write_byte(write.field, write.value) {
            if !rollback_field_writes(backend, &previous) {
                return Err(PciFieldWriteTransactionError::RollbackFailed);
            }
            return Err(PciFieldWriteTransactionError::Write(error));
        }
        let readback = match backend.read_byte(write.field) {
            Ok(value) => value,
            Err(error) => {
                if !rollback_field_writes(backend, &previous) {
                    return Err(PciFieldWriteTransactionError::RollbackFailed);
                }
                return Err(PciFieldWriteTransactionError::Readback(error));
            }
        };
        if readback != write.value {
            if !rollback_field_writes(backend, &previous) {
                return Err(PciFieldWriteTransactionError::RollbackFailed);
            }
            return Err(PciFieldWriteTransactionError::ReadbackMismatch);
        }
    }

    if let Err(error) = postcondition() {
        if !rollback_field_writes(backend, &previous) {
            return Err(PciFieldWriteTransactionError::RollbackFailed);
        }
        return Err(PciFieldWriteTransactionError::Postcondition(error));
    }
    Ok(())
}
#[derive(Clone)]
enum Kind<'a> {
    Scope,
    Device,
    Data(Value),
    Alias(Path),
    Method {
        args: usize,
        body: &'a [u8],
    },
    Opaque(u16),
    Region {
        space: u8,
        base: Value,
        length: Value,
    },
    Field {
        region: Name,
        bit: u64,
        bits: usize,
        flags: u8,
    },
    External,
}
/// One resolved `Field` element: a PCI configuration byte or a region field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldLocation {
    /// A root-bus-zero PCI configuration byte, with its own provider and journal.
    Pci(PciField),
    /// A field inside a SystemMemory, SystemIo or EmbeddedControl operation region.
    Region {
        /// The field, bounds already checked against the region.
        field: RegionField,
        /// The region's base address.
        base: u64,
        /// The region's length in bytes.
        length: u64,
    },
}

#[derive(Clone)]
struct Object<'a> {
    path: Path,
    kind: Kind<'a>,
}
/// Opaque lexical context for a forward AML name; only the parser constructs these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NameReference(Name);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Name {
    path: Path,
    tail: usize,
    search: bool,
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }
    fn byte(&mut self) -> Result<u8, Error> {
        let b = *self.bytes.get(self.at).ok_or(Error::Truncated)?;
        self.at += 1;
        Ok(b)
    }
    fn peek(&self) -> Result<u8, Error> {
        self.bytes.get(self.at).copied().ok_or(Error::Truncated)
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).ok_or(Error::Limit)?;
        let b = self.bytes.get(self.at..end).ok_or(Error::Truncated)?;
        self.at = end;
        Ok(b)
    }
    fn length(&mut self) -> Result<usize, Error> {
        let lead = self.byte()?;
        let count = usize::from(lead >> 6);
        if count == 0 {
            return Ok(usize::from(lead & 63));
        }
        if lead & 0x30 != 0 {
            return Err(Error::Package);
        }
        let mut value = usize::from(lead & 15);
        for i in 0..count {
            value |= usize::from(self.byte()?) << (4 + 8 * i);
        }
        Ok(value)
    }
    fn package(&mut self) -> Result<Reader<'a>, Error> {
        let start = self.at;
        let length = self.length()?;
        let header = self.at - start;
        let body = length.checked_sub(header).ok_or(Error::Package)?;
        Ok(Reader::new(self.take(body)?))
    }
    fn name(&mut self, scope: &Path) -> Result<Name, Error> {
        let mut path = scope.clone();
        let mut search = true;
        if self.peek()? == b'\\' {
            self.byte()?;
            path = Path::root();
            search = false;
        }
        while self.peek()? == b'^' {
            self.byte()?;
            if path.0.pop().is_none() {
                return Err(Error::Name);
            }
            search = false;
        }
        let count = match self.peek()? {
            0 => {
                self.byte()?;
                0
            }
            0x2e => {
                self.byte()?;
                2
            }
            0x2f => {
                self.byte()?;
                let n = usize::from(self.byte()?);
                if n == 0 {
                    return Err(Error::Name);
                }
                n
            }
            _ => 1,
        };
        for _ in 0..count {
            let s: [u8; 4] = self.take(4)?.try_into().map_err(|_| Error::Name)?;
            check_segment(&s)?;
            path = path.child(s)?;
        }
        Ok(Name {
            path,
            tail: count,
            // ACPI parent search applies only to a single unprefixed NameSeg.
            search: search && count == 1,
        })
    }
}

/// One operation-region field write produced by bounded method planning.
///
/// Hardware is untouched by the evaluator: this is an intent, and
/// [`apply_region_field_writes`] applies it only with an exact permission and a verified
/// readback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionFieldWrite {
    /// The field written, as resolved against its region.
    pub field: RegionField,
    /// The value to write, already masked to the field's width.
    pub value: u64,
}

/// How many distinct values one region permission may authorize.
pub const MAX_REGION_VALUES: usize = 8;

/// Caller-declared allowed values for one exact region field.
///
/// Permissions are data rather than callbacks: planning cannot trigger hidden hardware effects,
/// and a field that is not listed here cannot be written at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionFieldWritePermission {
    /// Exact field authorized by this permission.
    pub field: RegionField,
    values: [u64; MAX_REGION_VALUES],
    count: usize,
}

impl RegionFieldWritePermission {
    /// Authorize the listed values for one field. An empty list denies every write.
    pub fn from_values(field: RegionField, allowed: &[u64]) -> Result<Self, Error> {
        if allowed.len() > MAX_REGION_VALUES {
            return Err(Error::Limit);
        }
        let mut values = [0u64; MAX_REGION_VALUES];
        values[..allowed.len()].copy_from_slice(allowed);
        Ok(Self {
            field,
            values,
            count: allowed.len(),
        })
    }

    fn allows(self, value: u64) -> bool {
        self.values[..self.count].contains(&value)
    }
}

/// Field-granular access used by a caller-owned region write transaction.
///
/// The backend sees the field's absolute address, width, shift and unit width, so it can build
/// the aligned unit accesses itself; it must restrict itself to the region the field belongs to
/// and serialize the transaction against other users of that region.
pub trait RegionFieldWriteBackend {
    /// Transport failure type.
    type Error;
    /// Read the field's current value.
    fn read_field(&mut self, field: RegionField) -> Result<u64, Self::Error>;
    /// Write the field's value.
    fn write_field(&mut self, field: RegionField, value: u64) -> Result<(), Self::Error>;
}

/// Failure while applying a region write journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionWriteTransactionError<BackendError> {
    /// The journal or permission list exceeded the bounded capacity.
    Limit,
    /// A write was not authorized by an exact permission.
    PermissionDenied,
    /// A field is not self-consistent.
    InvalidField,
    /// The same field appears twice in the journal or the permissions.
    DuplicateField,
    /// The backend could not read a field.
    Read(BackendError),
    /// The backend could not write a field.
    Write(BackendError),
    /// A readback did not return the value written.
    ReadbackMismatch,
    /// Restoring an attempted write did not verify.
    RollbackFailed,
}

fn valid_region_field(field: RegionField) -> bool {
    matches!(field.access, 1 | 2 | 4 | 8)
        && field.bits > 0
        && field.bits <= 64
        && u16::from(field.shift) + u16::from(field.bits) <= 64
        && field.address.is_multiple_of(u64::from(field.access))
}

fn restore_region_writes<B: RegionFieldWriteBackend>(
    backend: &mut B,
    previous: &[RegionFieldWrite],
) -> bool {
    let mut restored = true;
    for write in previous.iter().rev() {
        let _ = backend.write_field(write.field, write.value);
        if !matches!(backend.read_field(write.field), Ok(value) if value == write.value) {
            restored = false;
        }
    }
    restored
}

/// Apply authorized region field writes, verify each readback, and roll back on failure.
///
/// Every write is authorized and every field validated before the first backend operation, then
/// writes are applied in AML order. A write error, a readback mismatch or a failed rollback is
/// reported separately: the caller must quarantine the device rather than assume the region kept
/// its old contents.
pub fn apply_region_field_writes<B: RegionFieldWriteBackend>(
    effects: &MethodEffects,
    permissions: &[RegionFieldWritePermission],
    backend: &mut B,
) -> Result<(), RegionWriteTransactionError<B::Error>> {
    if effects.region_writes.len() > MAX_FIELD_WRITES
        || permissions.len() > MAX_FIELD_WRITE_PERMISSIONS
    {
        return Err(RegionWriteTransactionError::Limit);
    }
    for (index, permission) in permissions.iter().enumerate() {
        if !valid_region_field(permission.field) {
            return Err(RegionWriteTransactionError::InvalidField);
        }
        if permissions[..index]
            .iter()
            .any(|previous| previous.field == permission.field)
        {
            return Err(RegionWriteTransactionError::DuplicateField);
        }
    }
    for (index, write) in effects.region_writes.iter().enumerate() {
        if !valid_region_field(write.field) {
            return Err(RegionWriteTransactionError::InvalidField);
        }
        if effects.region_writes[..index]
            .iter()
            .any(|previous| previous.field == write.field)
        {
            return Err(RegionWriteTransactionError::DuplicateField);
        }
        if !permissions
            .iter()
            .any(|permission| permission.field == write.field && permission.allows(write.value))
        {
            return Err(RegionWriteTransactionError::PermissionDenied);
        }
    }

    let mut previous = Vec::with_capacity(effects.region_writes.len());
    for write in &effects.region_writes {
        let before = match backend.read_field(write.field) {
            Ok(value) => value,
            Err(error) => {
                if !restore_region_writes(backend, &previous) {
                    return Err(RegionWriteTransactionError::RollbackFailed);
                }
                return Err(RegionWriteTransactionError::Read(error));
            }
        };
        // Record before the write: a backend may report failure after the device accepted it, so
        // the uncertain operation must be part of the rollback too.
        previous.push(RegionFieldWrite {
            field: write.field,
            value: before,
        });
        if let Err(error) = backend.write_field(write.field, write.value) {
            if !restore_region_writes(backend, &previous) {
                return Err(RegionWriteTransactionError::RollbackFailed);
            }
            return Err(RegionWriteTransactionError::Write(error));
        }
        match backend.read_field(write.field) {
            Ok(value) if value == write.value => {}
            Ok(_) => {
                if !restore_region_writes(backend, &previous) {
                    return Err(RegionWriteTransactionError::RollbackFailed);
                }
                return Err(RegionWriteTransactionError::ReadbackMismatch);
            }
            Err(_) => {
                if !restore_region_writes(backend, &previous) {
                    return Err(RegionWriteTransactionError::RollbackFailed);
                }
                return Err(RegionWriteTransactionError::ReadbackMismatch);
            }
        }
    }
    Ok(())
}

/// A namespace whose method bytecode borrows validated DSDT/SSDT payloads.
#[derive(Clone)]
pub struct Namespace<'a> {
    objects: Vec<Object<'a>>,
    value_bytes: usize,
    integer_mask: u64,
    reader: Option<&'a dyn FieldReader>,
    regions: Option<&'a dyn RegionFieldReader>,
    /// The provider that applies journaled region-field writes to hardware. Absent means the
    /// writes stay intents and nothing is written.
    region_writes: Option<&'a dyn RegionFieldWriteBackend<Error = Error>>,
}
impl Default for Namespace<'_> {
    fn default() -> Self {
        Self::new()
    }
}
impl<'a> Namespace<'a> {
    /// Empty namespace with the ACPI predefined scope objects.
    pub fn new() -> Self {
        let mut objects = Vec::new();
        for s in [*b"_SB_", *b"_PR_", *b"_GPE", *b"_SI_", *b"_TZ_"] {
            objects.push(Object {
                path: Path(vec![s]),
                kind: Kind::Scope,
            });
        }
        Self {
            objects,
            value_bytes: 0,
            integer_mask: u64::MAX,
            reader: None,
            regions: None,
            region_writes: None,
        }
    }
    /// Install a read-only provider; ordinary evaluation without one still refuses fields.
    pub fn set_field_reader(&mut self, reader: &'a dyn FieldReader) {
        self.reader = Some(reader);
    }
    /// Install the provider that serves operation-region fields.
    ///
    /// Without one, a method that touches a SystemMemory, SystemIo or EmbeddedControl field is
    /// refused exactly as before; installing one authorizes the regions that provider answers
    /// for, and nothing else.
    pub fn set_region_reader(&mut self, regions: &'a dyn RegionFieldReader) {
        self.regions = Some(regions);
    }

    /// Install the provider that applies operation-region field writes.
    ///
    /// A method's region writes are journaled intents until a writer is installed: then they
    /// are applied through [`apply_region_field_writes`], exact-permission checked with a
    /// verified readback. Without a writer the intents are dropped and no hardware write
    /// occurs.
    pub fn set_region_writer(&mut self, writer: &'a dyn RegionFieldWriteBackend<Error = Error>) {
        self.region_writes = Some(writer);
    }

    /// Notify a supported firmware interrupt-model method. Only the complete side-effect-free
    /// shape Store(Arg0, named integer) is accepted; validation precedes the single mutation.
    /// Absence is distinct from failure. No operation-region/global hardware writes occur.
    pub fn select_interrupt_model(&mut self, apic: bool) -> Result<bool, Error> {
        let path = Path::parse("\\_PIC")?;
        let Some(index) = self.objects.iter().position(|o| o.path == path) else {
            return Ok(false);
        };
        let Kind::Method { args: 1, body } = self.objects[index].kind else {
            return Err(Error::Type);
        };
        let mut r = Reader::new(body);
        if r.byte()? != 0x70 || r.byte()? != 0x68 {
            return Err(Error::Target);
        }
        let name = r.name(&Path::root())?;
        if r.at != r.bytes.len() {
            return Err(Error::Target);
        }
        let target = self.resolve(&name)?;
        if !matches!(self.objects[target].kind, Kind::Data(Value::Integer(_))) {
            return Err(Error::Target);
        }
        self.objects[target].kind = Kind::Data(Value::Integer(u64::from(apic)));
        Ok(true)
    }
    /// Select the integer model inherited by every table: DSDT revision 0/1 uses 32 bits,
    /// revision 2 and later uses 64 bits. `new()` defaults to the latter model.
    pub fn with_dsdt_revision(revision: u8) -> Self {
        let mut namespace = Self::new();
        if revision < 2 {
            namespace.integer_mask = u64::from(u32::MAX);
        }
        namespace
    }
    /// Number of retained objects.
    pub fn len(&self) -> usize {
        self.objects.len()
    }
    /// Whether there are no objects (predefined scopes normally make this false).
    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }
    /// Device paths, without assuming their `_STA` result is usable.
    pub fn devices(&self) -> impl Iterator<Item = &Path> {
        self.objects
            .iter()
            .filter_map(|o| matches!(o.kind, Kind::Device).then_some(&o.path))
    }
    fn insert(&mut self, path: Path, kind: Kind<'a>) -> Result<(), Error> {
        if path.0.is_empty() {
            return Err(Error::Name);
        }
        let parent = path.parent();
        if !parent.0.is_empty()
            && !self
                .objects
                .iter()
                .any(|o| o.path == parent && matches!(o.kind, Kind::Scope | Kind::Device))
        {
            return Err(Error::Missing);
        }
        let cost = match &kind {
            Kind::Data(value) => value_cost(value, 0)?,
            Kind::Region { base, length, .. } => value_cost(base, 0)?
                .checked_add(value_cost(length, 0)?)
                .ok_or(Error::Limit)?,
            _ => 0,
        };
        let total = self.value_bytes.checked_add(cost).ok_or(Error::Limit)?;
        if total > MAX_NAMESPACE_BYTES {
            return Err(Error::Limit);
        }
        if let Some(old) = self.objects.iter_mut().find(|o| o.path == path) {
            if matches!(old.kind, Kind::External) {
                old.kind = kind;
                self.value_bytes = total;
                return Ok(());
            }
            return Err(Error::Duplicate);
        }
        if self.objects.len() >= MAX_OBJECTS {
            return Err(Error::Limit);
        }
        self.objects.push(Object { path, kind });
        self.value_bytes = total;
        Ok(())
    }
    fn alias_target(&self, mut index: usize) -> Result<usize, Error> {
        for _ in 0..MAX_DEPTH {
            let Kind::Alias(path) = &self.objects[index].kind else {
                return Ok(index);
            };
            index = self
                .objects
                .iter()
                .position(|o| &o.path == path)
                .ok_or(Error::Missing)?;
        }
        Err(Error::Limit)
    }
    fn resolve(&self, name: &Name) -> Result<usize, Error> {
        let mut path = name.path.clone();
        loop {
            if let Some(index) = self.objects.iter().position(|o| o.path == path) {
                return self.alias_target(index);
            }
            if !name.search || path.0.len() <= name.tail {
                return Err(Error::Missing);
            }
            path.0.remove(path.0.len() - name.tail - 1);
        }
    }
    /// Atomically load an AML payload, excluding its 36-byte ACPI SDT header.
    /// Unknown module-level instructions refuse the entire table, restoring prior objects.
    pub fn load(&mut self, bytes: &'a [u8]) -> Result<(), Error> {
        if bytes.len() > 1024 * 1024 {
            return Err(Error::Limit);
        }
        let saved = self.objects.clone();
        let saved_bytes = self.value_bytes;
        let mut budget = MAX_STEPS;
        let result = self.terms(&mut Reader::new(bytes), &Path::root(), 0, &mut budget);
        if result.is_err() {
            self.objects = saved;
            self.value_bytes = saved_bytes;
        }
        result
    }
    fn terms(
        &mut self,
        r: &mut Reader<'a>,
        scope: &Path,
        depth: usize,
        steps: &mut usize,
    ) -> Result<(), Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        while r.at < r.bytes.len() {
            step(steps)?;
            let op = r.byte()?;
            match op {
                0x08 => {
                    let name = r.name(scope)?.path;
                    let v = self.data(r, scope, depth + 1, steps)?;
                    self.insert(name, Kind::Data(v))?;
                }
                0x06 => {
                    let source = r.name(scope)?;
                    let index = self.resolve(&source)?;
                    let path = self.objects[index].path.clone();
                    let name = r.name(scope)?.path;
                    self.insert(name, Kind::Alias(path))?;
                }
                0x10 => {
                    let mut p = r.package()?;
                    let target = p.name(scope)?;
                    let name = if target.path.0.is_empty() {
                        Path::root()
                    } else {
                        let index = self.resolve(&target)?;
                        if !matches!(self.objects[index].kind, Kind::Scope | Kind::Device) {
                            return Err(Error::Type);
                        }
                        self.objects[index].path.clone()
                    };
                    self.terms(&mut p, &name, depth + 1, steps)?;
                }
                0x14 => {
                    let mut p = r.package()?;
                    let name = p.name(scope)?.path;
                    let flags = p.byte()?;
                    self.insert(
                        name,
                        Kind::Method {
                            args: usize::from(flags & 7),
                            body: &p.bytes[p.at..],
                        },
                    )?;
                }
                0x15 => {
                    let name = r.name(scope)?.path;
                    r.take(2)?;
                    if !self.objects.iter().any(|o| o.path == name) {
                        self.insert(name, Kind::External)?;
                    }
                }
                0x5b => {
                    let ext = r.byte()?;
                    match ext {
                        0x82..=0x85 => {
                            let mut p = r.package()?;
                            let name = p.name(scope)?.path;
                            if ext == 0x83 {
                                p.take(6)?;
                            } else if ext == 0x84 {
                                p.take(3)?;
                            }
                            self.insert(
                                name.clone(),
                                if ext == 0x82 {
                                    Kind::Device
                                } else {
                                    Kind::Scope
                                },
                            )?;
                            self.terms(&mut p, &name, depth + 1, steps)?;
                        }
                        0x01 | 0x02 => {
                            let name = r.name(scope)?.path;
                            if ext == 1 {
                                r.byte()?;
                            }
                            self.insert(name, Kind::Opaque(0x5b00 | u16::from(ext)))?;
                        }
                        0x80 => {
                            let name = r.name(scope)?.path;
                            let space = r.byte()?;
                            let base = self.data(r, scope, depth + 1, steps)?;
                            let length = self.data(r, scope, depth + 1, steps)?;
                            // Region operands are retained only as bounded integers/references.
                            if !matches!(
                                base,
                                Value::Integer(_)
                                    | Value::Reference(_)
                                    | Value::DeferredReference(_)
                            ) || !matches!(
                                length,
                                Value::Integer(_)
                                    | Value::Reference(_)
                                    | Value::DeferredReference(_)
                            ) {
                                return Err(Error::Type);
                            }
                            self.insert(
                                name,
                                Kind::Region {
                                    space,
                                    base,
                                    length,
                                },
                            )?;
                        }
                        0x81 | 0x86 | 0x87 => {
                            let mut p = r.package()?;
                            let region = p.name(scope)?;
                            if ext != 0x81 {
                                p.name(scope)?;
                            }
                            if ext == 0x87 {
                                self.data(&mut p, scope, depth + 1, steps)?;
                            }
                            let flags = p.byte()?;
                            self.fields(
                                &mut p,
                                scope,
                                steps,
                                if ext == 0x81 { Some(region) } else { None },
                                flags,
                            )?;
                        }
                        _ => return Err(Error::Unsupported(0x5b00 | u16::from(ext))),
                    }
                }
                0xa3 => {}
                _ => return Err(Error::Unsupported(u16::from(op))),
            }
        }
        Ok(())
    }
    fn fields(
        &mut self,
        r: &mut Reader<'a>,
        scope: &Path,
        steps: &mut usize,
        region: Option<Name>,
        mut flags: u8,
    ) -> Result<(), Error> {
        let mut bit = 0u64;
        while r.at < r.bytes.len() {
            step(steps)?;
            match r.peek()? {
                0 => {
                    r.byte()?;
                    bit = bit.checked_add(r.length()? as u64).ok_or(Error::Limit)?;
                }
                1 => {
                    let access = r.take(3)?;
                    flags = (flags & !15) | (access[1] & 15);
                }
                3 => {
                    r.take(4)?;
                    flags = 0xff;
                }
                2 => return Err(Error::Unsupported(0x5b81)),
                _ => {
                    let s: [u8; 4] = r.take(4)?.try_into().map_err(|_| Error::Name)?;
                    check_segment(&s)?;
                    let bits = r.length()?;
                    self.insert(
                        scope.child(s)?,
                        match &region {
                            Some(region) => Kind::Field {
                                region: region.clone(),
                                bit,
                                bits,
                                flags,
                            },
                            None => Kind::Opaque(0x5b81),
                        },
                    )?;
                    bit = bit.checked_add(bits as u64).ok_or(Error::Limit)?;
                }
            }
        }
        Ok(())
    }
    fn data(
        &self,
        r: &mut Reader<'a>,
        scope: &Path,
        depth: usize,
        steps: &mut usize,
    ) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        step(steps)?;
        let op = r.byte()?;
        match op {
            0 => Ok(Value::Integer(0)),
            1 => Ok(Value::Integer(1)),
            0xff => Ok(Value::Integer(self.integer_mask)),
            0x0a | 0x0b | 0x0c | 0x0e => {
                let n = match op {
                    0x0a => 1,
                    0x0b => 2,
                    0x0c => 4,
                    _ => 8,
                };
                let b = r.take(n)?;
                Ok(Value::Integer(
                    b.iter()
                        .enumerate()
                        .fold(0, |v, (i, b)| v | (u64::from(*b) << (i * 8)))
                        & self.integer_mask,
                ))
            }
            0x0d => {
                let mut s = Vec::new();
                loop {
                    let b = r.byte()?;
                    if b == 0 {
                        break;
                    }
                    if s.len() >= MAX_VALUE_BYTES {
                        return Err(Error::Limit);
                    }
                    s.push(b);
                }
                Ok(Value::String(s))
            }
            0x11 => {
                let mut p = r.package()?;
                let size = self.data(&mut p, scope, depth + 1, steps)?;
                let n = self.constant(size, depth + 1)?.integer()?;
                let n = usize::try_from(n).map_err(|_| Error::Limit)?;
                if n > MAX_VALUE_BYTES {
                    return Err(Error::Limit);
                }
                let mut bytes = vec![0; n];
                let len = n.min(p.bytes.len() - p.at);
                bytes[..len].copy_from_slice(&p.bytes[p.at..p.at + len]);
                Ok(Value::Buffer(bytes))
            }
            0x12 | 0x13 => {
                let mut p = r.package()?;
                let count = if op == 0x12 {
                    u64::from(p.byte()?)
                } else {
                    self.constant(self.data(&mut p, scope, depth + 1, steps)?, depth + 1)?
                        .integer()?
                };
                if count > MAX_PACKAGE as u64 {
                    return Err(Error::Limit);
                }
                let mut values = Vec::new();
                let mut cost = core::mem::size_of::<Value>();
                for _ in 0..count {
                    let value = if p.at == p.bytes.len() {
                        Value::Uninitialized
                    } else {
                        self.data(&mut p, scope, depth + 1, steps)?
                    };
                    cost = cost
                        .checked_add(value_cost(&value, 0)?)
                        .ok_or(Error::Limit)?;
                    if cost > MAX_DATA_BYTES {
                        return Err(Error::Limit);
                    }
                    values.push(value);
                }
                if p.at != p.bytes.len() {
                    return Err(Error::Package);
                }
                let value = Value::Package(values);
                value_cost(&value, 0)?;
                Ok(value)
            }
            b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_' => {
                r.at -= 1;
                let name = r.name(scope)?;
                match self.resolve(&name) {
                    Ok(index) => Ok(Value::Reference(self.objects[index].path.clone())),
                    Err(Error::Missing) => Ok(Value::DeferredReference(NameReference(name))),
                    Err(error) => Err(error),
                }
            }
            _ => Err(Error::Unsupported(u16::from(op))),
        }
    }
    fn constant(&self, value: Value, depth: usize) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        match value {
            Value::Integer(value) => Ok(Value::Integer(value & self.integer_mask)),
            Value::DeferredReference(NameReference(name)) => {
                let index = self.resolve(&name)?;
                self.constant(
                    Value::Reference(self.objects[index].path.clone()),
                    depth + 1,
                )
            }
            Value::Reference(path) => {
                let index = self
                    .objects
                    .iter()
                    .position(|o| o.path == path)
                    .ok_or(Error::Missing)?;
                let index = self.alias_target(index)?;
                match &self.objects[index].kind {
                    Kind::Data(v) => self.constant(v.clone(), depth + 1),
                    Kind::External => Err(Error::Missing),
                    _ => Err(Error::Type),
                }
            }
            Value::Package(values) => Ok(Value::Package(
                values
                    .into_iter()
                    .map(|v| self.package_reference(v, depth + 1))
                    .collect::<Result<_, _>>()?,
            )),
            _ => Ok(value),
        }
    }
    fn package_reference(&self, value: Value, depth: usize) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        match value {
            Value::DeferredReference(NameReference(name)) => {
                let index = self.resolve(&name)?;
                Ok(Value::Reference(self.objects[index].path.clone()))
            }
            Value::Package(values) => Ok(Value::Package(
                values
                    .into_iter()
                    .map(|v| self.package_reference(v, depth + 1))
                    .collect::<Result<_, _>>()?,
            )),
            _ => Ok(value),
        }
    }
    /// Evaluate a name or invoke a method with an explicit argument list and a shared budget.
    /// No operation-region, I/O, PCI, global-store or synchronization side effects are allowed.
    pub fn evaluate(&self, path: &Path, args: &[Value]) -> Result<Value, Error> {
        let index = self
            .objects
            .iter()
            .position(|o| &o.path == path)
            .ok_or(Error::Missing)?;
        let mut steps = MAX_STEPS;
        let mut effects = EvaluationEffects::read_only();
        self.object(index, args, 0, &mut steps, &mut effects)
    }
    /// Execute a device method in the bounded interpreter and return only explicitly permitted
    /// PCI-config byte-write intents. This never writes hardware or publishes a route. Field
    /// writes are denied unless the caller supplies an exact field/value permission.
    pub(crate) fn plan_device_method(
        &self,
        path: &Path,
        member: [u8; 4],
        args: &[Value],
        permissions: &[PciFieldWritePermission],
    ) -> Result<MethodEffects, Error> {
        self.plan_device_method_with_regions(path, member, args, permissions, &[])
    }

    /// As [`Namespace::plan_device_method`], and additionally authorize exact region writes.
    pub(crate) fn plan_device_method_with_regions(
        &self,
        path: &Path,
        member: [u8; 4],
        args: &[Value],
        permissions: &[PciFieldWritePermission],
        region_permissions: &[RegionFieldWritePermission],
    ) -> Result<MethodEffects, Error> {
        if !self
            .objects
            .iter()
            .any(|o| &o.path == path && matches!(o.kind, Kind::Device))
        {
            return Err(Error::Type);
        }
        check_segment(&member)?;
        if permissions.len() > MAX_FIELD_WRITE_PERMISSIONS
            || region_permissions.len() > MAX_FIELD_WRITE_PERMISSIONS
        {
            return Err(Error::Limit);
        }
        for (index, permission) in permissions.iter().enumerate() {
            if permission.field.bytes != 1
                || permission.field.offset >= 256
                || permission.field.address & !0x1f0007 != 0
            {
                return Err(Error::Type);
            }
            if permissions[..index]
                .iter()
                .any(|previous| previous.field == permission.field)
            {
                return Err(Error::Duplicate);
            }
        }
        let method = path.child(member)?;
        let index = self
            .objects
            .iter()
            .position(|object| object.path == method)
            .ok_or(Error::Missing)?;
        if !matches!(self.objects[index].kind, Kind::Method { .. }) {
            return Err(Error::Type);
        }
        validate_arguments(args)?;
        let mut steps = MAX_STEPS;
        step(&mut steps)?;
        let mut effects =
            EvaluationEffects::with_region_permissions(permissions, region_permissions);
        let result = self.run_method(index, args, 0, &mut steps, &mut effects)?;
        Ok(MethodEffects {
            result,
            reads: effects.reads,
            writes: effects.writes,
            region_reads: effects.region_reads,
            region_writes: effects.region_writes,
        })
    }
    /// Evaluate a device member, distinguishing an absent member from failure inside one.
    pub fn device_value(&self, path: &Path, member: [u8; 4]) -> Result<Option<Value>, Error> {
        if !self
            .objects
            .iter()
            .any(|o| &o.path == path && matches!(o.kind, Kind::Device))
        {
            return Err(Error::Type);
        }
        check_segment(&member)?;
        let member = path.child(member)?;
        if !self.objects.iter().any(|o| o.path == member) {
            return Ok(None);
        }
        self.evaluate(&member, &[]).map(Some)
    }
    /// Inspect a device member's method argument count without invoking it.
    /// An absent member is `Ok(None)`; a present non-method member is a type error.
    pub fn device_method_arity(
        &self,
        path: &Path,
        member: [u8; 4],
    ) -> Result<Option<usize>, Error> {
        if !self
            .objects
            .iter()
            .any(|o| &o.path == path && matches!(o.kind, Kind::Device))
        {
            return Err(Error::Type);
        }
        check_segment(&member)?;
        let member = path.child(member)?;
        let Some(object) = self.objects.iter().find(|o| o.path == member) else {
            return Ok(None);
        };
        match &object.kind {
            Kind::Method { args, .. } => Ok(Some(*args)),
            _ => Err(Error::Type),
        }
    }
    /// Evaluate a device's `_STA`, using the ACPI default 0x0f only when the member
    /// genuinely does not exist. A failing method is never treated as an absent member.
    /// The boolean distinguishes the default from a computed result.
    pub fn device_status(&self, path: &Path) -> Result<(u32, bool), Error> {
        if !self
            .objects
            .iter()
            .any(|o| &o.path == path && matches!(o.kind, Kind::Device))
        {
            return Err(Error::Type);
        }
        let member = path.child(*b"_STA")?;
        if !self.objects.iter().any(|o| o.path == member) {
            return Ok((15, true));
        }
        let value = self.evaluate(&member, &[])?.integer()?;
        Ok((u32::try_from(value).map_err(|_| Error::Type)?, false))
    }
    /// Evaluate `_CRS` as buffer bytes. Missing members are distinct from failures inside
    /// an existing method. Resource decoding remains a separate, strict operation.
    pub fn device_resources(&self, path: &Path) -> Result<Option<Vec<u8>>, Error> {
        if !self
            .objects
            .iter()
            .any(|o| &o.path == path && matches!(o.kind, Kind::Device))
        {
            return Err(Error::Type);
        }
        let member = path.child(*b"_CRS")?;
        if !self.objects.iter().any(|o| o.path == member) {
            return Ok(None);
        }
        match self.evaluate(&member, &[])? {
            Value::Buffer(bytes) => Ok(Some(bytes)),
            _ => Err(Error::Type),
        }
    }
    fn field_metadata(
        &self,
        owner: &Path,
        member: [u8; 4],
        depth: usize,
    ) -> Result<Option<Value>, Error> {
        let path = owner.child(member)?;
        let Some(object) = self.objects.iter().find(|o| o.path == path) else {
            return Ok(None);
        };
        let Kind::Data(value) = &object.kind else {
            return Err(Error::Unsupported(0x5b81));
        };
        self.constant(value.clone(), depth + 1).map(Some)
    }
    /// Where one `Field` element lives, resolved against its operation region.
    fn field_location(
        &self,
        region: &Name,
        bit: u64,
        bits: usize,
        flags: u8,
        depth: usize,
    ) -> Result<FieldLocation, Error> {
        let index = self.resolve(region)?;
        let object = &self.objects[index];
        let Kind::Region {
            space,
            base,
            length,
        } = &object.kind
        else {
            return Err(Error::Unsupported(0x5b80));
        };
        let base = self.constant(base.clone(), depth + 1)?.integer()?;
        let length = self.constant(length.clone(), depth + 1)?.integer()?;
        if base.checked_add(length).is_none() {
            return Err(Error::Limit);
        }
        if *space != 2 {
            return Self::region_field(*space, base, length, bit, bits, flags);
        }
        if bits != 8 || !bit.is_multiple_of(8) || flags != 1 {
            return Err(Error::Unsupported(0x5b81));
        }
        let offset = bit / 8;
        if offset >= length {
            return Err(Error::Limit);
        }
        let offset = base.checked_add(offset).ok_or(Error::Limit)?;
        if offset >= 256 {
            return Err(Error::Limit);
        }
        let owner = object.path.parent();
        let address = self
            .field_metadata(&owner, *b"_ADR", depth)?
            .ok_or(Error::Missing)?
            .integer()?;
        if address & !0x1f0007 != 0 {
            return Err(Error::Type);
        }
        // Only a device immediately under a segment-zero/base-bus-zero root is supported.
        let root = owner.parent();
        let id = self
            .field_metadata(&root, *b"_HID", depth)?
            .ok_or(Error::Missing)?;
        if !matches!(id, Value::Integer(0x030ad041 | 0x080ad041))
            && !matches!(&id,Value::String(v) if v==b"PNP0A03" || v==b"PNP0A08")
        {
            return Err(Error::Type);
        }
        for member in [*b"_SEG", *b"_BBN"] {
            if let Some(value) = self.field_metadata(&root, member, depth)? {
                if value.integer()? != 0 {
                    return Err(Error::Type);
                }
            }
        }
        Ok(FieldLocation::Pci(PciField {
            address: address as u32,
            offset: offset as u16,
            bytes: 1,
        }))
    }

    /// Resolve a field in a SystemMemory, SystemIo or EmbeddedControl region.
    ///
    /// The access width comes from the field's own flags: `AnyAcc` is read one byte at a time,
    /// which is always valid, and `ByteAcc`/`WordAcc`/`DWordAcc`/`QWordAcc` fix it at 1, 2, 4 or
    /// 8 bytes. A wider unit must start on its own alignment, because that is what the firmware
    /// assumes when it declares one. The lock rule is *not* refused: this crate never takes the
    /// ACPI global lock, so a `Lock` field is left to the provider to serialize, and the update
    /// rule only affects writes, which are refused below. Everything is bounded to the region's
    /// own `base`/`length` before a provider sees an address.
    fn region_field(
        space: u8,
        base: u64,
        length: u64,
        bit: u64,
        bits: usize,
        flags: u8,
    ) -> Result<FieldLocation, Error> {
        let space = match space {
            0 => RegionSpace::SystemMemory,
            1 => RegionSpace::SystemIo,
            3 => RegionSpace::EmbeddedControl,
            _ => return Err(Error::Unsupported(0x5b80)),
        };
        if bits == 0 || bits > 64 {
            return Err(Error::Unsupported(0x5b81));
        }
        let access = match flags & 0x0f {
            0 => 1u8,
            1 => 1,
            2 => 2,
            3 => 4,
            4 => 8,
            _ => return Err(Error::Unsupported(0x5b81)),
        };
        let first = bit / 8;
        let shift = bit % 8;
        let bytes = (shift + bits as u64).div_ceil(8);
        let last = first.checked_add(bytes).ok_or(Error::Limit)?;
        if last > length {
            return Err(Error::Limit);
        }
        let address = base.checked_add(first).ok_or(Error::Limit)?;
        if address % u64::from(access) != 0 {
            return Err(Error::Unsupported(0x5b81));
        }
        // Every aligned unit the field touches must stay inside the region, not just the field.
        let units = last
            .div_ceil(u64::from(access))
            .checked_mul(u64::from(access))
            .ok_or(Error::Limit)?;
        if units > length {
            return Err(Error::Limit);
        }
        Ok(FieldLocation::Region {
            field: RegionField {
                space,
                address,
                bits: bits as u8,
                shift: shift as u8,
                access,
            },
            base,
            length,
        })
    }

    /// Read one region field through the caller's provider.
    fn read_region(
        &self,
        provider: &dyn RegionFieldReader,
        field: &RegionField,
        base: u64,
        length: u64,
    ) -> Result<u64, Error> {
        let access = u64::from(field.access);
        let first = field.address.checked_sub(base).ok_or(Error::Limit)?;
        let bytes = (u64::from(field.shift) + u64::from(field.bits)).div_ceil(8);
        let last = first.checked_add(bytes).ok_or(Error::Limit)?;
        if first % access != 0 || last > length {
            return Err(Error::Limit);
        }
        let mut raw = 0u128;
        let mut at = first;
        while at < last {
            let unit = at.checked_add(access).ok_or(Error::Limit)?;
            if unit > length {
                return Err(Error::Limit);
            }
            let value = provider.read(field.space, base + at, field.access)?;
            if field.access < 8 && value >> (u32::from(field.access) * 8) != 0 {
                return Err(Error::Type);
            }
            raw |= u128::from(value) << ((at - first) * 8);
            at = unit;
        }
        let mask = if field.bits == 64 {
            u64::MAX
        } else {
            (1u64 << field.bits) - 1
        };
        let value = (raw >> field.shift) as u64 & mask;
        Ok(value & self.integer_mask)
    }

    fn field(
        &self,
        region: &Name,
        bit: u64,
        bits: usize,
        flags: u8,
        depth: usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<Value, Error> {
        let location = self.field_location(region, bit, bits, flags, depth)?;
        let field = match location {
            FieldLocation::Pci(field) => field,
            FieldLocation::Region {
                field,
                base,
                length,
            } => {
                let provider = self.regions.ok_or(Error::Unsupported(0x5b81))?;
                let value = self.read_region(provider, &field, base, length)?;
                if effects.region_reads.len() >= MAX_FIELD_READS {
                    return Err(Error::Limit);
                }
                effects.region_reads.push(RegionFieldRead { field, value });
                return Ok(Value::Integer(value));
            }
        };
        if let Some(write) = effects
            .writes
            .iter()
            .rev()
            .find(|write| write.field == field)
        {
            return Ok(Value::Integer(u64::from(write.value)));
        }
        let reader = self.reader.ok_or(Error::Unsupported(0x5b81))?;
        let value = (reader.read(field)? & 255) as u8;
        if effects.planning() {
            if let Some(previous) = effects.reads.iter().find(|read| read.field == field) {
                if previous.value != value {
                    return Err(Error::WriteDenied);
                }
            } else {
                if effects.reads.len() >= MAX_FIELD_READS {
                    return Err(Error::Limit);
                }
                effects.reads.push(PciFieldRead { field, value });
            }
        }
        Ok(Value::Integer(u64::from(value)))
    }
    fn object(
        &self,
        index: usize,
        args: &[Value],
        depth: usize,
        steps: &mut usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        step(steps)?;
        validate_arguments(args)?;
        let o = &self.objects[index];
        match &o.kind {
            Kind::Alias(path) => {
                let target = self
                    .objects
                    .iter()
                    .position(|o| &o.path == path)
                    .ok_or(Error::Missing)?;
                self.object(target, args, depth + 1, steps, effects)
            }
            Kind::Data(v) if args.is_empty() => self.constant(v.clone(), depth + 1),
            Kind::Method { args: expected, .. } if *expected == args.len() => self
                .run_method(index, args, depth, steps, effects)?
                .ok_or(Error::NoReturn),
            Kind::Field {
                region,
                bit,
                bits,
                flags,
            } => self.field(region, *bit, *bits, *flags, depth + 1, effects),
            Kind::Region { .. } => Err(Error::Unsupported(0x5b80)),
            Kind::Opaque(op) => Err(Error::Unsupported(*op)),
            Kind::External => Err(Error::Missing),
            _ => Err(Error::Type),
        }
    }
    fn run_method(
        &self,
        index: usize,
        args: &[Value],
        depth: usize,
        steps: &mut usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<Option<Value>, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        let object = &self.objects[index];
        let Kind::Method {
            args: expected,
            body,
        } = &object.kind
        else {
            return Err(Error::Type);
        };
        if *expected != args.len() {
            return Err(Error::Type);
        }
        let mut frame = Frame {
            scope: object.path.parent(),
            locals: core::array::from_fn(|_| Value::Uninitialized),
            names: Vec::new(),
            fields: Vec::new(),
            args: core::array::from_fn(|i| match args.get(i) {
                Some(Value::Integer(value)) => Value::Integer(value & self.integer_mask),
                Some(value) => value.clone(),
                None => Value::Uninitialized,
            }),
        };
        self.execute(
            &mut Reader::new(body),
            &mut frame,
            depth + 1,
            steps,
            effects,
        )
    }
    fn expr(
        &self,
        r: &mut Reader<'a>,
        f: &mut Frame,
        depth: usize,
        steps: &mut usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        step(steps)?;
        let op = r.peek()?;
        match op {
            0x60..=0x67 => {
                r.byte()?;
                Ok(f.locals[usize::from(op - 0x60)].clone())
            }
            0x68..=0x6e => {
                r.byte()?;
                Ok(f.args[usize::from(op - 0x68)].clone())
            }
            0x70 => {
                r.byte()?;
                let v = self.expr(r, f, depth + 1, steps, effects)?;
                self.target(r, f, v.clone(), depth + 1, steps, effects)?;
                Ok(v)
            }
            0x75 => {
                r.byte()?;
                let local = r.byte()?;
                if !(0x60..=0x67).contains(&local) {
                    return Err(Error::Target);
                }
                let index = usize::from(local - 0x60);
                let value =
                    Value::Integer(f.locals[index].integer()?.wrapping_add(1) & self.integer_mask);
                f.locals[index] = value.clone();
                Ok(value)
            }
            0x72 | 0x74 | 0x77 | 0x79..=0x80 | 0x90..=0x95 => {
                r.byte()?;
                let a = self.expr(r, f, depth + 1, steps, effects)?.integer()?;
                let unary = op == 0x80 || op == 0x92;
                let b = if unary {
                    0
                } else {
                    self.expr(r, f, depth + 1, steps, effects)?.integer()?
                };
                let value = match op {
                    0x72 => a.wrapping_add(b),
                    0x74 => a.wrapping_sub(b),
                    0x77 => a.wrapping_mul(b),
                    0x79 => {
                        if b < 64 {
                            a << b
                        } else {
                            0
                        }
                    }
                    0x7a => {
                        if b < 64 {
                            a >> b
                        } else {
                            0
                        }
                    }
                    0x7b => a & b,
                    0x7c => !(a & b),
                    0x7d => a | b,
                    0x7e => !(a | b),
                    0x7f => a ^ b,
                    0x80 => !a,
                    0x90 => truth(a != 0 && b != 0),
                    0x91 => truth(a != 0 || b != 0),
                    0x92 => truth(a == 0),
                    0x93 => truth(a == b),
                    0x94 => truth(a > b),
                    0x95 => truth(a < b),
                    _ => return Err(Error::Unsupported(u16::from(op))),
                };
                let v = Value::Integer(value & self.integer_mask);
                if op < 0x90 {
                    self.target(r, f, v.clone(), depth + 1, steps, effects)?;
                }
                Ok(v)
            }
            b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_' => {
                let name = r.name(&f.scope)?;
                if let Some(index) = f.names.iter().position(|(p, _)| *p == name.path) {
                    return Ok(f.names[index].1.clone());
                }
                if let Some(field) = f.fields.iter().find(|field| field.name == name.path) {
                    let bytes = f.buffer(field.source)?;
                    return Ok(Value::Integer(u64::from(u32::from_le_bytes(
                        bytes[field.offset..field.offset + 4]
                            .try_into()
                            .map_err(|_| Error::Type)?,
                    ))));
                }
                let index = self.resolve(&name)?;
                let count = if let Kind::Method { args, .. } = self.objects[index].kind {
                    args
                } else {
                    0
                };
                let mut args = Vec::new();
                for _ in 0..count {
                    args.push(self.expr(r, f, depth + 1, steps, effects)?);
                }
                self.object(index, &args, depth + 1, steps, effects)
            }
            _ => {
                let v = self.data(r, &f.scope, depth + 1, steps)?;
                self.constant(v, depth + 1)
            }
        }
    }
    fn target(
        &self,
        r: &mut Reader<'a>,
        f: &mut Frame,
        value: Value,
        depth: usize,
        steps: &mut usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<(), Error> {
        if r.peek()? == 0x88 {
            step(steps)?;
            r.byte()?;
            let source = r.byte()?;
            if !(0x60..=0x67).contains(&source) {
                return Err(Error::Target);
            }
            let index = usize::try_from(self.expr(r, f, depth + 1, steps, effects)?.integer()?)
                .map_err(|_| Error::Limit)?;
            if r.byte()? != 0 || !matches!(value, Value::Integer(_) | Value::Package(_)) {
                return Err(Error::Target);
            }
            let local = usize::from(source - 0x60);
            let mut replacement = f.locals[local].clone();
            let Value::Package(values) = &mut replacement else {
                return Err(Error::Target);
            };
            *values.get_mut(index).ok_or(Error::Limit)? = value;
            frame_local(f, local, replacement)?;
            return Ok(());
        }
        if matches!(value, Value::Package(_)) {
            let local = r.byte()?;
            if !(0x60..=0x67).contains(&local) {
                return Err(Error::Target);
            }
            return frame_local(f, usize::from(local - 0x60), value);
        }
        if matches!(r.peek()?, b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_') {
            let target_at = r.at;
            let name = r.name(&f.scope)?;
            if !f.fields.iter().any(|field| field.name == name.path)
                && !f.names.iter().any(|(path, _)| *path == name.path)
            {
                let index = match self.resolve(&name) {
                    Ok(index) => Some(index),
                    Err(Error::Missing) => None,
                    Err(error) => return Err(error),
                };
                if let Some(index) = index {
                    match &self.objects[index].kind {
                        Kind::Field {
                            region,
                            bit,
                            bits,
                            flags,
                        } => {
                            let location =
                                self.field_location(region, *bit, *bits, *flags, depth + 1)?;
                            let field = match location {
                                FieldLocation::Pci(field) => field,
                                FieldLocation::Region { field, .. } => {
                                    // A region write needs its own exact permission; without
                                    // one the method is refused, never silently ignored.
                                    let mask = if field.bits == 64 {
                                        u64::MAX
                                    } else {
                                        (1u64 << field.bits) - 1
                                    };
                                    let value = value.integer()? & mask;
                                    let allowed =
                                        effects.region_permissions.is_some_and(|permissions| {
                                            permissions.iter().any(|permission| {
                                                permission.field == field
                                                    && permission.allows(value)
                                            })
                                        });
                                    if !allowed {
                                        return Err(Error::WriteDenied);
                                    }
                                    if effects.region_writes.len() >= MAX_FIELD_WRITES {
                                        return Err(Error::Limit);
                                    }
                                    effects
                                        .region_writes
                                        .push(RegionFieldWrite { field, value });
                                    return Ok(());
                                }
                            };
                            let value =
                                u8::try_from(value.integer()? & 0xff).map_err(|_| Error::Type)?;
                            let allowed = effects.permissions.is_some_and(|permissions| {
                                permissions.iter().any(|permission| {
                                    permission.field == field && permission.allows(value)
                                })
                            });
                            if !allowed {
                                return Err(Error::WriteDenied);
                            }
                            if effects.writes.len() >= MAX_FIELD_WRITES {
                                return Err(Error::Limit);
                            }
                            effects.writes.push(PciFieldWrite { field, value });
                            return Ok(());
                        }
                        Kind::Opaque(0x5b81) => return Err(Error::Unsupported(0x5b81)),
                        _ => {}
                    }
                }
            }
            r.at = target_at;
        }
        target(r, f, value)
    }
    fn execute(
        &self,
        r: &mut Reader<'a>,
        f: &mut Frame,
        depth: usize,
        steps: &mut usize,
        effects: &mut EvaluationEffects<'_>,
    ) -> Result<Option<Value>, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        while r.at < r.bytes.len() {
            step(steps)?;
            match r.peek()? {
                8 => {
                    r.byte()?;
                    let name = r.name(&f.scope)?;
                    if !name.search
                        || name.tail != 1
                        || f.names.len() >= 16
                        || f.names.iter().any(|(p, _)| *p == name.path)
                        || f.fields.iter().any(|v| v.name == name.path)
                    {
                        return Err(Error::Target);
                    }
                    let value = self.data(r, &f.scope, depth + 1, steps)?;
                    if f.names
                        .iter()
                        .try_fold(value_cost(&value, 0)?, |cost, (_, v)| {
                            cost.checked_add(value_cost(v, 0)?).ok_or(Error::Limit)
                        })?
                        > MAX_VALUE_BYTES
                    {
                        return Err(Error::Limit);
                    }
                    let mut total = value_cost(&value, 0)?;
                    for owned in f
                        .locals
                        .iter()
                        .chain(f.args.iter())
                        .chain(f.names.iter().map(|(_, v)| v))
                    {
                        total = total
                            .checked_add(value_cost(owned, 0)?)
                            .ok_or(Error::Limit)?;
                    }
                    if total > MAX_DATA_BYTES {
                        return Err(Error::Limit);
                    }
                    f.names.push((name.path, value));
                }
                0x8a => {
                    r.byte()?;
                    let source = buffer_source(r, f)?;
                    let offset = self.expr(r, f, depth + 1, steps, effects)?.integer()?;
                    let name = r.name(&f.scope)?;
                    if !name.search
                        || name.tail != 1
                        || f.fields.len() >= 16
                        || f.names.iter().any(|(p, _)| *p == name.path)
                        || f.fields.iter().any(|v| v.name == name.path)
                    {
                        return Err(Error::Target);
                    }
                    let bytes = f.buffer(source)?;
                    let offset = usize::try_from(offset).map_err(|_| Error::Limit)?;
                    if offset.checked_add(4).is_none_or(|end| end > bytes.len()) {
                        return Err(Error::Limit);
                    }
                    f.fields.push(BufferField {
                        name: name.path,
                        source,
                        offset,
                    });
                }

                0xa4 => {
                    r.byte()?;
                    return Ok(Some(self.expr(r, f, depth + 1, steps, effects)?));
                }
                0xa0 => {
                    r.byte()?;
                    let mut p = r.package()?;
                    let yes = self.expr(&mut p, f, depth + 1, steps, effects)?.integer()? != 0;
                    if yes {
                        if let Some(v) = self.execute(&mut p, f, depth + 1, steps, effects)? {
                            return Ok(Some(v));
                        }
                    }
                    if r.at < r.bytes.len() && r.peek()? == 0xa1 {
                        r.byte()?;
                        let mut no = r.package()?;
                        if !yes {
                            if let Some(v) = self.execute(&mut no, f, depth + 1, steps, effects)? {
                                return Ok(Some(v));
                            }
                        }
                    }
                }
                0xa2 => {
                    r.byte()?;
                    let p = r.package()?;
                    loop {
                        step(steps)?;
                        let mut body = Reader::new(p.bytes);
                        if self
                            .expr(&mut body, f, depth + 1, steps, effects)?
                            .integer()?
                            == 0
                        {
                            break;
                        }
                        if let Some(v) = self.execute(&mut body, f, depth + 1, steps, effects)? {
                            return Ok(Some(v));
                        }
                    }
                }
                0xa3 => {
                    r.byte()?;
                }
                0xa1 => return Err(Error::Package),
                _ => {
                    self.expr(r, f, depth + 1, steps, effects)?;
                }
            }
        }
        Ok(None)
    }
}
struct EvaluationEffects<'a> {
    permissions: Option<&'a [PciFieldWritePermission]>,
    region_permissions: Option<&'a [RegionFieldWritePermission]>,
    reads: Vec<PciFieldRead>,
    writes: Vec<PciFieldWrite>,
    region_reads: Vec<RegionFieldRead>,
    region_writes: Vec<RegionFieldWrite>,
}
impl<'a> EvaluationEffects<'a> {
    fn read_only() -> Self {
        Self {
            permissions: None,
            region_permissions: None,
            reads: Vec::new(),
            writes: Vec::new(),
            region_reads: Vec::new(),
            region_writes: Vec::new(),
        }
    }
    fn with_region_permissions(
        permissions: &'a [PciFieldWritePermission],
        regions: &'a [RegionFieldWritePermission],
    ) -> Self {
        Self {
            permissions: Some(permissions),
            region_permissions: Some(regions),
            reads: Vec::new(),
            writes: Vec::new(),
            region_reads: Vec::new(),
            region_writes: Vec::new(),
        }
    }

    /// Whether any write intent is allowed to be produced at all.
    fn planning(&self) -> bool {
        self.permissions.is_some() || self.region_permissions.is_some()
    }
}
struct Frame {
    scope: Path,
    locals: [Value; 8],
    args: [Value; 7],
    names: Vec<(Path, Value)>,
    fields: Vec<BufferField>,
}
#[derive(Clone, Copy)]
enum BufferSource {
    Local(usize),
    Argument(usize),
    Name(usize),
}
struct BufferField {
    name: Path,
    source: BufferSource,
    offset: usize,
}
impl Frame {
    fn buffer(&self, source: BufferSource) -> Result<&[u8], Error> {
        let value = match source {
            BufferSource::Local(index) => &self.locals[index],
            BufferSource::Argument(index) => &self.args[index],
            BufferSource::Name(index) => &self.names[index].1,
        };
        match value {
            Value::Buffer(bytes) => Ok(bytes),
            _ => Err(Error::Type),
        }
    }
    fn buffer_mut(&mut self, source: BufferSource) -> Result<&mut [u8], Error> {
        let value = match source {
            BufferSource::Local(index) => &mut self.locals[index],
            BufferSource::Argument(index) => &mut self.args[index],
            BufferSource::Name(index) => &mut self.names[index].1,
        };
        match value {
            Value::Buffer(bytes) => Ok(bytes),
            _ => Err(Error::Type),
        }
    }
}
fn buffer_source(r: &mut Reader<'_>, frame: &Frame) -> Result<BufferSource, Error> {
    let source = match r.peek()? {
        opcode @ 0x60..=0x67 => {
            r.byte()?;
            BufferSource::Local(usize::from(opcode - 0x60))
        }
        opcode @ 0x68..=0x6e => {
            r.byte()?;
            BufferSource::Argument(usize::from(opcode - 0x68))
        }
        b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_' => {
            let name = r.name(&frame.scope)?;
            let index = frame
                .names
                .iter()
                .position(|(path, _)| *path == name.path)
                .ok_or(Error::Target)?;
            BufferSource::Name(index)
        }
        _ => return Err(Error::Target),
    };
    frame.buffer(source)?;
    Ok(source)
}

// Owned package copies are permitted only in locals, with a frame-wide/depth budget.
// No AML reference to a local/index escapes and no namespace package is ever mutated.
fn frame_local(f: &mut Frame, index: usize, value: Value) -> Result<(), Error> {
    let mut cost = value_cost(&value, 0)?;
    for (i, local) in f.locals.iter().enumerate() {
        if i != index {
            cost = cost
                .checked_add(value_cost(local, 0)?)
                .ok_or(Error::Limit)?;
        }
    }
    for arg in &f.args {
        cost = cost.checked_add(value_cost(arg, 0)?).ok_or(Error::Limit)?;
    }
    for (_, name) in &f.names {
        cost = cost.checked_add(value_cost(name, 0)?).ok_or(Error::Limit)?;
    }
    if cost > MAX_DATA_BYTES {
        return Err(Error::Limit);
    }
    f.locals[index] = value;
    Ok(())
}
fn target(r: &mut Reader<'_>, f: &mut Frame, value: Value) -> Result<(), Error> {
    // General compound stores remain refused. Only the separately budgeted owned local
    // package paths above and private buffer-field integer writes are supported.
    if !matches!(value, Value::Integer(_)) {
        return Err(Error::Target);
    }
    match r.byte()? {
        0 => Ok(()),
        n @ 0x60..=0x67 => {
            f.locals[usize::from(n - 0x60)] = value;
            Ok(())
        }
        n @ 0x68..=0x6e => {
            f.args[usize::from(n - 0x68)] = value;
            Ok(())
        }
        b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_' => {
            r.at -= 1;
            let name = r.name(&f.scope)?;
            let (source, offset) = f
                .fields
                .iter()
                .find(|field| field.name == name.path)
                .map(|field| (field.source, field.offset))
                .ok_or(Error::Target)?;
            let bytes = f.buffer_mut(source)?;
            bytes[offset..offset + 4].copy_from_slice(&(value.integer()? as u32).to_le_bytes());
            Ok(())
        }
        _ => Err(Error::Target),
    }
}
fn validate_arguments(args: &[Value]) -> Result<(), Error> {
    let mut bytes = 0usize;
    for arg in args {
        bytes = bytes.checked_add(value_cost(arg, 0)?).ok_or(Error::Limit)?;
        if bytes > MAX_VALUE_BYTES {
            return Err(Error::Limit);
        }
    }
    Ok(())
}
fn value_cost(value: &Value, depth: usize) -> Result<usize, Error> {
    if depth >= MAX_DEPTH {
        return Err(Error::Limit);
    }
    let mut cost = core::mem::size_of::<Value>();
    match value {
        Value::Buffer(bytes) | Value::String(bytes) => {
            if bytes.len() > MAX_VALUE_BYTES {
                return Err(Error::Limit);
            }
            cost += bytes.len();
        }
        Value::Package(items) => {
            if items.len() > MAX_PACKAGE {
                return Err(Error::Limit);
            }
            for item in items {
                cost = cost
                    .checked_add(value_cost(item, depth + 1)?)
                    .ok_or(Error::Limit)?;
            }
        }
        Value::Reference(path) => cost += path.0.len() * 4,
        Value::DeferredReference(NameReference(name)) => cost += name.path.0.len() * 4,
        _ => {}
    }
    if cost > MAX_DATA_BYTES {
        return Err(Error::Limit);
    }
    Ok(cost)
}
fn truth(b: bool) -> u64 {
    if b {
        u64::MAX
    } else {
        0
    }
}
fn step(steps: &mut usize) -> Result<(), Error> {
    *steps = steps.checked_sub(1).ok_or(Error::Steps)?;
    Ok(())
}

#[cfg(test)]
mod tests;
