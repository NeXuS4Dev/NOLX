//! The Security Reference Monitor's decision rules: principals, access-control lists, tokens,
//! integrity labels, privileges, and `SeAccessCheck`.
//!
//! The monitor answers one question: given a token and an object's security descriptor, which
//! of the rights the caller asked for may it have? Everything else in the kernel -- handle
//! creation, `access(2)`, `open(2)` -- calls this, so the rules live once.
//!
//! The evaluation order is the one NT uses, with NOLX's simplifications listed where they
//! apply:
//!
//! 1. **Mandatory label.** An object with an integrity label refuses write-class rights
//!    (`WRITE`, `DELETE`, `WRITE_DAC`) to a token whose integrity level is lower. Reads are not
//!    restricted by the label. This is the "no write up" policy.
//! 2. **Owner.** The owner always holds `READ_CONTROL` and `WRITE_DAC`, whatever the DACL says,
//!    so an owner can always repair a descriptor it has locked itself out of.
//! 3. **DACL, in order.** A `Deny` entry that matches the token and covers a still-wanted right
//!    refuses the request at once. An `Allow` entry grants the rights it names that are still
//!    wanted. Order is what makes a deny written before an allow mean something.
//! 4. **Privileges.** `BACKUP` grants reads and `RESTORE` grants writes when the DACL has not
//!    already granted them. A deny entry is not overridden by a privilege, which is stricter
//!    than NT's backup semantics and is recorded here as a deliberate choice.
//!
//! A null DACL (`dacl: None`) grants everything asked for, as in NT. An empty DACL denies
//! everything.
//!
//! Every decision can be recorded in an [`AuditLog`], which is bounded: the newest records
//! survive and the totals are kept exactly.

/// The rights a request can ask for.
pub mod rights {
    /// Read the object's contents.
    pub const READ: u32 = 1 << 0;
    /// Change the object's contents.
    pub const WRITE: u32 = 1 << 1;
    /// Run the object as code.
    pub const EXECUTE: u32 = 1 << 2;
    /// Remove the object.
    pub const DELETE: u32 = 1 << 3;
    /// Read the security descriptor.
    pub const READ_CONTROL: u32 = 1 << 4;
    /// Change the DACL.
    pub const WRITE_DAC: u32 = 1 << 5;
    /// Wait on the object's signaled state.
    ///
    /// A waitable is not readable data: an input device, an event or a timer can be waited on
    /// without being read, and a caller that may only read cannot necessarily wait. Keeping the
    /// right separate is what lets a device grant "read the reports" and "wait for a report" as
    /// two decisions, which is exactly how the input bindings use it.
    pub const SYNCHRONIZE: u32 = 1 << 6;
    /// Every right defined here.
    pub const ALL: u32 = READ | WRITE | EXECUTE | DELETE | READ_CONTROL | WRITE_DAC | SYNCHRONIZE;
    /// The rights the integrity label restricts.
    pub const WRITE_CLASS: u32 = WRITE | DELETE | WRITE_DAC;
}

/// Privileges a token may hold.
pub mod privilege {
    /// Read any object, as backup software needs to.
    pub const BACKUP: u32 = 1 << 0;
    /// Write any object, as restore software needs to.
    pub const RESTORE: u32 = 1 << 1;
}

/// A security identifier: who a token or an ACE refers to.
///
/// POSIX identities project into this space: a uid is `Sid::user(uid)` and a gid is
/// `Sid::group(gid)`, so `access(2)` and `open(2)` can use the same rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sid {
    authority: u8,
    value: u32,
}

impl Sid {
    /// Every token matches this; it is the "everyone" principal.
    pub const WORLD: Sid = Sid {
        authority: 1,
        value: 0,
    };
    /// The kernel's own principal.
    pub const SYSTEM: Sid = Sid {
        authority: 5,
        value: 18,
    };

    /// The principal for POSIX user `uid`.
    #[must_use]
    pub const fn user(uid: u32) -> Sid {
        Sid {
            authority: 2,
            value: uid,
        }
    }

    /// The principal for POSIX group `gid`.
    #[must_use]
    pub const fn group(gid: u32) -> Sid {
        Sid {
            authority: 3,
            value: gid,
        }
    }
}

/// Integrity levels, lowest first. The numbers are the ones NT uses, so a dump reads the same.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum IntegrityLevel {
    /// Untrusted code, such as a sandboxed process.
    Low = 0x1000,
    /// Ordinary user processes.
    Medium = 0x2000,
    /// Elevated administrative processes.
    High = 0x3000,
    /// The kernel and its services.
    System = 0x4000,
}

/// Whether an entry grants or refuses rights.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AceKind {
    /// Grants the rights in the mask.
    Allow,
    /// Refuses the rights in the mask.
    Deny,
}

/// One access-control entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ace {
    /// Grant or refuse.
    pub kind: AceKind,
    /// The principal the entry applies to.
    pub sid: Sid,
    /// The rights it covers.
    pub mask: u32,
}

/// The most entries an ACL can hold. Fixed, because a descriptor lives inside an object's
/// bookkeeping and must not allocate.
pub const MAX_ACES: usize = 8;

/// The most group memberships a token can carry.
pub const MAX_GROUPS: usize = 8;

/// Why a fixed-capacity structure refused an entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SrmError {
    /// The ACL or the group list is full.
    Full,
}

/// An ordered list of access-control entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Acl {
    aces: [Ace; MAX_ACES],
    len: usize,
}

impl Acl {
    /// An ACL with no entries, which denies everything.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            aces: [Ace {
                kind: AceKind::Deny,
                sid: Sid::WORLD,
                mask: 0,
            }; MAX_ACES],
            len: 0,
        }
    }

    /// Append an entry. Order matters: entries are evaluated first to last.
    ///
    /// # Errors
    ///
    /// [`SrmError::Full`] if the ACL already holds [`MAX_ACES`] entries.
    pub fn push(&mut self, ace: Ace) -> Result<(), SrmError> {
        if self.len == MAX_ACES {
            return Err(SrmError::Full);
        }
        self.aces[self.len] = ace;
        self.len += 1;
        Ok(())
    }

    /// The entries, in evaluation order.
    #[must_use]
    pub fn aces(&self) -> &[Ace] {
        &self.aces[..self.len]
    }
}

/// The security identity a request runs under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    user: Sid,
    groups: [Sid; MAX_GROUPS],
    group_count: usize,
    privileges: u32,
    integrity: IntegrityLevel,
}

impl Token {
    /// A token for `user` at `integrity`, with no groups and no privileges.
    #[must_use]
    pub const fn new(user: Sid, integrity: IntegrityLevel) -> Self {
        Self {
            user,
            groups: [Sid::WORLD; MAX_GROUPS],
            group_count: 0,
            privileges: 0,
            integrity,
        }
    }

    /// Add a group membership.
    ///
    /// # Errors
    ///
    /// [`SrmError::Full`] if the token already has [`MAX_GROUPS`] groups.
    pub fn with_group(mut self, group: Sid) -> Result<Self, SrmError> {
        if self.group_count == MAX_GROUPS {
            return Err(SrmError::Full);
        }
        self.groups[self.group_count] = group;
        self.group_count += 1;
        Ok(self)
    }

    /// Grant the privileges in `privileges` (a mask from [`privilege`]).
    #[must_use]
    pub const fn with_privileges(mut self, privileges: u32) -> Self {
        self.privileges |= privileges;
        self
    }

    /// The token's user principal.
    #[must_use]
    pub const fn user(&self) -> Sid {
        self.user
    }

    /// The token's integrity level.
    #[must_use]
    pub const fn integrity(&self) -> IntegrityLevel {
        self.integrity
    }

    /// Whether the token holds every privilege in `mask`.
    #[must_use]
    pub const fn has_privilege(&self, mask: u32) -> bool {
        self.privileges & mask == mask
    }

    /// Whether an ACE naming `sid` applies to this token: it is the user, one of its groups,
    /// or the everyone principal.
    #[must_use]
    pub fn matches(&self, sid: Sid) -> bool {
        sid == Sid::WORLD || sid == self.user || self.groups[..self.group_count].contains(&sid)
    }
}

/// An object's security descriptor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecurityDescriptor {
    /// The owner principal.
    pub owner: Sid,
    /// The discretionary ACL. `None` is a null DACL, which grants everything.
    pub dacl: Option<Acl>,
    /// The mandatory integrity label, if the object has one.
    pub label: Option<IntegrityLevel>,
}

/// The outcome of an access check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Every requested right is granted; the mask is what was granted.
    Granted(u32),
    /// At least one requested right is refused; `missing` is what was refused.
    Denied {
        /// The rights that were asked for and not granted.
        missing: u32,
    },
}

/// `SeAccessCheck`: decide which of `desired` `token` may have on an object described by
/// `descriptor`. See the module documentation for the order of evaluation.
#[must_use]
pub fn access_check(token: &Token, descriptor: &SecurityDescriptor, desired: u32) -> Decision {
    if let Some(label) = descriptor.label {
        let write_class = desired & rights::WRITE_CLASS;
        if token.integrity() < label && write_class != 0 {
            return Decision::Denied {
                missing: write_class,
            };
        }
    }

    let Some(acl) = descriptor.dacl else {
        return Decision::Granted(desired);
    };

    let mut granted = if token.user() == descriptor.owner {
        desired & (rights::READ_CONTROL | rights::WRITE_DAC)
    } else {
        0
    };
    let mut remaining = desired & !granted;

    for ace in acl.aces() {
        if remaining == 0 {
            break;
        }
        if !token.matches(ace.sid) {
            continue;
        }
        match ace.kind {
            AceKind::Deny => {
                let refused = ace.mask & remaining;
                if refused != 0 {
                    return Decision::Denied { missing: refused };
                }
            }
            AceKind::Allow => {
                granted |= ace.mask & remaining;
                remaining = desired & !granted;
            }
        }
    }

    if remaining != 0 {
        let mut bypass = 0;
        if token.has_privilege(privilege::BACKUP) {
            bypass |= rights::READ | rights::READ_CONTROL;
        }
        if token.has_privilege(privilege::RESTORE) {
            bypass |= rights::WRITE | rights::DELETE;
        }
        granted |= remaining & bypass;
        remaining = desired & !granted;
    }

    if remaining == 0 {
        Decision::Granted(granted)
    } else {
        Decision::Denied { missing: remaining }
    }
}

/// One recorded access decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// The user the decision was made for.
    pub subject: Sid,
    /// The rights that were asked for.
    pub desired: u32,
    /// Whether every requested right was granted.
    pub success: bool,
}

/// A bounded log of access decisions. The newest `N` records are kept; the totals are exact
/// whatever was evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AuditLog<const N: usize> {
    records: [Option<AuditRecord>; N],
    next: usize,
    total: u64,
    failures: u64,
}

impl<const N: usize> AuditLog<N> {
    /// An empty log, usable in a `static` initialiser.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            records: [None; N],
            next: 0,
            total: 0,
            failures: 0,
        }
    }

    /// Record a decision, evicting the oldest record if the log is full.
    pub fn record(&mut self, record: AuditRecord) {
        if N == 0 {
            return;
        }
        self.records[self.next] = Some(record);
        self.next = (self.next + 1) % N;
        self.total += 1;
        if !record.success {
            self.failures += 1;
        }
    }

    /// Every decision ever recorded, including evicted ones.
    #[must_use]
    pub const fn total(&self) -> u64 {
        self.total
    }

    /// Every refused decision ever recorded, including evicted ones.
    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    /// The most recent record, if any.
    #[must_use]
    pub fn last(&self) -> Option<AuditRecord> {
        if N == 0 || self.total == 0 {
            return None;
        }
        let previous = (self.next + N - 1) % N;
        self.records[previous]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;

    fn descriptor(owner: Sid, dacl: Option<Acl>) -> SecurityDescriptor {
        SecurityDescriptor {
            owner,
            dacl,
            label: None,
        }
    }

    fn acl(entries: &[Ace]) -> Acl {
        let mut acl = Acl::empty();
        for entry in entries {
            acl.push(*entry).unwrap();
        }
        acl
    }

    fn allow(sid: Sid, mask: u32) -> Ace {
        Ace {
            kind: AceKind::Allow,
            sid,
            mask,
        }
    }

    fn deny(sid: Sid, mask: u32) -> Ace {
        Ace {
            kind: AceKind::Deny,
            sid,
            mask,
        }
    }

    fn alice() -> Token {
        Token::new(Sid::user(ALICE), IntegrityLevel::Medium)
    }

    #[test]
    fn null_dacl_grants_everything_and_empty_dacl_denies_everything() {
        let null = descriptor(Sid::SYSTEM, None);
        assert_eq!(
            access_check(&alice(), &null, rights::ALL),
            Decision::Granted(rights::ALL)
        );
        let empty = descriptor(Sid::SYSTEM, Some(Acl::empty()));
        assert_eq!(
            access_check(&alice(), &empty, rights::READ),
            Decision::Denied {
                missing: rights::READ
            }
        );
    }

    #[test]
    fn owner_holds_read_control_and_write_dac_implicitly_but_not_read() {
        let owned = descriptor(Sid::user(ALICE), Some(Acl::empty()));
        assert_eq!(
            access_check(&alice(), &owned, rights::READ_CONTROL | rights::WRITE_DAC),
            Decision::Granted(rights::READ_CONTROL | rights::WRITE_DAC)
        );
        assert_eq!(
            access_check(&alice(), &owned, rights::READ),
            Decision::Denied {
                missing: rights::READ
            }
        );
    }

    #[test]
    fn deny_written_before_allow_wins_for_the_same_principal() {
        let dacl = acl(&[
            deny(Sid::user(BOB), rights::WRITE),
            allow(Sid::WORLD, rights::ALL),
        ]);
        let d = descriptor(Sid::SYSTEM, Some(dacl));
        let bob = Token::new(Sid::user(BOB), IntegrityLevel::Medium);
        assert_eq!(
            access_check(&bob, &d, rights::WRITE),
            Decision::Denied {
                missing: rights::WRITE
            }
        );
        assert_eq!(
            access_check(&bob, &d, rights::READ),
            Decision::Granted(rights::READ)
        );
        assert_eq!(
            access_check(&alice(), &d, rights::WRITE),
            Decision::Granted(rights::WRITE)
        );
    }

    #[test]
    fn allow_written_before_deny_is_not_undone_by_the_later_deny() {
        // Once an allow has granted a right, a later deny for the same right has nothing left
        // to refuse. The order is part of the contract.
        let dacl = acl(&[
            allow(Sid::user(ALICE), rights::WRITE),
            deny(Sid::WORLD, rights::WRITE),
        ]);
        let d = descriptor(Sid::SYSTEM, Some(dacl));
        assert_eq!(
            access_check(&alice(), &d, rights::WRITE),
            Decision::Granted(rights::WRITE)
        );
    }

    #[test]
    fn group_membership_is_matched_through_the_token() {
        let staff = Sid::group(50);
        let dacl = acl(&[allow(staff, rights::READ)]);
        let d = descriptor(Sid::SYSTEM, Some(dacl));
        let member = alice().with_group(staff).unwrap();
        assert_eq!(
            access_check(&member, &d, rights::READ),
            Decision::Granted(rights::READ)
        );
        assert_eq!(
            access_check(&alice(), &d, rights::READ),
            Decision::Denied {
                missing: rights::READ
            }
        );
    }

    #[test]
    fn group_list_is_bounded() {
        let mut token = alice();
        for gid in 0..MAX_GROUPS as u32 {
            token = token.with_group(Sid::group(gid)).unwrap();
        }
        assert_eq!(token.with_group(Sid::group(99)), Err(SrmError::Full));
    }

    #[test]
    fn acl_is_bounded() {
        let mut acl = Acl::empty();
        for _ in 0..MAX_ACES {
            acl.push(allow(Sid::WORLD, rights::READ)).unwrap();
        }
        assert_eq!(
            acl.push(allow(Sid::WORLD, rights::READ)),
            Err(SrmError::Full)
        );
    }

    #[test]
    fn missing_rights_are_reported_exactly() {
        let dacl = acl(&[allow(Sid::user(ALICE), rights::READ)]);
        let d = descriptor(Sid::SYSTEM, Some(dacl));
        assert_eq!(
            access_check(&alice(), &d, rights::READ | rights::WRITE | rights::DELETE),
            Decision::Denied {
                missing: rights::WRITE | rights::DELETE
            }
        );
    }

    #[test]
    fn integrity_label_refuses_write_class_to_a_lower_token() {
        let dacl = acl(&[allow(Sid::WORLD, rights::ALL)]);
        let labelled = SecurityDescriptor {
            owner: Sid::SYSTEM,
            dacl: Some(dacl),
            label: Some(IntegrityLevel::Medium),
        };
        let low = Token::new(Sid::user(ALICE), IntegrityLevel::Low);
        assert_eq!(
            access_check(&low, &labelled, rights::READ),
            Decision::Granted(rights::READ),
            "reads are not restricted by the label"
        );
        assert_eq!(
            access_check(&low, &labelled, rights::WRITE),
            Decision::Denied {
                missing: rights::WRITE
            }
        );
        assert_eq!(
            access_check(&alice(), &labelled, rights::WRITE),
            Decision::Granted(rights::WRITE),
            "a token at the label's level may write"
        );
    }

    #[test]
    fn backup_privilege_reads_what_the_dacl_does_not_grant() {
        let d = descriptor(Sid::SYSTEM, Some(Acl::empty()));
        let backup = alice().with_privileges(privilege::BACKUP);
        assert_eq!(
            access_check(&backup, &d, rights::READ),
            Decision::Granted(rights::READ)
        );
        assert_eq!(
            access_check(&backup, &d, rights::WRITE),
            Decision::Denied {
                missing: rights::WRITE
            },
            "backup is read-only"
        );
    }

    #[test]
    fn restore_privilege_writes_but_does_not_override_a_deny() {
        let dacl = acl(&[deny(Sid::user(ALICE), rights::WRITE)]);
        let d = descriptor(Sid::SYSTEM, Some(dacl));
        let restore = alice().with_privileges(privilege::RESTORE);
        assert_eq!(
            access_check(&restore, &d, rights::WRITE),
            Decision::Denied {
                missing: rights::WRITE
            },
            "a deny entry is stricter than the privilege"
        );
        let open = descriptor(Sid::SYSTEM, Some(Acl::empty()));
        assert_eq!(
            access_check(&restore, &open, rights::WRITE),
            Decision::Granted(rights::WRITE)
        );
    }

    #[test]
    fn audit_log_keeps_exact_totals_past_its_capacity() {
        let mut log: AuditLog<2> = AuditLog::empty();
        assert_eq!(log.last(), None);
        for i in 0..5 {
            log.record(AuditRecord {
                subject: Sid::user(i),
                desired: rights::READ,
                success: i % 2 == 0,
            });
        }
        assert_eq!(log.total(), 5);
        assert_eq!(log.failures(), 2);
        assert_eq!(
            log.last(),
            Some(AuditRecord {
                subject: Sid::user(4),
                desired: rights::READ,
                success: true,
            })
        );
    }

    #[test]
    fn zero_capacity_audit_log_counts_without_storing() {
        let mut log: AuditLog<0> = AuditLog::empty();
        log.record(AuditRecord {
            subject: Sid::WORLD,
            desired: rights::READ,
            success: false,
        });
        assert_eq!(log.total(), 0, "nothing is recorded into a zero-size log");
        assert_eq!(log.last(), None);
    }
}
