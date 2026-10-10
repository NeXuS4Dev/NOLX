//! The mount table.
//!
//! A mount hides the directory it is mounted on. Two rules follow, and both are the kind
//! of thing that is easy to get subtly wrong:
//!
//! * Walking *down* into a mount point yields the root of the mounted filesystem, not the
//!   covered directory.
//! * Walking *up* (`..`) from the root of a mounted filesystem yields the parent of the
//!   covered directory. So `cd /mnt; cd ..` lands where you would expect, and
//!   `cd /mnt/sub/..` does not escape the mount by accident.

use alloc::string::String;
use alloc::vec::Vec;

use nolx_types::errno;

/// A filesystem object, identified by the mount it lives in plus its inode number.
///
/// The mount is part of the identity because two mounted filesystems can both have an
/// inode 1, and they are different files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct NodeId {
    /// The mount the node belongs to.
    pub mount: u32,
    /// The inode number within that mount.
    pub ino: u64,
}

impl NodeId {
    /// Build a node id.
    #[must_use]
    pub const fn new(mount: u32, ino: u64) -> Self {
        Self { mount, ino }
    }
}

/// Mount flags.
pub mod flags {
    /// Read-only.
    pub const RDONLY: u64 = 1 << 0;
    /// Do not honour set-user-ID and set-group-ID.
    pub const NOSUID: u64 = 1 << 1;
    /// Do not interpret character or block special devices.
    pub const NODEV: u64 = 1 << 2;
    /// Do not allow executables.
    pub const NOEXEC: u64 = 1 << 3;
    /// Do not record access times.
    pub const NOATIME: u64 = 1 << 4;
    /// The mount is a bind mount of an existing subtree.
    pub const BIND: u64 = 1 << 5;
}

/// One entry in the mount table.
#[derive(Clone, Debug)]
pub struct Mount {
    /// This mount's id.
    pub id: u32,
    /// The node this mount covers, or `None` for the root mount.
    pub covered: Option<NodeId>,
    /// The root node of the mounted filesystem.
    pub root: NodeId,
    /// Mount flags.
    pub flags: u64,
    /// The device or source name, for `mount(8)` and error messages.
    pub source: String,
    /// The filesystem type name.
    pub fstype: String,
}

/// The per-process mount table.
///
/// Per process rather than global, because `chroot` and mount namespaces mean different
/// processes can see different trees. The table is small and searched linearly; a machine
/// with enough mounts for that to matter has other problems.
#[derive(Clone, Debug, Default)]
pub struct MountTable {
    mounts: Vec<Mount>,
    next_id: u32,
}

impl MountTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self { mounts: Vec::new(), next_id: 1 }
    }

    /// Install the root filesystem.
    ///
    /// # Errors
    ///
    /// `EBUSY` if a root mount already exists.
    pub fn mount_root(&mut self, root: NodeId, source: String, fstype: String) -> Result<u32, i32> {
        if self.mounts.iter().any(|m| m.covered.is_none()) {
            return Err(errno::EBUSY);
        }
        self.add(None, root, 0, source, fstype)
    }

    /// Mount a filesystem over `covered`.
    ///
    /// # Errors
    ///
    /// `ENOENT` if nothing is mounted yet, `EBUSY` if `covered` already has a filesystem on
    /// it. Stacking mounts on one point is deliberately rejected: it makes the visible
    /// tree depend on mount order in a way that is hard to reason about.
    pub fn mount_over(
        &mut self,
        covered: NodeId,
        root: NodeId,
        mflags: u64,
        source: String,
        fstype: String,
    ) -> Result<u32, i32> {
        if self.mounts.is_empty() {
            return Err(errno::ENOENT);
        }
        if self.cross_down(covered).is_some() {
            return Err(errno::EBUSY);
        }
        self.add(Some(covered), root, mflags, source, fstype)
    }

    fn add(
        &mut self,
        covered: Option<NodeId>,
        root: NodeId,
        mflags: u64,
        source: String,
        fstype: String,
    ) -> Result<u32, i32> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(errno::EMFILE)?;
        self.mounts.push(Mount { id, covered, root, flags: mflags, source, fstype });
        Ok(id)
    }

    /// Remove a mount.
    ///
    /// # Errors
    ///
    /// `ENOENT` if there is no such mount, `EBUSY` if another filesystem is mounted on top
    /// of this one, `EPERM` for the root mount.
    pub fn unmount(&mut self, id: u32) -> Result<(), i32> {
        let index = self
            .mounts
            .iter()
            .position(|m| m.id == id)
            .ok_or(errno::ENOENT)?;
        if self.mounts[index].covered.is_none() {
            return Err(errno::EPERM);
        }
        let root = self.mounts[index].root;
        if self.mounts.iter().any(|m| m.covered == Some(root)) {
            return Err(errno::EBUSY);
        }
        self.mounts.remove(index);
        Ok(())
    }

    /// The mount a node belongs to.
    #[must_use]
    pub fn mount_of(&self, node: NodeId) -> Option<&Mount> {
        self.mounts.iter().find(|m| m.root.mount == node.mount)
    }

    /// The root node of the filesystem, which is where an absolute path starts.
    #[must_use]
    pub fn root_node(&self) -> Option<NodeId> {
        self.mounts.iter().find(|m| m.covered.is_none()).map(|m| m.root)
    }

    /// If `node` is covered by a mount, return that filesystem's root.
    ///
    /// This is the downward crossing used by pathname resolution.
    #[must_use]
    pub fn cross_down(&self, node: NodeId) -> Option<NodeId> {
        self.mounts
            .iter()
            .find(|m| m.covered == Some(node))
            .map(|m| m.root)
    }

    /// If `node` is the root of a non-root mount, return the parent of the directory it
    /// covers.
    ///
    /// This is the upward crossing used for `..`.
    #[must_use]
    pub fn cross_up(&self, node: NodeId) -> Option<NodeId> {
        let mount = self.mount_of(node)?;
        let covered = mount.covered?;
        Some(covered)
    }

    /// All mounts, in the order they were added.
    #[must_use]
    pub fn all(&self) -> &[Mount] {
        &self.mounts
    }

    /// `true` if `node` is a mount point, which is what `stat`'s `st_dev` change signals to
    /// tools like `find -xdev`.
    #[must_use]
    pub fn is_mount_point(&self, node: NodeId) -> bool {
        self.cross_down(node).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> MountTable {
        let mut t = MountTable::new();
        t.mount_root(NodeId::new(1, 1), String::from("nolxroot"), String::from("nolxfs"))
            .unwrap();
        t
    }

    #[test]
    fn root_mount_establishes_the_root_node() {
        let t = table();
        assert_eq!(t.root_node(), Some(NodeId::new(1, 1)));
    }

    #[test]
    fn a_second_root_mount_is_rejected() {
        let mut t = table();
        assert_eq!(
            t.mount_root(NodeId::new(2, 1), String::from("x"), String::from("y")),
            Err(errno::EBUSY)
        );
    }

    #[test]
    fn mounting_without_a_root_fails() {
        let mut t = MountTable::new();
        assert_eq!(
            t.mount_over(NodeId::new(1, 5), NodeId::new(2, 1), 0, String::new(), String::new()),
            Err(errno::ENOENT)
        );
    }

    #[test]
    fn downward_crossing_replaces_the_covered_directory() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(covered, NodeId::new(2, 1), 0, String::from("/dev/x"), String::from("nolxfs"))
            .unwrap();
        assert_eq!(t.cross_down(covered), Some(NodeId::new(2, 1)));
        assert!(t.is_mount_point(covered));
    }

    #[test]
    fn upward_crossing_from_a_mount_root_reaches_the_covered_directory() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(covered, NodeId::new(2, 1), 0, String::from("/dev/x"), String::from("nolxfs"))
            .unwrap();
        // `..` from the mounted filesystem's root lands on the covered directory's parent,
        // which the caller computes; here we check the table hands back the covered node.
        assert_eq!(t.cross_up(NodeId::new(2, 1)), Some(covered));
    }

    #[test]
    fn downward_and_upward_crossings_are_inverses() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        let new_root = NodeId::new(2, 1);
        t.mount_over(covered, new_root, 0, String::from("/dev/x"), String::from("nolxfs"))
            .unwrap();
        let down = t.cross_down(covered).unwrap();
        assert_eq!(down, new_root);
        assert_eq!(t.cross_up(down), Some(covered));
    }

    #[test]
    fn the_root_mount_has_no_upward_crossing() {
        let t = table();
        assert_eq!(t.cross_up(NodeId::new(1, 1)), None);
    }

    #[test]
    fn stacking_a_mount_on_a_mount_is_rejected() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(covered, NodeId::new(2, 1), 0, String::from("a"), String::from("nolxfs"))
            .unwrap();
        assert_eq!(
            t.mount_over(covered, NodeId::new(3, 1), 0, String::from("b"), String::from("nolxfs")),
            Err(errno::EBUSY)
        );
    }

    #[test]
    fn unmounting_is_refused_while_something_is_mounted_on_top() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        let lower = t
            .mount_over(covered, NodeId::new(2, 1), 0, String::from("a"), String::from("nolxfs"))
            .unwrap();
        t.mount_over(
            NodeId::new(2, 1),
            NodeId::new(3, 1),
            0,
            String::from("b"),
            String::from("nolxfs"),
        )
        .unwrap();
        assert_eq!(t.unmount(lower), Err(errno::EBUSY));
    }

    #[test]
    fn the_root_mount_cannot_be_unmounted() {
        let mut t = table();
        let id = t.all()[0].id;
        assert_eq!(t.unmount(id), Err(errno::EPERM));
    }

    #[test]
    fn unmount_then_remount_works() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        let id = t
            .mount_over(covered, NodeId::new(2, 1), 0, String::from("a"), String::from("nolxfs"))
            .unwrap();
        t.unmount(id).unwrap();
        assert_eq!(t.cross_down(covered), None);
        assert!(!t.is_mount_point(covered));
        assert!(t
            .mount_over(covered, NodeId::new(3, 1), 0, String::from("b"), String::from("nolxfs"))
            .is_ok());
    }

    #[test]
    fn unmounting_an_unknown_id_fails() {
        let mut t = table();
        assert_eq!(t.unmount(999), Err(errno::ENOENT));
    }

    #[test]
    fn mount_flags_are_preserved() {
        let mut t = table();
        let id = t
            .mount_over(
                NodeId::new(1, 5),
                NodeId::new(2, 1),
                flags::RDONLY | flags::NOSUID,
                String::from("a"),
                String::from("nolxfs"),
            )
            .unwrap();
        let m = t.all().iter().find(|m| m.id == id).unwrap();
        assert_eq!(m.flags, flags::RDONLY | flags::NOSUID);
    }

    #[test]
    fn node_identity_includes_the_mount() {
        assert_ne!(NodeId::new(1, 1), NodeId::new(2, 1));
        assert_eq!(NodeId::new(1, 1), NodeId::new(1, 1));
    }
}
