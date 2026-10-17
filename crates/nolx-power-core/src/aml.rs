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
    fn parent(&self) -> Self {
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
#[derive(Clone)]
enum Kind<'a> {
    Scope,
    Device,
    Data(Value),
    Alias(Path),
    Method { args: usize, body: &'a [u8] },
    Opaque(u16),
    External,
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

/// A namespace whose method bytecode borrows validated DSDT/SSDT payloads.
#[derive(Clone)]
pub struct Namespace<'a> {
    objects: Vec<Object<'a>>,
    value_bytes: usize,
    integer_mask: u64,
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
        }
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
        let cost = if let Kind::Data(value) = &kind {
            value_cost(value, 0)?
        } else {
            0
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
                            r.byte()?;
                            self.data(r, scope, depth + 1, steps)?;
                            self.data(r, scope, depth + 1, steps)?;
                            self.insert(name, Kind::Opaque(0x5b80))?;
                        }
                        0x81 | 0x86 | 0x87 => {
                            let mut p = r.package()?;
                            p.name(scope)?;
                            if ext != 0x81 {
                                p.name(scope)?;
                            }
                            if ext == 0x87 {
                                self.data(&mut p, scope, depth + 1, steps)?;
                            }
                            p.byte()?;
                            self.fields(&mut p, scope, steps)?;
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
    fn fields(&mut self, r: &mut Reader<'a>, scope: &Path, steps: &mut usize) -> Result<(), Error> {
        while r.at < r.bytes.len() {
            step(steps)?;
            match r.peek()? {
                0 => {
                    r.byte()?;
                    r.length()?;
                }
                1 => {
                    r.take(3)?;
                }
                3 => {
                    r.take(4)?;
                }
                2 => return Err(Error::Unsupported(0x5b81)),
                _ => {
                    let s: [u8; 4] = r.take(4)?.try_into().map_err(|_| Error::Name)?;
                    check_segment(&s)?;
                    r.length()?;
                    self.insert(scope.child(s)?, Kind::Opaque(0x5b81))?;
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
        self.object(index, args, 0, &mut steps)
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
    fn object(
        &self,
        index: usize,
        args: &[Value],
        depth: usize,
        steps: &mut usize,
    ) -> Result<Value, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        step(steps)?;
        let mut argument_bytes = 0usize;
        for arg in args {
            argument_bytes = argument_bytes
                .checked_add(value_cost(arg, 0)?)
                .ok_or(Error::Limit)?;
            if argument_bytes > MAX_VALUE_BYTES {
                return Err(Error::Limit);
            }
        }
        let o = &self.objects[index];
        match &o.kind {
            Kind::Alias(path) => {
                let target = self
                    .objects
                    .iter()
                    .position(|o| &o.path == path)
                    .ok_or(Error::Missing)?;
                self.object(target, args, depth + 1, steps)
            }
            Kind::Data(v) if args.is_empty() => self.constant(v.clone(), depth + 1),
            Kind::Method {
                args: expected,
                body,
            } if *expected == args.len() => {
                let mut frame = Frame {
                    scope: o.path.parent(),
                    locals: core::array::from_fn(|_| Value::Uninitialized),
                    args: core::array::from_fn(|i| match args.get(i) {
                        Some(Value::Integer(value)) => Value::Integer(value & self.integer_mask),
                        Some(value) => value.clone(),
                        None => Value::Uninitialized,
                    }),
                };
                self.execute(&mut Reader::new(body), &mut frame, depth + 1, steps)?
                    .ok_or(Error::NoReturn)
            }
            Kind::Opaque(op) => Err(Error::Unsupported(*op)),
            Kind::External => Err(Error::Missing),
            _ => Err(Error::Type),
        }
    }
    fn expr(
        &self,
        r: &mut Reader<'a>,
        f: &mut Frame,
        depth: usize,
        steps: &mut usize,
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
                let v = self.expr(r, f, depth + 1, steps)?;
                target(r, f, v.clone())?;
                Ok(v)
            }
            0x72 | 0x74 | 0x77 | 0x79..=0x80 | 0x90..=0x95 => {
                r.byte()?;
                let a = self.expr(r, f, depth + 1, steps)?.integer()?;
                let unary = op == 0x80 || op == 0x92;
                let b = if unary {
                    0
                } else {
                    self.expr(r, f, depth + 1, steps)?.integer()?
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
                    target(r, f, v.clone())?;
                }
                Ok(v)
            }
            b'\\' | b'^' | 0x2e | 0x2f | b'A'..=b'Z' | b'_' => {
                let name = r.name(&f.scope)?;
                let index = self.resolve(&name)?;
                let count = if let Kind::Method { args, .. } = self.objects[index].kind {
                    args
                } else {
                    0
                };
                let mut args = Vec::new();
                for _ in 0..count {
                    args.push(self.expr(r, f, depth + 1, steps)?);
                }
                self.object(index, &args, depth + 1, steps)
            }
            _ => {
                let v = self.data(r, &f.scope, depth + 1, steps)?;
                self.constant(v, depth + 1)
            }
        }
    }
    fn execute(
        &self,
        r: &mut Reader<'a>,
        f: &mut Frame,
        depth: usize,
        steps: &mut usize,
    ) -> Result<Option<Value>, Error> {
        if depth >= MAX_DEPTH {
            return Err(Error::Limit);
        }
        while r.at < r.bytes.len() {
            step(steps)?;
            match r.peek()? {
                0xa4 => {
                    r.byte()?;
                    return Ok(Some(self.expr(r, f, depth + 1, steps)?));
                }
                0xa0 => {
                    r.byte()?;
                    let mut p = r.package()?;
                    let yes = self.expr(&mut p, f, depth + 1, steps)?.integer()? != 0;
                    if yes {
                        if let Some(v) = self.execute(&mut p, f, depth + 1, steps)? {
                            return Ok(Some(v));
                        }
                    }
                    if r.at < r.bytes.len() && r.peek()? == 0xa1 {
                        r.byte()?;
                        let mut no = r.package()?;
                        if !yes {
                            if let Some(v) = self.execute(&mut no, f, depth + 1, steps)? {
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
                        if self.expr(&mut body, f, depth + 1, steps)?.integer()? == 0 {
                            break;
                        }
                        if let Some(v) = self.execute(&mut body, f, depth + 1, steps)? {
                            return Ok(Some(v));
                        }
                    }
                }
                0xa3 => {
                    r.byte()?;
                }
                0xa1 => return Err(Error::Package),
                _ => {
                    self.expr(r, f, depth + 1, steps)?;
                }
            }
        }
        Ok(None)
    }
}
struct Frame {
    scope: Path,
    locals: [Value; 8],
    args: [Value; 7],
}
fn target(r: &mut Reader<'_>, f: &mut Frame, value: Value) -> Result<(), Error> {
    // Compound local/argument stores are outside the pure subset; refusing them also
    // prevents repeated stores/calls from amplifying large buffers into unbounded frames.
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
        _ => Err(Error::Target),
    }
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
