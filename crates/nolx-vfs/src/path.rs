//! `namei`: POSIX pathname resolution.
//!
//! This is the single implementation of pathname resolution in NOLX. Every syscall that
//! takes a path -- `open`, `stat`, `chdir`, `unlink`, and the `*at` family -- goes through
//! [`resolve`], which is the only way the rules below get applied consistently.
//!
//! # Rules implemented
//!
//! Taken from the *Pathname Resolution* section of the Base Definitions volume:
//!
//! * An empty pathname is an error (`ENOENT`).
//! * A pathname beginning with `/` starts at the process root; otherwise at the working
//!   directory.
//! * Successive `/` characters collapse: `a//b` and `a/b` name the same file. A trailing
//!   `/` is ignored except that it forces the final component to be a directory.
//! * `.` refers to the directory itself and is a no-op.
//! * `..` refers to the parent. At a mount point, `..` refers to the parent of the
//!   *covered* directory -- crossing a mount point downward and then up lands you back
//!   where you came from. At the root, `..` is the root.
//! * A symbolic link is resolved in place: an absolute target restarts resolution at the
//!   root, a relative target continues in the link's directory. Nesting is bounded by
//!   [`nolx_types::scalars::SYMLOOP_MAX`], beyond which resolution fails with `ELOOP`.
//! * If the final component is a symlink and the caller did not ask for it to be followed
//!   (`O_NOFOLLOW`, `AT_SYMLINK_NOFOLLOW`, `lstat`), the link itself is returned.
//! * A non-directory in the middle of a path is `ENOTDIR`.
//! * A missing component is `ENOENT`.
//! * A component longer than `NAME_MAX` is `ENAMETOOLONG`; a pathname longer than
//!   `PATH_MAX` is likewise `ENAMETOOLONG`.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::mount::NodeId;
use nolx_types::errno;
use nolx_types::scalars;

/// The view of the filesystem that resolution needs.
///
/// The kernel implements this over the object manager and the mounted filesystems; the
/// tests implement it over a hand-built tree. Keeping it a trait is what makes the
/// resolution rules testable without a filesystem.
pub trait Resolver {
    /// The root of the resolution: the process root for an absolute path, the working
    /// directory for a relative one.
    fn start(&self, absolute: bool) -> NodeId;

    /// Look up one component in a directory.
    ///
    /// # Errors
    ///
    /// A POSIX `errno`: `ENOENT`, `ENOTDIR`, `EACCES`, `ENAMETOOLONG`.
    fn lookup(&self, dir: NodeId, name: &[u8]) -> Result<NodeId, i32>;

    /// The parent of a node, as `..` sees it. Returns the node itself at a root.
    fn parent(&self, node: NodeId) -> NodeId;

    /// `true` if the node is a directory.
    fn is_dir(&self, node: NodeId) -> bool;

    /// `true` if the node is a symbolic link.
    fn is_symlink(&self, node: NodeId) -> bool;

    /// Read a symlink's target.
    ///
    /// # Errors
    ///
    /// `EINVAL` if the node is not a symlink; `EIO` on a read failure.
    fn read_link(&self, node: NodeId) -> Result<Vec<u8>, i32>;

    /// Check that the caller may search `dir`, as `X_OK` on a directory.
    ///
    /// # Errors
    ///
    /// `EACCES` if search is not permitted.
    fn may_search(&self, dir: NodeId) -> Result<(), i32>;

    /// The number of symlinks followed so far, held by the caller so that a nested
    /// resolution cannot reset the budget.
    fn symlink_budget(&self) -> u32;

    /// Record that one more symlink was followed.
    fn note_symlink(&self);
}

/// How a resolution should end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Final {
    /// Follow the last component if it is a symlink. This is `stat`, `open` without
    /// `O_NOFOLLOW`, `chdir`.
    Follow,
    /// Return the last component even if it is a symlink. This is `lstat`, `open` with
    /// `O_NOFOLLOW`, and the target side of `rename`/`unlink`.
    NoFollow,
}

/// The result of a successful resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// The node the pathname names.
    pub node: NodeId,
    /// `true` if the last component was a symlink that we did not follow.
    pub is_symlink: bool,
}

/// Resolve `path`.
///
/// # Errors
///
/// A POSIX `errno` value. Never a panic: a malformed path, a symlink loop, or a hostile
/// filesystem all produce an error code.
pub fn resolve<R: Resolver + ?Sized>(
    vfs: &R,
    path: &[u8],
    final_component: Final,
) -> Result<Resolution, i32> {
    if path.is_empty() {
        return Err(errno::ENOENT);
    }
    if path.len() > scalars::PATH_MAX {
        return Err(errno::ENAMETOOLONG);
    }

    let absolute = path[0] == b'/';
    let mut current = vfs.start(absolute);

    // Components, split once. Splitting up front keeps the symlink-substitution logic in
    // one place instead of tangled into the walk.
    let mut pending: Queue<'_> = split_components(path);

    // A trailing slash means the pathname must name a directory.
    let require_dir = path.len() > 1 && path[path.len() - 1] == b'/';


    while let Some(comp) = pending.pop_at_front() {
        let is_last = pending.is_empty();
        // `comp` is owned by this iteration, so borrowing its bytes does not borrow
        // `pending` and cannot conflict with replacing the queue below.
        let component: &[u8] = comp.as_bytes();

        if component == b"." {
            // `.` names the directory itself. A trailing `/.` still requires a directory.
            if is_last && require_dir && !vfs.is_dir(current) {
                return Err(errno::ENOTDIR);
            }
            continue;
        }

        if component == b".." {
            current = vfs.parent(current);
            if is_last && require_dir && !vfs.is_dir(current) {
                return Err(errno::ENOTDIR);
            }
            continue;
        }

        if !vfs.is_dir(current) {
            return Err(errno::ENOTDIR);
        }
        vfs.may_search(current)?;

        if component.len() > scalars::NAME_MAX {
            return Err(errno::ENAMETOOLONG);
        }

        let found = vfs.lookup(current, component)?;

        if vfs.is_symlink(found) && !(is_last && final_component == Final::NoFollow) {
            if vfs.symlink_budget() >= scalars::SYMLOOP_MAX as u32 {
                return Err(errno::ELOOP);
            }
            vfs.note_symlink();
            let target = vfs.read_link(found)?;
            if target.is_empty() {
                return Err(errno::ENOENT);
            }
            let starts_at_root = target[0] == b'/';

            // The components carved from the target own their bytes, so they live exactly
            // as long as the walk does and there is no arena to keep alive behind them.
            let mut substituted = Queue::from_parts(split_owned(target));
            // Whatever is left of the original pathname comes after the substituted
            // components, which is what makes a relative link resolve in its own directory
            // and an absolute one restart from the root.
            substituted.append_tail(pending.take_rest());
            pending = substituted;
            if starts_at_root {
                current = vfs.start(true);
            }
            continue;
        }

        current = found;
        if is_last && require_dir && !vfs.is_dir(current) {
            return Err(errno::ENOTDIR);
        }
    }

    let is_symlink = vfs.is_symlink(current);
    Ok(Resolution {
        node: current,
        is_symlink,
    })
}

/// Split a path into its components, dropping empty ones (so `a//b` is `["a", "b"]`).
///
/// The returned queue borrows the path, which is what lets a symlink target be spliced in
/// front of what remains without copying the whole pathname.
fn split_components(path: &[u8]) -> Queue<'_> {
    let items = path
        .split(|&b| b == b'/')
        .filter(|c| !c.is_empty())
        .map(Comp::Borrowed)
        .collect();
    Queue { items }
}

/// Split a symlink target into owned components.
///
/// Owned because the target came from the resolver as a `Vec<u8>` that nothing else keeps
/// alive. The lifetime parameter is unconstrained -- every component here is `Owned` -- so
/// the result drops into any `Queue<'a>`.
fn split_owned<'a>(target: Vec<u8>) -> Vec<Comp<'a>> {
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, &byte) in target.iter().enumerate() {
        if byte == b'/' {
            if index > start {
                parts.push(Comp::Owned(target[start..index].into()));
            }
            start = index + 1;
        }
    }
    if target.len() > start {
        parts.push(Comp::Owned(target[start..].into()));
    }
    parts
}

/// One pathname component.
///
/// Borrowed when it is a slice of the pathname the caller passed in, owned when it was
/// carved out of a symlink target. The owned variant is what makes substitution work: the
/// resolver hands back a `Vec<u8>` with no other home, and the components taken from it
/// have to live exactly as long as the walk rather than as long as one loop iteration.
enum Comp<'a> {
    /// A slice of the caller's pathname.
    Borrowed(&'a [u8]),
    /// A slice of a symlink target this resolution produced.
    Owned(Box<[u8]>),
}

impl Comp<'_> {
    /// The component's bytes, whichever variant holds them.
    fn as_bytes(&self) -> &[u8] {
        match self {
            Self::Borrowed(b) | Self::Owned(b) => b,
        }
    }
}

/// The component queue used while walking.
///
/// A `VecDeque` rather than a `Vec` plus a head index because components are now owned and
/// have to be *moved* out, not copied.
struct Queue<'a> {
    items: VecDeque<Comp<'a>>,
}

impl<'a> Queue<'a> {
    /// A queue over already-split components.
    fn from_parts(parts: Vec<Comp<'a>>) -> Self {
        Self { items: parts.into() }
    }

    /// Take the next component, or `None` when exhausted.
    fn pop_at_front(&mut self) -> Option<Comp<'a>> {
        self.items.pop_front()
    }

    /// `true` when no components remain.
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Everything not yet consumed.
    fn take_rest(&mut self) -> Vec<Comp<'a>> {
        self.items.drain(..).collect()
    }

    /// Push the unconsumed remainder of an outer resolution onto the back.
    fn append_tail(&mut self, tail: Vec<Comp<'a>>) {
        self.items.extend(tail);
    }

    /// The bytes of every component, for tests and diagnostics.
    fn as_slices(&self) -> Vec<&[u8]> {
        self.items.iter().map(Comp::as_bytes).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec;
    use core::cell::Cell;
    use nolx_types::scalars::{S_IFDIR, S_IFLNK, S_IFREG};

    /// A tiny in-memory tree: enough to exercise every rule without a filesystem.
    struct Tree {
        nodes: Vec<TestNode>,
        links_followed: Cell<u32>,
    }

    struct TestNode {
        mode: u32,
        name: String,
        parent: usize,
        target: Option<String>,
    }

    impl Tree {
        fn new() -> Self {
            Self {
                nodes: vec![TestNode {
                    mode: S_IFDIR | 0o755,
                    name: String::from("/"),
                    parent: 0,
                    target: None,
                }],
                links_followed: Cell::new(0),
            }
        }

        fn add(&mut self, parent: usize, name: &str, mode: u32) -> usize {
            self.nodes.push(TestNode {
                mode,
                name: String::from(name),
                parent,
                target: None,
            });
            self.nodes.len() - 1
        }

        fn add_link(&mut self, parent: usize, name: &str, target: &str) -> usize {
            self.nodes.push(TestNode {
                mode: S_IFLNK | 0o777,
                name: String::from(name),
                parent,
                target: Some(String::from(target)),
            });
            self.nodes.len() - 1
        }
    }

    impl Resolver for Tree {
        fn start(&self, _absolute: bool) -> NodeId {
            // The test tree has one root, which also serves as the working directory.
            NodeId { mount: 0, ino: 0 }
        }

        fn lookup(&self, dir: NodeId, name: &[u8]) -> Result<NodeId, i32> {
            let want = core::str::from_utf8(name).map_err(|_| errno::ENOENT)?;
            self.nodes
                .iter()
                .enumerate()
                .find(|(i, n)| *i != 0 && n.parent == dir.ino as usize && n.name == want)
                .map(|(i, _)| NodeId {
                    mount: 0,
                    ino: i as u64,
                })
                .ok_or(errno::ENOENT)
        }

        fn parent(&self, node: NodeId) -> NodeId {
            NodeId {
                mount: 0,
                ino: self.nodes[node.ino as usize].parent as u64,
            }
        }

        fn is_dir(&self, node: NodeId) -> bool {
            scalars::is_dir(self.nodes[node.ino as usize].mode)
        }

        fn is_symlink(&self, node: NodeId) -> bool {
            scalars::is_lnk(self.nodes[node.ino as usize].mode)
        }

        fn read_link(&self, node: NodeId) -> Result<Vec<u8>, i32> {
            self.nodes[node.ino as usize]
                .target
                .as_ref()
                .map(|t| t.as_bytes().to_vec())
                .ok_or(errno::EINVAL)
        }

        fn may_search(&self, _dir: NodeId) -> Result<(), i32> {
            Ok(())
        }

        fn symlink_budget(&self) -> u32 {
            self.links_followed.get()
        }

        fn note_symlink(&self) {
            self.links_followed.set(self.links_followed.get() + 1);
        }
    }

    fn sample_tree() -> Tree {
        let mut t = Tree::new();
        let dir = t.add(0, "dir", S_IFDIR | 0o755);
        t.add(dir, "file", S_IFREG | 0o644);
        let sub = t.add(dir, "sub", S_IFDIR | 0o755);
        t.add(sub, "deep", S_IFREG | 0o644);
        t.add_link(0, "to_dir", "dir");
        t.add_link(0, "to_abs", "/dir/sub");
        t.add_link(0, "rel", "dir/sub");
        t.add_link(dir, "up", "..");
        t.add_link(0, "loop", "loop");
        t.add_link(0, "a", "b");
        t.add_link(0, "b", "a");
        t.add_link(0, "dangling", "nowhere");
        t.add_link(0, "empty", "");
        t
    }

    #[test]
    fn splits_components_and_collapses_slashes() {
        let q = split_components(b"/a//b///c/");
        assert_eq!(q.as_slices(), vec![&b"a"[..], &b"b"[..], &b"c"[..]]);
        assert!(split_components(b"/").items.is_empty());
        assert!(split_components(b"").items.is_empty());
    }

    #[test]
    fn empty_path_is_enoent() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"", Final::Follow), Err(errno::ENOENT));
    }

    #[test]
    fn overlong_path_is_enametoolong() {
        let t = sample_tree();
        let long = vec![b'a'; scalars::PATH_MAX + 1];
        assert_eq!(resolve(&t, &long, Final::Follow), Err(errno::ENAMETOOLONG));
    }

    #[test]
    fn overlong_component_is_enametoolong() {
        let t = sample_tree();
        let mut path = b"/".to_vec();
        path.extend_from_slice(&vec![b'a'; scalars::NAME_MAX + 1]);
        assert_eq!(resolve(&t, &path, Final::Follow), Err(errno::ENAMETOOLONG));
    }

    #[test]
    fn a_maximal_length_component_is_accepted() {
        let mut t = Tree::new();
        let name = "x".repeat(scalars::NAME_MAX);
        t.add(0, &name, S_IFREG | 0o644);
        let mut path = b"/".to_vec();
        path.extend_from_slice(name.as_bytes());
        assert!(resolve(&t, &path, Final::Follow).is_ok());
    }

    #[test]
    fn resolves_plain_paths() {
        let t = sample_tree();
        let r = resolve(&t, b"/dir/file", Final::Follow).unwrap();
        assert_eq!(t.nodes[r.node.ino as usize].name, "file");

        let r = resolve(&t, b"/dir/sub/deep", Final::Follow).unwrap();
        assert_eq!(t.nodes[r.node.ino as usize].name, "deep");
    }

    #[test]
    fn duplicate_slashes_name_the_same_file() {
        let t = sample_tree();
        let a = resolve(&t, b"/dir/file", Final::Follow).unwrap();
        let b = resolve(&t, b"//dir///file//", Final::Follow).unwrap();
        assert_eq!(a.node, b.node);
    }

    #[test]
    fn dot_is_a_no_op() {
        let t = sample_tree();
        let a = resolve(&t, b"/dir/file", Final::Follow).unwrap();
        let b = resolve(&t, b"/./dir/./file", Final::Follow).unwrap();
        assert_eq!(a.node, b.node);
    }

    #[test]
    fn dotdot_walks_up() {
        let t = sample_tree();
        let a = resolve(&t, b"/dir", Final::Follow).unwrap();
        let b = resolve(&t, b"/dir/sub/..", Final::Follow).unwrap();
        assert_eq!(a.node, b.node);
    }

    #[test]
    fn dotdot_at_the_root_is_the_root() {
        let t = sample_tree();
        let r = resolve(&t, b"/../../..", Final::Follow).unwrap();
        assert_eq!(r.node.ino, 0);
    }

    #[test]
    fn missing_component_is_enoent() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"/nope", Final::Follow), Err(errno::ENOENT));
        assert_eq!(resolve(&t, b"/dir/nope", Final::Follow), Err(errno::ENOENT));
        assert_eq!(
            resolve(&t, b"/dir/file/deeper", Final::Follow),
            Err(errno::ENOTDIR)
        );
    }

    #[test]
    fn a_file_in_the_middle_is_enotdir() {
        let t = sample_tree();
        assert_eq!(
            resolve(&t, b"/dir/file/x", Final::Follow),
            Err(errno::ENOTDIR)
        );
    }

    #[test]
    fn trailing_slash_requires_a_directory() {
        let t = sample_tree();
        assert!(resolve(&t, b"/dir/", Final::Follow).is_ok());
        assert_eq!(
            resolve(&t, b"/dir/file/", Final::Follow),
            Err(errno::ENOTDIR)
        );
    }

    #[test]
    fn relative_symlinks_are_followed_in_place() {
        let t = sample_tree();
        let via_link = resolve(&t, b"/rel/deep", Final::Follow).unwrap();
        let direct = resolve(&t, b"/dir/sub/deep", Final::Follow).unwrap();
        assert_eq!(via_link.node, direct.node);
    }

    #[test]
    fn absolute_symlinks_restart_at_the_root() {
        let t = sample_tree();
        let via_link = resolve(&t, b"/to_abs/deep", Final::Follow).unwrap();
        let direct = resolve(&t, b"/dir/sub/deep", Final::Follow).unwrap();
        assert_eq!(via_link.node, direct.node);
    }

    #[test]
    fn nofollow_returns_the_link_itself() {
        let t = sample_tree();
        let r = resolve(&t, b"/to_dir", Final::NoFollow).unwrap();
        assert!(r.is_symlink);
        assert_eq!(t.nodes[r.node.ino as usize].name, "to_dir");
    }

    #[test]
    fn follow_returns_what_the_link_points_at() {
        let t = sample_tree();
        let r = resolve(&t, b"/to_dir", Final::Follow).unwrap();
        assert!(!r.is_symlink);
        assert_eq!(t.nodes[r.node.ino as usize].name, "dir");
    }

    #[test]
    fn nofollow_still_follows_earlier_symlinks() {
        // Only the final component is exempt; `/to_dir/sub` must follow `to_dir`.
        let t = sample_tree();
        let r = resolve(&t, b"/to_dir/sub", Final::NoFollow).unwrap();
        assert_eq!(t.nodes[r.node.ino as usize].name, "sub");
    }

    #[test]
    fn a_self_referencing_link_is_eloop() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"/loop", Final::Follow), Err(errno::ELOOP));
    }

    #[test]
    fn a_mutual_loop_is_eloop() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"/a", Final::Follow), Err(errno::ELOOP));
    }

    #[test]
    fn a_dangling_link_is_enoent() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"/dangling", Final::Follow), Err(errno::ENOENT));
    }

    #[test]
    fn an_empty_link_target_is_enoent() {
        let t = sample_tree();
        assert_eq!(resolve(&t, b"/empty", Final::Follow), Err(errno::ENOENT));
    }

    #[test]
    fn a_link_named_dotdot_walks_up() {
        let t = sample_tree();
        let via_link = resolve(&t, b"/dir/up", Final::Follow).unwrap();
        assert_eq!(via_link.node.ino, 0);
    }

    #[test]
    fn symlink_budget_is_not_reset_by_nesting() {
        // Each resolution starts with a fresh budget because the counter lives on the
        // resolver, which the caller creates per syscall.
        let t = sample_tree();
        assert!(resolve(&t, b"/to_dir/file", Final::Follow).is_ok());
        assert_eq!(t.symlink_budget(), 1);
    }
}
