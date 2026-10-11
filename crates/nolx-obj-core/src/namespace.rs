//! The object namespace.
//!
//! A tree of named nodes rooted at `\`. Each node is a directory, a symbolic link, or an
//! object carrying a payload `T` (for the kernel, an object pointer). Paths use `\` as the
//! separator, as the executive's names do; `\Device\Harddisk0` is the shape.
//!
//! Rules, all enforced here so the kernel cannot get them wrong per call site:
//!
//! * Names are case-sensitive, non-empty, at most [`NAME_MAX`] bytes, and may not contain
//!   `\` or NUL or be `.` or `..`.
//! * Paths are absolute and contain no empty, `.` or `..` components.
//! * Symbolic links are followed at every component, including the last, up to
//!   [`MAX_SYMLINK_DEPTH`] links deep; past that the lookup fails with
//!   [`NsError::SymlinkLoop`] rather than recursing forever.
//! * Removing a directory is refused while it has children.
//!
//! Storage is a fixed array of `N` slots, so the namespace is built before any allocator
//! exists, the same as the handle table.

use crate::security::{access_check, rights, Decision, SecurityDescriptor, Token};

/// The longest name a node may carry, in bytes.
pub const NAME_MAX: usize = 32;
/// The longest path, or symbolic-link target, in bytes.
pub const PATH_MAX: usize = 256;
/// How many symbolic links one lookup may follow before it is reported as a loop.
pub const MAX_SYMLINK_DEPTH: usize = 16;

const ROOT: usize = 0;

/// Why a namespace operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NsError {
    /// The name is empty, too long, reserved, or contains a separator or NUL.
    InvalidName,
    /// The path is not absolute, has an empty, `.` or `..` component, or is too long.
    InvalidPath,
    /// A component does not exist.
    NotFound,
    /// A node with that name already exists in the directory.
    NameCollision,
    /// A path component that must be a directory is not one.
    NotADirectory,
    /// The path names a directory or a symbolic link where an object was required.
    NotAnObject,
    /// The directory still has children.
    DirectoryNotEmpty,
    /// Symbolic links nest deeper than [`MAX_SYMLINK_DEPTH`], or form a cycle.
    SymlinkLoop,
    /// The symbolic-link target is longer than [`PATH_MAX`].
    TargetTooLong,
    /// Every slot is in use.
    Full,
    /// The operation would remove the root.
    IsRoot,
    /// The token did not satisfy a private namespace's boundary descriptor.
    AccessDenied {
        /// The boundary rights that were not granted.
        missing: u32,
    },
}

/// A fixed-capacity string, so names and targets live inside the namespace's array.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FixedText<const L: usize> {
    bytes: [u8; L],
    len: usize,
}

impl<const L: usize> FixedText<L> {
    const fn empty() -> Self {
        Self {
            bytes: [0; L],
            len: 0,
        }
    }

    fn try_from_text(text: &str) -> Option<Self> {
        if text.len() > L {
            return None;
        }
        let mut bytes = [0; L];
        bytes[..text.len()].copy_from_slice(text.as_bytes());
        Some(Self {
            bytes,
            len: text.len(),
        })
    }

    fn as_str(&self) -> &str {
        // Only ever filled from a `&str`, so this is valid UTF-8.
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

/// What a node is.
///
/// The symlink variant carries its whole target inline. That makes the enum as large as a
/// path, which clippy flags. Boxing it would need an allocator this `no_std` crate does not
/// have, and the namespace is a fixed array sized at compile time, so the extra bytes are a
/// fixed cost paid per slot rather than a per-object cost. The allow is deliberate.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Kind<T> {
    Directory,
    Symlink(FixedText<PATH_MAX>),
    Object(T),
}

#[derive(Debug)]
struct Node<T> {
    parent: usize,
    name: FixedText<NAME_MAX>,
    kind: Kind<T>,
}

/// The namespace: up to `N` nodes, including the root.
#[derive(Debug)]
pub struct Namespace<T, const N: usize> {
    nodes: [Option<Node<T>>; N],
}

impl<T, const N: usize> Default for Namespace<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T, const N: usize> Namespace<T, N> {
    /// A namespace with no nodes, not even a root. `const`, so it can be a `static` directly
    /// and never sits on a stack. Call [`Namespace::ensure_root`] before use.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            nodes: [const { None }; N],
        }
    }

    /// Add the root directory if it is missing. Idempotent.
    ///
    /// # Panics
    ///
    /// If `N` is zero: there must be room for the root.
    pub fn ensure_root(&mut self) {
        assert!(N >= 1, "the namespace needs room for its root");
        if self.nodes[ROOT].is_none() {
            self.nodes[ROOT] = Some(Node {
                parent: ROOT,
                name: FixedText::empty(),
                kind: Kind::Directory,
            });
        }
    }
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= NAME_MAX
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '\\' || c == '\0')
}

fn check_path(path: &str) -> Result<(), NsError> {
    if path.starts_with('\\') && path.len() <= PATH_MAX {
        Ok(())
    } else {
        Err(NsError::InvalidPath)
    }
}

impl<T, const N: usize> Namespace<T, N> {
    /// A namespace holding only the root directory.
    ///
    /// # Panics
    ///
    /// If `N` is zero: there must be room for the root.
    #[must_use]
    pub fn new() -> Self {
        let mut namespace = Self::empty();
        namespace.ensure_root();
        namespace
    }

    fn node(&self, index: usize) -> Result<&Node<T>, NsError> {
        self.nodes
            .get(index)
            .and_then(Option::as_ref)
            .ok_or(NsError::NotFound)
    }

    fn find_child(&self, directory: usize, name: &str) -> Option<usize> {
        self.nodes.iter().enumerate().find_map(|(index, slot)| {
            slot.as_ref()
                .filter(|node| {
                    index != ROOT && node.parent == directory && node.name.as_str() == name
                })
                .map(|_| index)
        })
    }

    fn has_children(&self, directory: usize) -> bool {
        self.nodes.iter().enumerate().any(|(index, slot)| {
            index != directory && slot.as_ref().is_some_and(|node| node.parent == directory)
        })
    }

    /// Resolve a path to a node index, following symbolic links.
    fn resolve(&self, path: &str, depth: usize) -> Result<usize, NsError> {
        if depth > MAX_SYMLINK_DEPTH {
            return Err(NsError::SymlinkLoop);
        }
        check_path(path)?;
        if path.len() == 1 {
            return Ok(ROOT);
        }
        let mut current = ROOT;
        for component in path[1..].split('\\') {
            if component.is_empty() || component == "." || component == ".." {
                return Err(NsError::InvalidPath);
            }
            if !matches!(self.node(current)?.kind, Kind::Directory) {
                return Err(NsError::NotADirectory);
            }
            let child = self
                .find_child(current, component)
                .ok_or(NsError::NotFound)?;
            current = match &self.node(child)?.kind {
                Kind::Symlink(target) => self.resolve(target.as_str(), depth + 1)?,
                _ => child,
            };
        }
        Ok(current)
    }

    /// Create a node under `parent` (a path to a directory) and return its index.
    fn attach(&mut self, parent: &str, name: &str, kind: Kind<T>) -> Result<usize, NsError> {
        if !valid_name(name) {
            return Err(NsError::InvalidName);
        }
        let directory = self.resolve(parent, 0)?;
        if !matches!(self.node(directory)?.kind, Kind::Directory) {
            return Err(NsError::NotADirectory);
        }
        if self.find_child(directory, name).is_some() {
            return Err(NsError::NameCollision);
        }
        let name = FixedText::try_from_text(name).ok_or(NsError::InvalidName)?;
        let slot = self
            .nodes
            .iter()
            .position(Option::is_none)
            .ok_or(NsError::Full)?;
        self.nodes[slot] = Some(Node {
            parent: directory,
            name,
            kind,
        });
        Ok(slot)
    }

    /// Create a directory `name` under the directory at `parent`.
    ///
    /// # Errors
    ///
    /// Any [`NsError`] from name, path, collision, or capacity checks.
    pub fn create_directory(&mut self, parent: &str, name: &str) -> Result<(), NsError> {
        self.attach(parent, name, Kind::Directory).map(|_| ())
    }

    /// Bind `object` to `name` under the directory at `parent`.
    ///
    /// # Errors
    ///
    /// As [`Namespace::create_directory`].
    pub fn insert_object(&mut self, parent: &str, name: &str, object: T) -> Result<(), NsError> {
        self.attach(parent, name, Kind::Object(object)).map(|_| ())
    }

    /// Create a symbolic link `name` under `parent` pointing at `target`.
    ///
    /// The target is not required to exist yet, as with NT symbolic links: it is resolved at
    /// lookup time, so a link can be created before the thing it names.
    ///
    /// # Errors
    ///
    /// [`NsError::InvalidPath`] if `target` is not an absolute path, [`NsError::TargetTooLong`]
    /// if it exceeds [`PATH_MAX`], and the usual name and collision errors.
    pub fn create_symlink(
        &mut self,
        parent: &str,
        name: &str,
        target: &str,
    ) -> Result<(), NsError> {
        check_path(target)?;
        let target = FixedText::try_from_text(target).ok_or(NsError::TargetTooLong)?;
        self.attach(parent, name, Kind::Symlink(target)).map(|_| ())
    }

    /// The object at `path`, following symbolic links.
    ///
    /// # Errors
    ///
    /// [`NsError::NotAnObject`] if the path names a directory, and the resolution errors.
    pub fn lookup(&self, path: &str) -> Result<&T, NsError> {
        let index = self.resolve(path, 0)?;
        match &self.node(index)?.kind {
            Kind::Object(object) => Ok(object),
            Kind::Directory | Kind::Symlink(_) => Err(NsError::NotAnObject),
        }
    }

    /// Whether `path` resolves to a directory, following symbolic links.
    ///
    /// # Errors
    ///
    /// Returns the path-resolution error if `path` is malformed or a component is missing.
    pub fn is_directory(&self, path: &str) -> Result<bool, NsError> {
        let index = self.resolve(path, 0)?;
        Ok(matches!(self.node(index)?.kind, Kind::Directory))
    }

    /// Whether the namespace contains only its root directory.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.nodes.iter().skip(1).all(Option::is_none)
    }

    /// Remove the entry at `path`.
    ///
    /// The final component is *not* followed, so a symbolic link is removed rather than what
    /// it points at. Returns the object for an object entry, `None` for a directory or link.
    ///
    /// # Errors
    ///
    /// [`NsError::IsRoot`] for `\`, [`NsError::DirectoryNotEmpty`] for a directory that still
    /// has children, and the resolution errors for the parent path.
    pub fn remove(&mut self, path: &str) -> Result<Option<T>, NsError> {
        check_path(path)?;
        if path.len() == 1 {
            return Err(NsError::IsRoot);
        }
        let split = path.rfind('\\').ok_or(NsError::InvalidPath)?;
        let parent_path = if split == 0 { "\\" } else { &path[..split] };
        let name = &path[split + 1..];
        if !valid_name(name) {
            return Err(NsError::InvalidName);
        }
        let parent = self.resolve(parent_path, 0)?;
        let child = self.find_child(parent, name).ok_or(NsError::NotFound)?;
        if matches!(self.node(child)?.kind, Kind::Directory) && self.has_children(child) {
            return Err(NsError::DirectoryNotEmpty);
        }
        let removed = self.nodes[child].take().ok_or(NsError::NotFound)?;
        Ok(match removed.kind {
            Kind::Object(object) => Some(object),
            Kind::Directory | Kind::Symlink(_) => None,
        })
    }
}

/// A separately rooted namespace protected by a boundary security descriptor.
///
/// Every lookup requires `READ`; creation requires `WRITE`; removal requires `DELETE`. The
/// namespace has its own fixed-capacity node array, so names in one private namespace never
/// collide with or resolve into another namespace.
#[derive(Debug)]
pub struct PrivateNamespace<T, const N: usize> {
    boundary: SecurityDescriptor,
    namespace: Namespace<T, N>,
}

impl<T, const N: usize> PrivateNamespace<T, N> {
    /// Create an empty private namespace guarded by `boundary`.
    #[must_use]
    pub fn new(boundary: SecurityDescriptor) -> Self {
        Self {
            boundary,
            namespace: Namespace::new(),
        }
    }

    fn authorize(&self, token: &Token, desired: u32) -> Result<(), NsError> {
        match access_check(token, &self.boundary, desired) {
            Decision::Granted(_) => Ok(()),
            Decision::Denied { missing } => Err(NsError::AccessDenied { missing }),
        }
    }

    /// Create a directory if `token` has write access to the namespace boundary.
    pub fn create_directory(
        &mut self,
        token: &Token,
        parent: &str,
        name: &str,
    ) -> Result<(), NsError> {
        self.authorize(token, rights::WRITE)?;
        self.namespace.create_directory(parent, name)
    }

    /// Bind `object` under `parent` if `token` has write access to the boundary.
    pub fn insert_object(
        &mut self,
        token: &Token,
        parent: &str,
        name: &str,
        object: T,
    ) -> Result<(), NsError> {
        self.authorize(token, rights::WRITE)?;
        self.namespace.insert_object(parent, name, object)
    }

    /// Create a symbolic link if `token` has write access to the namespace boundary.
    pub fn create_symlink(
        &mut self,
        token: &Token,
        parent: &str,
        name: &str,
        target: &str,
    ) -> Result<(), NsError> {
        self.authorize(token, rights::WRITE)?;
        self.namespace.create_symlink(parent, name, target)
    }

    /// Look up an object if `token` has read access to the namespace boundary.
    pub fn lookup(&self, token: &Token, path: &str) -> Result<&T, NsError> {
        self.authorize(token, rights::READ)?;
        self.namespace.lookup(path)
    }

    /// Remove an entry if `token` has delete access to the namespace boundary.
    pub fn remove(&mut self, token: &Token, path: &str) -> Result<Option<T>, NsError> {
        self.authorize(token, rights::DELETE)?;
        self.namespace.remove(path)
    }

    /// Whether the namespace has no entries beyond its root directory.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.namespace.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Ns = Namespace<u32, 16>;

    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;

    fn boundary() -> SecurityDescriptor {
        let mut dacl = crate::security::Acl::empty();
        dacl.push(crate::security::Ace {
            kind: crate::security::AceKind::Allow,
            sid: crate::security::Sid::user(ALICE),
            mask: rights::ALL,
        })
        .unwrap();
        SecurityDescriptor {
            owner: crate::security::Sid::SYSTEM,
            dacl: Some(dacl),
            label: None,
        }
    }

    fn alice() -> Token {
        Token::new(
            crate::security::Sid::user(ALICE),
            crate::security::IntegrityLevel::Medium,
        )
    }

    fn bob() -> Token {
        Token::new(
            crate::security::Sid::user(BOB),
            crate::security::IntegrityLevel::Medium,
        )
    }

    fn populated() -> Ns {
        let mut ns = Ns::new();
        ns.create_directory("\\", "Device").unwrap();
        ns.insert_object("\\Device", "Null", 1).unwrap();
        ns.create_directory("\\", "BaseNamedObjects").unwrap();
        ns
    }

    #[test]
    fn root_exists_and_is_not_an_object() {
        let ns = Ns::new();
        assert_eq!(ns.lookup("\\"), Err(NsError::NotAnObject));
    }

    #[test]
    fn object_round_trips_through_directories() {
        let ns = populated();
        assert_eq!(ns.lookup("\\Device\\Null"), Ok(&1));
    }

    #[test]
    fn names_are_case_sensitive() {
        let ns = populated();
        assert_eq!(ns.lookup("\\device\\Null"), Err(NsError::NotFound));
        assert_eq!(ns.lookup("\\Device\\null"), Err(NsError::NotFound));
    }

    #[test]
    fn collision_is_reported_and_leaves_the_original() {
        let mut ns = populated();
        assert_eq!(
            ns.insert_object("\\Device", "Null", 2),
            Err(NsError::NameCollision)
        );
        assert_eq!(ns.lookup("\\Device\\Null"), Ok(&1));
    }

    #[test]
    fn invalid_names_are_rejected() {
        let mut ns = Ns::new();
        for name in ["", ".", "..", "a\\b", "nul\0"] {
            assert_eq!(
                ns.create_directory("\\", name),
                Err(NsError::InvalidName),
                "name {name:?}"
            );
        }
        let long = "x".repeat(NAME_MAX + 1);
        assert_eq!(ns.create_directory("\\", &long), Err(NsError::InvalidName));
        let max = "x".repeat(NAME_MAX);
        assert!(ns.create_directory("\\", &max).is_ok());
    }

    #[test]
    fn malformed_paths_are_rejected() {
        let ns = populated();
        let malformed = [
            "Device\\Null",
            "\\Device\\",
            "\\\\Device",
            "\\Device\\.\\Null",
            "\\Device\\..\\Device",
        ];
        for path in malformed {
            assert_eq!(ns.lookup(path), Err(NsError::InvalidPath), "path {path:?}");
        }
    }

    #[test]
    fn walking_through_an_object_is_not_a_directory() {
        let ns = populated();
        assert_eq!(
            ns.lookup("\\Device\\Null\\Extra"),
            Err(NsError::NotADirectory)
        );
    }

    #[test]
    fn missing_component_is_not_found() {
        let ns = populated();
        assert_eq!(ns.lookup("\\Device\\Missing"), Err(NsError::NotFound));
        assert_eq!(ns.lookup("\\Nope\\Null"), Err(NsError::NotFound));
    }

    #[test]
    fn symlink_is_followed_at_any_component() {
        let mut ns = populated();
        ns.create_symlink("\\", "Null", "\\Device\\Null").unwrap();
        assert_eq!(ns.lookup("\\Null"), Ok(&1));

        ns.create_directory("\\", "Alias").unwrap();
        ns.create_symlink("\\Alias", "Dev", "\\Device").unwrap();
        assert_eq!(ns.lookup("\\Alias\\Dev\\Null"), Ok(&1));
    }

    #[test]
    fn symlink_may_precede_its_target() {
        let mut ns = Ns::new();
        ns.create_symlink("\\", "Later", "\\Target").unwrap();
        assert_eq!(ns.lookup("\\Later"), Err(NsError::NotFound));
        ns.insert_object("\\", "Target", 7).unwrap();
        assert_eq!(ns.lookup("\\Later"), Ok(&7));
    }

    #[test]
    fn symlink_cycle_is_a_loop_not_a_hang() {
        let mut ns = Ns::new();
        ns.create_symlink("\\", "A", "\\B").unwrap();
        ns.create_symlink("\\", "B", "\\A").unwrap();
        assert_eq!(ns.lookup("\\A"), Err(NsError::SymlinkLoop));
    }

    #[test]
    fn symlink_target_must_be_absolute_and_fit() {
        let mut ns = Ns::new();
        assert_eq!(
            ns.create_symlink("\\", "Rel", "Device"),
            Err(NsError::InvalidPath)
        );
        let long = format!("\\{}", "x".repeat(PATH_MAX));
        assert_eq!(
            ns.create_symlink("\\", "Long", &long),
            Err(NsError::InvalidPath),
            "the path check runs before the length check"
        );
    }

    #[test]
    fn remove_takes_the_object_and_frees_the_name() {
        let mut ns = populated();
        assert_eq!(ns.remove("\\Device\\Null"), Ok(Some(1)));
        assert_eq!(ns.lookup("\\Device\\Null"), Err(NsError::NotFound));
        ns.insert_object("\\Device", "Null", 3).unwrap();
        assert_eq!(ns.lookup("\\Device\\Null"), Ok(&3));
    }

    #[test]
    fn remove_refuses_nonempty_directory_then_allows_empty() {
        let mut ns = populated();
        assert_eq!(ns.remove("\\Device"), Err(NsError::DirectoryNotEmpty));
        ns.remove("\\Device\\Null").unwrap();
        assert_eq!(ns.remove("\\Device"), Ok(None));
    }

    #[test]
    fn remove_takes_the_link_not_its_target() {
        let mut ns = populated();
        ns.create_symlink("\\", "Null", "\\Device\\Null").unwrap();
        assert_eq!(ns.remove("\\Null"), Ok(None));
        assert_eq!(ns.lookup("\\Device\\Null"), Ok(&1));
    }

    #[test]
    fn root_cannot_be_removed() {
        let mut ns = Ns::new();
        assert_eq!(ns.remove("\\"), Err(NsError::IsRoot));
    }

    #[test]
    fn capacity_is_reported() {
        let mut ns: Namespace<u32, 2> = Namespace::new();
        ns.create_directory("\\", "A").unwrap();
        assert_eq!(ns.create_directory("\\", "B"), Err(NsError::Full));
    }

    #[test]
    fn directory_queries_follow_links_and_distinguish_objects() {
        let mut ns = populated();
        ns.create_symlink("\\", "Devices", "\\Device").unwrap();
        assert_eq!(ns.is_directory("\\Devices"), Ok(true));
        assert_eq!(ns.is_directory("\\Device\\Null"), Ok(false));
        assert_eq!(ns.is_directory("\\Missing"), Err(NsError::NotFound));
    }

    #[test]
    fn private_namespaces_are_isolated_for_the_same_path() {
        let mut first: PrivateNamespace<u32, 8> = PrivateNamespace::new(boundary());
        let mut second: PrivateNamespace<u32, 8> = PrivateNamespace::new(boundary());
        let owner = alice();
        for namespace in [&mut first, &mut second] {
            namespace.create_directory(&owner, "\\", "Private").unwrap();
        }
        first
            .insert_object(&owner, "\\Private", "Item", 11)
            .unwrap();
        second
            .insert_object(&owner, "\\Private", "Item", 22)
            .unwrap();

        assert_eq!(first.lookup(&owner, "\\Private\\Item"), Ok(&11));
        assert_eq!(second.lookup(&owner, "\\Private\\Item"), Ok(&22));
        assert!(!first.is_empty());
        assert!(!second.is_empty());
    }

    #[test]
    fn private_namespace_symbolic_links_stay_inside_their_root() {
        let mut namespace: PrivateNamespace<u32, 8> = PrivateNamespace::new(boundary());
        let owner = alice();
        let outsider = bob();
        namespace.create_directory(&owner, "\\", "Private").unwrap();
        namespace
            .insert_object(&owner, "\\Private", "Item", 11)
            .unwrap();
        namespace
            .create_symlink(&owner, "\\", "Alias", "\\Private\\Item")
            .unwrap();

        assert_eq!(namespace.lookup(&owner, "\\Alias"), Ok(&11));
        assert_eq!(
            namespace.create_symlink(&outsider, "\\", "Denied", "\\Private\\Item",),
            Err(NsError::AccessDenied {
                missing: rights::WRITE
            })
        );
        assert_eq!(namespace.lookup(&owner, "\\Private\\Item"), Ok(&11));
    }

    #[test]
    fn private_namespace_boundary_gates_read_write_and_delete() {
        let mut namespace: PrivateNamespace<u32, 8> = PrivateNamespace::new(boundary());
        let owner = alice();
        let outsider = bob();
        namespace.insert_object(&owner, "\\", "Secret", 7).unwrap();

        assert_eq!(
            namespace.lookup(&outsider, "\\Secret"),
            Err(NsError::AccessDenied {
                missing: rights::READ
            })
        );
        assert_eq!(
            namespace.insert_object(&outsider, "\\", "Intrusion", 9),
            Err(NsError::AccessDenied {
                missing: rights::WRITE
            })
        );
        assert_eq!(
            namespace.remove(&outsider, "\\Secret"),
            Err(NsError::AccessDenied {
                missing: rights::DELETE
            })
        );
        assert_eq!(namespace.lookup(&owner, "\\Secret"), Ok(&7));
        assert_eq!(namespace.remove(&owner, "\\Secret"), Ok(Some(7)));
        assert!(namespace.is_empty());
    }
}
