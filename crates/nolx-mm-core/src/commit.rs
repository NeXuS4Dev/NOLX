//! Commit charge and pool accounting.
//!
//! Two things are counted here, and they are counted in one place on purpose:
//!
//! * **Commit charge** -- how many bytes a process has been promised. Charging before a
//!   mapping is created and releasing after it is destroyed is what makes `RLIMIT_AS`,
//!   `RLIMIT_DATA`, and `getrusage` agree with each other: they are all reads of these
//!   counters, not separate bookkeeping that can drift.
//! * **Pools** -- the kernel's own memory, split into a nonpaged pool, a paged pool, and
//!   the system PTE pool. The split is not cosmetic: the nonpaged pool is memory that must
//!   stay resident (anything reachable at or above `DISPATCH_LEVEL`), the paged pool may be
//!   paged out and is therefore illegal to touch at those levels, and the system PTE pool
//!   is the address space for temporary kernel mappings. Charging each kind against its own
//!   limit is what turns "the paged pool is exhausted" into a failed allocation instead of a
//!   system that quietly stop being able to page.
//!
//! The rules the types enforce:
//!
//! * A charge that would exceed the limit fails *and changes nothing*. A caller that has
//!   already committed to the operation must therefore charge first and allocate second,
//!   and release if the allocation fails. [`CommitCharge::charge`] returning `Ok` is a
//!   promise; nothing after it is allowed to fail for want of commit.
//! * Counters cannot overflow: a charge is compared against the limit before it is added,
//!   so `current` can never wrap.
//! * Releasing more than is charged is reported rather than silently clamped. A kernel
//!   whose accounting has gone wrong should find out at the release site, where the caller
//!   that got the arithmetic wrong is still identifiable.

use core::fmt;

/// The commit charge: a limit, a running total, and the high-water mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitCharge {
    label: &'static str,
    limit: u64,
    current: u64,
    peak: u64,
    charges: u64,
    releases: u64,
    failures: u64,
}

impl CommitCharge {
    /// A counter with `limit` bytes of room.
    #[must_use]
    pub const fn new(label: &'static str, limit: u64) -> Self {
        Self {
            label,
            limit,
            current: 0,
            peak: 0,
            charges: 0,
            releases: 0,
            failures: 0,
        }
    }

    /// A counter with no limit, for the system-wide total that is checked against physical
    /// memory instead.
    #[must_use]
    pub const fn unlimited(label: &'static str) -> Self {
        Self::new(label, u64::MAX)
    }

    /// What this counter is called, for the boot log and for panics.
    #[must_use]
    pub const fn label(&self) -> &'static str {
        self.label
    }

    /// The limit in bytes.
    #[must_use]
    pub const fn limit(&self) -> u64 {
        self.limit
    }

    /// The bytes currently charged.
    #[must_use]
    pub const fn current(&self) -> u64 {
        self.current
    }

    /// The most bytes ever charged at once.
    #[must_use]
    pub const fn peak(&self) -> u64 {
        self.peak
    }

    /// The bytes that can still be charged before the limit is reached.
    #[must_use]
    pub const fn available(&self) -> u64 {
        self.limit - self.current
    }

    /// How many charges have succeeded.
    #[must_use]
    pub const fn charges(&self) -> u64 {
        self.charges
    }

    /// How many charges have been released.
    #[must_use]
    pub const fn releases(&self) -> u64 {
        self.releases
    }

    /// How many charges were refused.
    #[must_use]
    pub const fn failures(&self) -> u64 {
        self.failures
    }

    /// Whether `amount` bytes would fit.
    #[must_use]
    pub const fn would_fit(&self, amount: u64) -> bool {
        amount > 0 && amount <= self.available()
    }

    /// Charge `amount` bytes, returning the new total.
    ///
    /// On failure nothing is charged, so a caller may treat `Ok` as permission to proceed.
    pub fn charge(&mut self, amount: u64) -> Result<u64, CommitError> {
        if amount == 0 {
            self.failures += 1;
            return Err(CommitError::Zero);
        }
        if amount > self.available() {
            self.failures += 1;
            return Err(CommitError::Exceeded {
                requested: amount,
                available: self.available(),
            });
        }
        self.current += amount;
        self.charges += 1;
        self.peak = self.peak.max(self.current);
        Ok(self.current)
    }

    /// Charge `amount` bytes and report whether it fit, for callers that do not care why.
    pub fn try_charge(&mut self, amount: u64) -> bool {
        self.charge(amount).is_ok()
    }

    /// Release `amount` bytes, returning the new total.
    pub fn release(&mut self, amount: u64) -> Result<u64, CommitError> {
        if amount == 0 {
            return Err(CommitError::Zero);
        }
        if amount > self.current {
            return Err(CommitError::NotCharged {
                amount,
                current: self.current,
            });
        }
        self.current -= amount;
        self.releases += 1;
        Ok(self.current)
    }

    /// Raise the limit, or lower it to at least what is charged.
    ///
    /// A limit below the current charge is refused: somebody would have to decide which
    /// existing mapping to revoke, and that decision belongs to the caller, not here.
    pub fn set_limit(&mut self, limit: u64) -> Result<u64, CommitError> {
        if limit < self.current {
            return Err(CommitError::LimitBelowCurrent {
                limit,
                current: self.current,
            });
        }
        self.limit = limit;
        Ok(limit)
    }

    /// Verify the counter's own invariants.
    pub fn check(&self) -> Result<(), CommitError> {
        if self.current > self.limit {
            return Err(CommitError::Accounting("the charge is over its limit"));
        }
        if self.peak < self.current {
            return Err(CommitError::Accounting(
                "the peak is below the current charge",
            ));
        }
        if self.peak > self.limit {
            return Err(CommitError::Accounting("the peak is above the limit"));
        }
        if self.charges < self.releases {
            return Err(CommitError::Accounting("more releases than charges"));
        }
        Ok(())
    }
}

/// Why a charge or a release failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// A zero-byte charge is never what the caller meant, and it hides overflow bugs.
    Zero,
    /// The charge would take the total past the limit.
    Exceeded {
        /// The amount that was asked for.
        requested: u64,
        /// What was left under the limit.
        available: u64,
    },
    /// More bytes were released than are charged.
    NotCharged {
        /// The amount that was released.
        amount: u64,
        /// What was charged at the time.
        current: u64,
    },
    /// The requested limit is below what is already charged.
    LimitBelowCurrent {
        /// The limit that was asked for.
        limit: u64,
        /// What is charged now.
        current: u64,
    },
    /// A counter's invariants do not hold: a bug in the accounting code.
    Accounting(&'static str),
}

impl fmt::Display for CommitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => write!(formatter, "a zero-byte commit charge is not a charge"),
            Self::Exceeded {
                requested,
                available,
            } => write!(
                formatter,
                "charge of {requested} bytes exceeds the {available} bytes left under the limit"
            ),
            Self::NotCharged { amount, current } => write!(
                formatter,
                "release of {amount} bytes with only {current} bytes charged"
            ),
            Self::LimitBelowCurrent { limit, current } => write!(
                formatter,
                "a limit of {limit} bytes is below the {current} bytes already charged"
            ),
            Self::Accounting(what) => write!(formatter, "commit accounting is wrong: {what}"),
        }
    }
}

impl core::error::Error for CommitError {}

/// Which kernel pool an allocation comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PoolKind {
    /// Always-resident memory: anything that may be touched at or above `DISPATCH_LEVEL`.
    NonPaged,
    /// Pageable memory: fine at `PASSIVE_LEVEL`, illegal above it.
    Paged,
    /// Page-table entries for transient kernel mappings and I/O.
    SystemPte,
}

impl PoolKind {
    /// All three kinds, in the order they are reported.
    #[must_use]
    pub const fn all() -> [Self; 3] {
        [Self::NonPaged, Self::Paged, Self::SystemPte]
    }

    /// The name used in logs.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NonPaged => "nonpaged pool",
            Self::Paged => "paged pool",
            Self::SystemPte => "system PTE pool",
        }
    }

    /// Whether memory from this pool may be paged out.
    ///
    /// The kernel asserts this at every paged-pool allocation: a pageable allocation at or
    /// above `DISPATCH_LEVEL` is a bug that would only show up as a hang in a page fault
    /// while the scheduler is already busy.
    #[must_use]
    pub const fn is_pageable(self) -> bool {
        matches!(self, Self::Paged)
    }
}

/// The limits of the three pools, in bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PoolLimits {
    /// Limit of the nonpaged pool.
    pub non_paged: u64,
    /// Limit of the paged pool.
    pub paged: u64,
    /// Limit of the system PTE pool.
    pub system_pte: u64,
}

/// The kernel's three pools, each with its own charge and limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pools {
    non_paged: CommitCharge,
    paged: CommitCharge,
    system_pte: CommitCharge,
}

impl Pools {
    /// Build the three pools with the given limits.
    #[must_use]
    pub const fn new(limits: PoolLimits) -> Self {
        Self {
            non_paged: CommitCharge::new("nonpaged pool", limits.non_paged),
            paged: CommitCharge::new("paged pool", limits.paged),
            system_pte: CommitCharge::new("system PTE pool", limits.system_pte),
        }
    }

    /// The pool of one kind.
    #[must_use]
    pub const fn get(&self, kind: PoolKind) -> &CommitCharge {
        match kind {
            PoolKind::NonPaged => &self.non_paged,
            PoolKind::Paged => &self.paged,
            PoolKind::SystemPte => &self.system_pte,
        }
    }

    /// The pool of one kind, mutably.
    pub fn get_mut(&mut self, kind: PoolKind) -> &mut CommitCharge {
        match kind {
            PoolKind::NonPaged => &mut self.non_paged,
            PoolKind::Paged => &mut self.paged,
            PoolKind::SystemPte => &mut self.system_pte,
        }
    }

    /// Charge `amount` bytes to one pool.
    pub fn charge(&mut self, kind: PoolKind, amount: u64) -> Result<u64, CommitError> {
        self.get_mut(kind).charge(amount)
    }

    /// Release `amount` bytes from one pool.
    pub fn release(&mut self, kind: PoolKind, amount: u64) -> Result<u64, CommitError> {
        self.get_mut(kind).release(amount)
    }

    /// The bytes charged across all three pools.
    #[must_use]
    pub const fn total_charged(&self) -> u64 {
        self.non_paged.current() + self.paged.current() + self.system_pte.current()
    }

    /// The bytes available across all three pools.
    #[must_use]
    pub const fn total_available(&self) -> u64 {
        self.non_paged.available() + self.paged.available() + self.system_pte.available()
    }

    /// Verify all three pools.
    pub fn check(&self) -> Result<(), CommitError> {
        for kind in PoolKind::all() {
            self.get(kind).check()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_rng::Rng;
    use crate::vad::{VadRequest, VadTree};

    #[test]
    fn a_new_charge_is_empty_with_its_whole_limit_available() {
        let charge = CommitCharge::new("test", 4096);
        assert_eq!(charge.current(), 0);
        assert_eq!(charge.peak(), 0);
        assert_eq!(charge.limit(), 4096);
        assert_eq!(charge.available(), 4096);
        assert_eq!(charge.label(), "test");
        assert_eq!(charge.check(), Ok(()));
        assert!(charge.would_fit(4096));
        assert!(!charge.would_fit(4097));
        assert!(!charge.would_fit(0));
    }

    #[test]
    fn charging_accumulates_and_releasing_returns_to_zero() {
        let mut charge = CommitCharge::new("test", 4096);
        assert_eq!(charge.charge(1000), Ok(1000));
        assert_eq!(charge.charge(1000), Ok(2000));
        assert_eq!(charge.current(), 2000);
        assert_eq!(charge.available(), 2096);
        assert_eq!(charge.release(2000), Ok(0));
        assert_eq!(charge.current(), 0);
        assert_eq!(charge.charges(), 2);
        assert_eq!(charge.releases(), 1);
        assert_eq!(charge.peak(), 2000);
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn a_charge_beyond_the_limit_is_refused_and_changes_nothing() {
        let mut charge = CommitCharge::new("test", 4096);
        charge.charge(4000).expect("charged");
        assert_eq!(
            charge.charge(97),
            Err(CommitError::Exceeded {
                requested: 97,
                available: 96
            })
        );
        assert_eq!(
            charge.current(),
            4000,
            "a refused charge must not move the total"
        );
        assert_eq!(charge.failures(), 1);
        assert_eq!(charge.charge(96), Ok(4096));
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn the_peak_remembers_the_high_water_mark() {
        let mut charge = CommitCharge::new("test", 1 << 20);
        charge.charge(10).expect("charged");
        charge.charge(90).expect("charged");
        charge.release(50).expect("released");
        assert_eq!(charge.current(), 50);
        assert_eq!(charge.peak(), 100);
        charge.release(50).expect("released");
        assert_eq!(charge.current(), 0);
        assert_eq!(charge.peak(), 100, "the peak is not lowered by a release");
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn releasing_more_than_was_charged_is_reported() {
        let mut charge = CommitCharge::new("test", 4096);
        charge.charge(100).expect("charged");
        assert_eq!(
            charge.release(101),
            Err(CommitError::NotCharged {
                amount: 101,
                current: 100
            })
        );
        assert_eq!(charge.current(), 100);
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn a_zero_byte_charge_is_refused() {
        let mut charge = CommitCharge::new("test", 4096);
        assert_eq!(charge.charge(0), Err(CommitError::Zero));
        assert_eq!(charge.release(0), Err(CommitError::Zero));
        assert_eq!(charge.current(), 0);
    }

    #[test]
    fn a_limit_can_be_raised_and_lowered_but_not_below_the_charge() {
        let mut charge = CommitCharge::new("test", 100);
        charge.charge(80).expect("charged");
        assert_eq!(
            charge.set_limit(60),
            Err(CommitError::LimitBelowCurrent {
                limit: 60,
                current: 80
            })
        );
        assert_eq!(
            charge.set_limit(80),
            Ok(80),
            "lowering to exactly the charge is fine"
        );
        assert_eq!(
            charge.charge(1),
            Err(CommitError::Exceeded {
                requested: 1,
                available: 0
            })
        );
        assert_eq!(charge.set_limit(1 << 20), Ok(1 << 20));
        assert_eq!(charge.charge(1), Ok(81));
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn an_enormous_charge_cannot_overflow_the_counter() {
        let mut charge = CommitCharge::unlimited("system");
        assert_eq!(charge.charge(u64::MAX), Ok(u64::MAX));
        assert_eq!(
            charge.charge(1),
            Err(CommitError::Exceeded {
                requested: 1,
                available: 0
            })
        );
        assert_eq!(charge.release(u64::MAX), Ok(0));
        assert_eq!(charge.check(), Ok(()));
    }

    #[test]
    fn the_pools_are_accounted_separately() {
        let mut pools = Pools::new(PoolLimits {
            non_paged: 8192,
            paged: 1 << 20,
            system_pte: 4096,
        });
        pools.charge(PoolKind::NonPaged, 4096).expect("charged");
        pools.charge(PoolKind::Paged, 8192).expect("charged");
        assert_eq!(pools.get(PoolKind::NonPaged).current(), 4096);
        assert_eq!(pools.get(PoolKind::Paged).current(), 8192);
        assert_eq!(pools.get(PoolKind::SystemPte).current(), 0);
        assert_eq!(pools.total_charged(), 4096 + 8192);
        // The nonpaged pool is nearly full; the paged pool is untouched by that.
        assert!(pools.charge(PoolKind::NonPaged, 4097).is_err());
        assert_eq!(pools.charge(PoolKind::Paged, 1 << 19), Ok(8192 + (1 << 19)));
        pools.release(PoolKind::NonPaged, 4096).expect("released");
        assert_eq!(pools.get(PoolKind::NonPaged).current(), 0);
        assert_eq!(pools.check(), Ok(()));
    }

    #[test]
    fn pool_names_say_whether_the_memory_may_be_paged_out() {
        assert_eq!(PoolKind::all().len(), 3);
        assert_eq!(PoolKind::NonPaged.name(), "nonpaged pool");
        assert!(!PoolKind::NonPaged.is_pageable());
        assert!(PoolKind::Paged.is_pageable());
        assert!(!PoolKind::SystemPte.is_pageable());
    }

    #[test]
    fn errors_are_displayable_and_are_errors() {
        fn assert_error<E: core::error::Error>() {}
        assert_error::<CommitError>();
        // The variants that carry numbers must still compare and debug cleanly.
        let error = CommitError::Exceeded {
            requested: 5,
            available: 3,
        };
        assert_eq!(
            error,
            CommitError::Exceeded {
                requested: 5,
                available: 3
            }
        );
        assert!(format!("{error:?}").contains("Exceeded"));
    }

    #[test]
    fn random_charges_and_releases_agree_with_a_model() {
        let mut rng = Rng::new(0x0C0F_FEE1);
        let limit = 1 << 20;
        let mut charge = CommitCharge::new("test", limit);
        // Model: the multiset of amounts currently charged, and the totals seen.
        let mut outstanding: Vec<u64> = Vec::new();
        let mut charged_sum = 0u64;
        let mut peak = 0u64;
        let mut refusals = 0u64;

        for step in 0..5000 {
            if rng.chance(50) || outstanding.is_empty() {
                let amount = 1 + rng.below(64 * 1024);
                match charge.charge(amount) {
                    Ok(total) => {
                        outstanding.push(amount);
                        charged_sum += amount;
                        peak = peak.max(charged_sum);
                        assert_eq!(total, charged_sum, "the total drifted at step {step}");
                    }
                    Err(CommitError::Exceeded { .. }) => {
                        refusals += 1;
                        assert!(charged_sum + amount > limit);
                    }
                    Err(other) => panic!("unexpected error at step {step}: {other:?}"),
                }
            } else {
                let index = rng.below(outstanding.len() as u64) as usize;
                let amount = outstanding.swap_remove(index);
                charged_sum -= amount;
                assert_eq!(charge.release(amount), Ok(charged_sum));
            }
            assert_eq!(charge.current(), charged_sum, "step {step}");
            assert_eq!(charge.peak(), peak, "step {step}");
            assert_eq!(charge.failures(), refusals, "step {step}");
            assert!(charge.current() <= limit);
            assert_eq!(charge.check(), Ok(()), "step {step}");
        }
    }

    #[test]
    fn a_vad_tree_charged_against_a_quota_never_exceeds_it() {
        let mut rng = Rng::new(0x5AD0_5AD0);
        let limit = 1 << 20;
        let mut quota = CommitCharge::new("process", limit);
        let mut tree: VadTree<256> = VadTree::new(0x1_0000, 0x4000_0000);
        let mut mapped: Vec<(u64, u64)> = Vec::new();
        let mut refusals = 0u64;

        for step in 0..2000 {
            if rng.chance(55) || mapped.is_empty() {
                let size = (1 + rng.below(64)) * 0x1000;
                let address = 0x1_0000 + rng.below(1024) * 0x1000;
                let request = VadRequest::private_anonymous(address, address + size);
                // Charge first: if the quota refuses, nothing is inserted at all.
                if quota.charge(size).is_err() {
                    refusals += 1;
                    continue;
                }
                match tree.insert(request) {
                    Ok(()) => mapped.push((address, address + size)),
                    Err(_) => {
                        quota
                            .release(size)
                            .expect("released after a refused insert");
                    }
                }
            } else {
                let index = rng.below(mapped.len() as u64) as usize;
                let (start, end) = mapped.swap_remove(index);
                tree.remove(start, end).expect("removed");
                quota.release(end - start).expect("released");
            }
            assert_eq!(
                tree.commit_total(),
                quota.current(),
                "step {step}: the tree and the quota disagree"
            );
            assert!(quota.current() <= limit, "step {step}: over the limit");
            assert_eq!(tree.check(), Ok(()), "step {step}");
            assert_eq!(quota.check(), Ok(()), "step {step}");
        }
        assert!(refusals > 0, "the limit should have been hit at least once");
    }
}
