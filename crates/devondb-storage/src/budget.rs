//! The one memory accountant every subsystem charges (docs/MVCC.md §7.2).
//!
//! Page cache frames, the committed MVCC overlay, transaction write sets,
//! scan working sets, and Sort/Aggregate buffers all charge the same
//! budget. Callers that fail a charge run their reclaim ladder (evict,
//! checkpoint, spill — docs/MVCC.md §7.2) and retry once before surfacing
//! [`DevonError::BudgetExceeded`](devondb_types::DevonError).

use std::{
    fmt,
    sync::{
        Arc, PoisonError, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
};

type Reclaimer = Arc<dyn Fn(usize) -> bool + Send + Sync>;

/// A shared byte budget with lock-free charge and release.
pub struct MemoryBudget {
    limit: usize,
    charged: AtomicUsize,
    reclaimer: RwLock<Option<Reclaimer>>,
}

impl fmt::Debug for MemoryBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MemoryBudget")
            .field("limit", &self.limit)
            .field("charged", &self.charged())
            .field(
                "reclaimer_installed",
                &self
                    .reclaimer
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .is_some(),
            )
            .finish()
    }
}

impl MemoryBudget {
    /// Creates a budget capped at `limit` bytes.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            charged: AtomicUsize::new(0),
            reclaimer: RwLock::new(None),
        }
    }

    /// Creates an effectively unlimited budget (tests, pre-7.5 interim).
    #[must_use]
    pub fn unlimited() -> Self {
        Self::new(usize::MAX)
    }

    /// Attempts to charge `bytes`; returns `false` when it would exceed
    /// the limit, leaving the charged total unchanged.
    pub fn try_charge(&self, bytes: usize) -> bool {
        let mut current = self.charged.load(Ordering::Relaxed);
        loop {
            let Some(next) = current.checked_add(bytes) else {
                return false;
            };
            if next > self.limit {
                return false;
            }
            match self.charged.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    /// Installs the cache reclaimer used by [`Self::charge_or_reclaim`].
    ///
    /// Installing a new hook replaces the previous one. This lets a
    /// long-lived budget follow the current pager when a read-only follower
    /// refreshes its database context.
    pub fn set_reclaimer(&self, reclaimer: Arc<dyn Fn(usize) -> bool + Send + Sync>) {
        *self
            .reclaimer
            .write()
            .unwrap_or_else(PoisonError::into_inner) = Some(reclaimer);
    }

    /// Attempts a charge, asks the installed reclaimer for headroom on
    /// failure, then retries the charge exactly once.
    ///
    /// With no installed reclaimer this has the same behavior as
    /// [`Self::try_charge`].
    pub fn charge_or_reclaim(&self, bytes: usize) -> bool {
        if self.try_charge(bytes) {
            return true;
        }
        let reclaimer = self
            .reclaimer
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let Some(reclaimer) = reclaimer else {
            return false;
        };
        reclaimer(bytes);
        self.try_charge(bytes)
    }

    /// Releases `bytes` previously charged.
    ///
    /// Releasing more than was charged is a bookkeeping bug: debug builds
    /// assert on it so tests catch charge/release mis-pairing at the
    /// offending call site; release builds saturate at zero rather than
    /// wrapping, keeping enforcement conservative.
    pub fn release(&self, bytes: usize) {
        let mut current = self.charged.load(Ordering::Relaxed);
        loop {
            debug_assert!(
                bytes <= current,
                "released {bytes} bytes with only {current} charged — a charge/release mis-pairing"
            );
            let next = current.saturating_sub(bytes);
            match self.charged.compare_exchange_weak(
                current,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }

    /// Returns the bytes currently charged.
    #[must_use]
    pub fn charged(&self) -> usize {
        self.charged.load(Ordering::Relaxed)
    }

    /// Returns the configured limit in bytes.
    #[must_use]
    pub const fn limit(&self) -> usize {
        self.limit
    }
}

/// An RAII charge against a [`MemoryBudget`], released on drop.
///
/// Bounded working sets (HNSW search arenas, verified-CSR sets, adjacency
/// scratch — `docs/HNSW.md` §6) hold one of these per reservation so a
/// failure path can never leak charged bytes.
#[derive(Debug)]
pub struct ChargedBytes<'a> {
    budget: &'a MemoryBudget,
    bytes: usize,
}

impl<'a> ChargedBytes<'a> {
    /// Charges `bytes` against `budget`, or fails with the caller's
    /// context. The charge is released when the value drops.
    pub fn try_new(
        budget: &'a MemoryBudget,
        bytes: usize,
        context: impl FnOnce() -> String,
    ) -> devondb_types::DevonResult<Self> {
        if !budget.try_charge(bytes) {
            return Err(devondb_types::DevonError::BudgetExceeded { context: context() });
        }
        Ok(Self { budget, bytes })
    }

    /// Grows this reservation by `additional` bytes, or fails with the
    /// caller's context, leaving the existing charge intact.
    pub fn grow(
        &mut self,
        additional: usize,
        context: impl FnOnce() -> String,
    ) -> devondb_types::DevonResult<()> {
        if !self.budget.try_charge(additional) {
            return Err(devondb_types::DevonError::BudgetExceeded { context: context() });
        }
        self.bytes += additional;
        Ok(())
    }

    /// Shrinks this reservation by `bytes`, saturating at zero and releasing
    /// the relinquished bytes to the budget immediately.
    pub fn shrink(&mut self, bytes: usize) {
        let released = self.bytes.min(bytes);
        self.bytes -= released;
        self.budget.release(released);
    }

    /// Returns the bytes currently held by this reservation.
    #[must_use]
    pub fn bytes(&self) -> usize {
        self.bytes
    }
}

impl Drop for ChargedBytes<'_> {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::MemoryBudget;

    #[test]
    fn charged_bytes_releases_on_drop_and_grows() {
        let budget = MemoryBudget::new(100);
        {
            let mut charge =
                super::ChargedBytes::try_new(&budget, 60, || "arena".to_owned()).unwrap();
            charge.grow(30, || "arena grow".to_owned()).unwrap();
            assert_eq!(charge.bytes(), 90);
            assert!(charge.grow(20, || "over".to_owned()).is_err());
            assert_eq!(budget.charged(), 90);
        }
        assert_eq!(budget.charged(), 0);
        assert!(super::ChargedBytes::try_new(&budget, 101, || "too big".to_owned()).is_err());
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn charged_bytes_shrink_releases_immediately_and_saturates() {
        let budget = MemoryBudget::new(100);
        let mut charge = super::ChargedBytes::try_new(&budget, 80, || "arena".to_owned()).unwrap();

        charge.shrink(30);
        assert_eq!(charge.bytes(), 50);
        assert_eq!(budget.charged(), 50);
        assert!(budget.try_charge(50));

        charge.shrink(1_000);
        assert_eq!(charge.bytes(), 0);
        assert_eq!(budget.charged(), 50);
        drop(charge);
        assert_eq!(budget.charged(), 50);
    }

    #[test]
    fn charges_up_to_the_limit_and_refuses_past_it() {
        let budget = MemoryBudget::new(100);

        assert!(budget.try_charge(60));
        assert!(budget.try_charge(40));
        assert_eq!(budget.charged(), 100);
        assert!(!budget.try_charge(1));
        assert_eq!(budget.charged(), 100);
    }

    #[test]
    fn release_restores_headroom() {
        let budget = MemoryBudget::new(100);

        assert!(budget.try_charge(80));
        budget.release(30);
        assert_eq!(budget.charged(), 50);
        assert!(budget.try_charge(50));
        budget.release(100);
        assert_eq!(budget.charged(), 0);
    }

    #[test]
    fn unlimited_budget_accepts_large_charges() {
        let budget = MemoryBudget::unlimited();

        assert!(budget.try_charge(usize::MAX / 2));
        assert_eq!(budget.limit(), usize::MAX);
    }

    #[test]
    fn overflowing_charge_is_refused() {
        let budget = MemoryBudget::unlimited();

        assert!(budget.try_charge(usize::MAX - 8));
        assert!(!budget.try_charge(usize::MAX));
    }

    #[test]
    fn concurrent_charges_never_exceed_the_limit() {
        use std::sync::Arc;
        use std::thread;

        let budget = Arc::new(MemoryBudget::new(10_000));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let budget = Arc::clone(&budget);
                thread::spawn(move || (0..10_000).filter(|_| budget.try_charge(1)).count())
            })
            .collect();
        let granted: usize = workers.into_iter().map(|w| w.join().unwrap()).sum();

        assert_eq!(granted, 10_000);
        assert_eq!(budget.charged(), 10_000);
    }

    #[test]
    fn charge_or_reclaim_retries_after_reclaimer_frees_bytes() {
        use std::sync::Arc;

        let budget = Arc::new(MemoryBudget::new(10));
        assert!(budget.try_charge(10));
        let weak_budget = Arc::downgrade(&budget);
        budget.set_reclaimer(Arc::new(move |bytes| {
            let Some(budget) = weak_budget.upgrade() else {
                return false;
            };
            budget.release(bytes);
            true
        }));

        assert!(budget.charge_or_reclaim(4));
        assert_eq!(budget.charged(), 10);
    }

    #[test]
    fn charge_or_reclaim_without_reclaimer_is_a_plain_charge() {
        let budget = MemoryBudget::new(10);
        assert!(budget.try_charge(10));

        assert!(!budget.charge_or_reclaim(1));
        assert_eq!(budget.charged(), 10);
    }

    #[test]
    fn charge_or_reclaim_retries_only_once_when_nothing_is_freed() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };

        let budget = MemoryBudget::new(10);
        assert!(budget.try_charge(10));
        let calls = Arc::new(AtomicUsize::new(0));
        let reclaimer_calls = Arc::clone(&calls);
        budget.set_reclaimer(Arc::new(move |_| {
            reclaimer_calls.fetch_add(1, Ordering::Relaxed);
            false
        }));

        assert!(!budget.charge_or_reclaim(1));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(budget.charged(), 10);
    }
}
