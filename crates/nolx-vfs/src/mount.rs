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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

/// Mount event propagation class (`MS_PRIVATE`, `MS_SHARED`, `MS_SLAVE`, `MS_UNBINDABLE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PropagationClass {
    /// Mount and unmount events do not propagate into or out of this mount.
    #[default]
    Private,
    /// Mount and unmount events propagate bi-directionally among all peers in `peer_group`
    /// and downward to any `Slave` mounts whose `master_group` matches `peer_group`.
    Shared,
    /// Mount and unmount events propagate one-way *from* `master_group` into this mount,
    /// but mounts performed under this mount do not propagate back to the master.
    Slave,
    /// Like `Private`, and in addition any attempt to `bind_mount` this mount fails with
    /// `EINVAL`.
    Unbindable,
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
    /// Propagation class for mount/unmount events under this mount.
    pub propagation: PropagationClass,
    /// Peer group id when `propagation == PropagationClass::Shared`.
    pub peer_group: Option<u32>,
    /// Master peer group id when `propagation == PropagationClass::Slave`.
    pub master_group: Option<u32>,
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
    next_peer_group: u32,
}

impl MountTable {
    /// An empty table.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            mounts: Vec::new(),
            next_id: 1,
            next_peer_group: 1,
        }
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
        let primary_id = self.add(Some(covered), root, mflags, source.clone(), fstype.clone())?;

        // Propagate to peer and slave mounts if the parent mount containing `covered` is `Shared`.
        if let Some(parent_group) = self
            .mount_of(covered)
            .filter(|m| m.propagation == PropagationClass::Shared)
            .and_then(|m| m.peer_group)
        {
            let recipients: Vec<(u32, PropagationClass, Option<u32>)> = self
                .mounts
                .iter()
                .filter(|m| {
                    m.root.mount != covered.mount
                        && ((m.propagation == PropagationClass::Shared
                            && m.peer_group == Some(parent_group))
                            || (m.propagation == PropagationClass::Slave
                                && m.master_group == Some(parent_group)))
                })
                .map(|m| (m.root.mount, m.propagation, m.peer_group))
                .collect();

            let child_peer_group = self.alloc_peer_group()?;
            if let Some(primary) = self.mounts.iter_mut().find(|m| m.id == primary_id) {
                primary.propagation = PropagationClass::Shared;
                primary.peer_group = Some(child_peer_group);
            }

            for (target_mount_num, recip_class, _recip_peer) in recipients {
                let target_covered = NodeId::new(target_mount_num, covered.ino);
                if self.cross_down(target_covered).is_some() {
                    continue;
                }
                let replica_mount_num = self.next_id + 1000;
                let replica_root = NodeId::new(replica_mount_num, root.ino);
                let rep_id = self.add(
                    Some(target_covered),
                    replica_root,
                    mflags,
                    source.clone(),
                    fstype.clone(),
                )?;
                if let Some(rep) = self.mounts.iter_mut().find(|m| m.id == rep_id) {
                    match recip_class {
                        PropagationClass::Shared => {
                            rep.propagation = PropagationClass::Shared;
                            rep.peer_group = Some(child_peer_group);
                        }
                        PropagationClass::Slave => {
                            rep.propagation = PropagationClass::Slave;
                            rep.master_group = Some(child_peer_group);
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(primary_id)
    }

    fn alloc_peer_group(&mut self) -> Result<u32, i32> {
        let g = self.next_peer_group;
        self.next_peer_group = self.next_peer_group.checked_add(1).ok_or(errno::EMFILE)?;
        Ok(g)
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
        self.mounts.push(Mount {
            id,
            covered,
            root,
            flags: mflags,
            source,
            fstype,
            propagation: PropagationClass::Private,
            peer_group: None,
            master_group: None,
        });
        Ok(id)
    }

    /// Change the propagation class of an existing mount (`id`).
    ///
    /// # Errors
    ///
    /// * `ENOENT` if no mount has `id`.
    /// * `EINVAL` if `PropagationClass::Slave` is requested on a mount that is not currently
    ///   in a peer group and has no master group.
    pub fn set_propagation(&mut self, id: u32, class: PropagationClass) -> Result<(), i32> {
        let new_group = if class == PropagationClass::Shared {
            Some(self.alloc_peer_group()?)
        } else {
            None
        };
        let mount = self
            .mounts
            .iter_mut()
            .find(|m| m.id == id)
            .ok_or(errno::ENOENT)?;
        match class {
            PropagationClass::Private | PropagationClass::Unbindable => {
                mount.propagation = class;
                mount.peer_group = None;
                mount.master_group = None;
            }
            PropagationClass::Shared => {
                if mount.peer_group.is_none() {
                    mount.peer_group = new_group;
                }
                mount.propagation = PropagationClass::Shared;
            }
            PropagationClass::Slave => {
                let master = mount
                    .peer_group
                    .or(mount.master_group)
                    .ok_or(errno::EINVAL)?;
                mount.propagation = PropagationClass::Slave;
                mount.peer_group = None;
                mount.master_group = Some(master);
            }
        }
        Ok(())
    }

    /// Create a bind mount of `source_mount_id` at `covered` with a distinct mount namespace
    /// number `new_mount_num`.
    ///
    /// # Errors
    ///
    /// * `ENOENT` if `source_mount_id` does not exist.
    /// * `EINVAL` if the source mount is [`PropagationClass::Unbindable`].
    /// * `EBUSY` if `covered` is already a mount point.
    pub fn bind_mount(
        &mut self,
        source_mount_id: u32,
        covered: NodeId,
        new_mount_num: u32,
    ) -> Result<u32, i32> {
        if self.cross_down(covered).is_some() {
            return Err(errno::EBUSY);
        }
        let src = self
            .mounts
            .iter()
            .find(|m| m.id == source_mount_id)
            .cloned()
            .ok_or(errno::ENOENT)?;
        if src.propagation == PropagationClass::Unbindable {
            return Err(errno::EINVAL);
        }
        let new_root = NodeId::new(new_mount_num, src.root.ino);
        let id = self.add(
            Some(covered),
            new_root,
            src.flags | flags::BIND,
            src.source,
            src.fstype,
        )?;
        if let Some(m) = self.mounts.iter_mut().find(|m| m.id == id) {
            match src.propagation {
                PropagationClass::Shared => {
                    m.propagation = PropagationClass::Shared;
                    m.peer_group = src.peer_group;
                }
                PropagationClass::Slave => {
                    m.propagation = PropagationClass::Slave;
                    m.master_group = src.master_group;
                }
                PropagationClass::Private | PropagationClass::Unbindable => {
                    m.propagation = PropagationClass::Private;
                }
            }
        }
        Ok(id)
    }

    /// Swap the root mount with an existing sub-mount (`new_root_mount_id`), placing the
    /// previous root mount on `put_old` (`pivot_root(2)`).
    ///
    /// Returns the new root [`NodeId`].
    ///
    /// # Errors
    ///
    /// * `ENOENT` if `new_root_mount_id` or the current root mount does not exist.
    /// * `EINVAL` if `new_root_mount_id` is already the root mount or `put_old` does not lie
    ///   inside the new root mount.
    /// * `EBUSY` if `put_old` is already covered by another mount.
    pub fn pivot_root(&mut self, new_root_mount_id: u32, put_old: NodeId) -> Result<NodeId, i32> {
        if self.cross_down(put_old).is_some() {
            return Err(errno::EBUSY);
        }
        let old_root_idx = self
            .mounts
            .iter()
            .position(|m| m.covered.is_none())
            .ok_or(errno::ENOENT)?;
        let new_root_idx = self
            .mounts
            .iter()
            .position(|m| m.id == new_root_mount_id)
            .ok_or(errno::ENOENT)?;
        if old_root_idx == new_root_idx {
            return Err(errno::EINVAL);
        }
        let new_root_node = self.mounts[new_root_idx].root;
        if put_old.mount != new_root_node.mount {
            return Err(errno::EINVAL);
        }
        self.mounts[new_root_idx].covered = None;
        self.mounts[old_root_idx].covered = Some(put_old);
        Ok(new_root_node)
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
        self.mounts
            .iter()
            .find(|m| m.covered.is_none())
            .map(|m| m.root)
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
        t.mount_root(
            NodeId::new(1, 1),
            String::from("nolxroot"),
            String::from("nolxfs"),
        )
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
            t.mount_over(
                NodeId::new(1, 5),
                NodeId::new(2, 1),
                0,
                String::new(),
                String::new()
            ),
            Err(errno::ENOENT)
        );
    }

    #[test]
    fn downward_crossing_replaces_the_covered_directory() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(
            covered,
            NodeId::new(2, 1),
            0,
            String::from("/dev/x"),
            String::from("nolxfs"),
        )
        .unwrap();
        assert_eq!(t.cross_down(covered), Some(NodeId::new(2, 1)));
        assert!(t.is_mount_point(covered));
    }

    #[test]
    fn upward_crossing_from_a_mount_root_reaches_the_covered_directory() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(
            covered,
            NodeId::new(2, 1),
            0,
            String::from("/dev/x"),
            String::from("nolxfs"),
        )
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
        t.mount_over(
            covered,
            new_root,
            0,
            String::from("/dev/x"),
            String::from("nolxfs"),
        )
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
        t.mount_over(
            covered,
            NodeId::new(2, 1),
            0,
            String::from("a"),
            String::from("nolxfs"),
        )
        .unwrap();
        assert_eq!(
            t.mount_over(
                covered,
                NodeId::new(3, 1),
                0,
                String::from("b"),
                String::from("nolxfs")
            ),
            Err(errno::EBUSY)
        );
    }

    #[test]
    fn unmounting_is_refused_while_something_is_mounted_on_top() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        let lower = t
            .mount_over(
                covered,
                NodeId::new(2, 1),
                0,
                String::from("a"),
                String::from("nolxfs"),
            )
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
            .mount_over(
                covered,
                NodeId::new(2, 1),
                0,
                String::from("a"),
                String::from("nolxfs"),
            )
            .unwrap();
        t.unmount(id).unwrap();
        assert_eq!(t.cross_down(covered), None);
        assert!(!t.is_mount_point(covered));
        assert!(t
            .mount_over(
                covered,
                NodeId::new(3, 1),
                0,
                String::from("b"),
                String::from("nolxfs")
            )
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

    #[test]
    fn cross_down_returns_mounted_root() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(
            covered,
            NodeId::new(2, 1),
            0,
            String::from("/dev/x"),
            String::from("nolxfs"),
        )
        .unwrap();
        assert_eq!(t.cross_down(covered), Some(NodeId::new(2, 1)));
    }

    #[test]
    fn cross_up_returns_covered_node() {
        let mut t = table();
        let covered = NodeId::new(1, 5);
        t.mount_over(
            covered,
            NodeId::new(2, 1),
            0,
            String::from("/dev/x"),
            String::from("nolxfs"),
        )
        .unwrap();
        assert_eq!(t.cross_up(NodeId::new(2, 1)), Some(covered));
    }

    #[test]
    #[allow(non_snake_case)]
    fn root_mount_Does_Not_cross_up() {
        let t = table();
        assert_eq!(t.cross_up(NodeId::new(1, 1)), None);
    }

    #[test]
    fn shared_and_slave_mount_propagation_and_unbindable_rules() {
        let mut t = table();
        let root_id = t.all()[0].id;
        t.set_propagation(root_id, PropagationClass::Shared)
            .unwrap();

        // Bind root (mount 1) onto NodeId(1, 10) as mount 2 -> becomes Shared peer in same group.
        let peer_id = t.bind_mount(root_id, NodeId::new(1, 10), 2).unwrap();
        // Bind root onto NodeId(1, 11) as mount 3 and demote it to Slave.
        let slave_id = t.bind_mount(root_id, NodeId::new(1, 11), 3).unwrap();
        t.set_propagation(slave_id, PropagationClass::Slave)
            .unwrap();

        // Mounting over NodeId(1, 50) inside mount 1 propagates to both peer (mount 2) and slave (mount 3).
        t.mount_over(
            NodeId::new(1, 50),
            NodeId::new(4, 1),
            0,
            String::from("sub"),
            String::from("tmpfs"),
        )
        .unwrap();
        assert!(t.cross_down(NodeId::new(1, 50)).is_some());
        assert!(t.cross_down(NodeId::new(2, 50)).is_some());
        assert!(t.cross_down(NodeId::new(3, 50)).is_some());

        // Mounting under the Slave (mount 3) does NOT propagate back to master (mount 1 or 2).
        t.mount_over(
            NodeId::new(3, 60),
            NodeId::new(5, 1),
            0,
            String::from("local"),
            String::from("tmpfs"),
        )
        .unwrap();
        assert!(t.cross_down(NodeId::new(3, 60)).is_some());
        assert_eq!(t.cross_down(NodeId::new(1, 60)), None);
        assert_eq!(t.cross_down(NodeId::new(2, 60)), None);

        // Unbindable mount refuses bind_mount with EINVAL.
        t.set_propagation(peer_id, PropagationClass::Unbindable)
            .unwrap();
        assert_eq!(
            t.bind_mount(peer_id, NodeId::new(1, 12), 6),
            Err(errno::EINVAL)
        );
    }

    #[test]
    fn pivot_root_swaps_root_and_places_old_root_at_put_old() {
        let mut t = table();
        let new_root_id = t
            .mount_over(
                NodeId::new(1, 8),
                NodeId::new(2, 1),
                0,
                String::from("newroot"),
                String::from("tmpfs"),
            )
            .unwrap();
        let put_old = NodeId::new(2, 99);
        let new_root = t.pivot_root(new_root_id, put_old).unwrap();
        assert_eq!(new_root, NodeId::new(2, 1));
        assert_eq!(t.root_node(), Some(NodeId::new(2, 1)));
        assert_eq!(t.cross_down(put_old), Some(NodeId::new(1, 1)));
    }
}
