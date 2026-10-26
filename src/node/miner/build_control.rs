//! One monotonic budget shared by all attempts for a local payload job.

use reth_tasks::ManualCancel;
use std::time::{Duration, Instant};

// Bound overlap while an obsolete synchronous transaction is returning to a cancellation check.
pub(crate) static LOCAL_BUILD_PERMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Separates stopping transaction selection from discarding an invalid or expired candidate.
#[derive(Debug, Clone)]
pub struct BuildControl {
    pub(crate) transactions_until: Instant,
    pub(crate) finish_until: Instant,
    cancel: ManualCancel,
}

impl BuildControl {
    /// Starts a shared attempt budget, capped by the remaining slot time, with a
    /// separate reserve for system transactions and state-root completion.
    pub fn new(
        started: Instant,
        total_budget: Duration,
        reserve: Duration,
        slot_remaining: Duration,
        cancel: ManualCancel,
    ) -> Self {
        let budget = total_budget.min(slot_remaining);
        Self {
            transactions_until: started + budget.saturating_sub(reserve),
            finish_until: started + budget,
            cancel,
        }
    }

    pub(crate) fn stop_transactions(&self, now: Instant) -> bool {
        self.cancel.is_cancelled() || now >= self.transactions_until
    }

    pub(crate) fn check_active(&self, now: Instant) -> Result<(), &'static str> {
        if self.cancel.is_cancelled() {
            Err("payload build canceled")
        } else if now >= self.finish_until {
            Err("payload build completion deadline exceeded")
        } else {
            Ok(())
        }
    }

    pub(crate) fn root_wait(&self, now: Instant) -> Result<Duration, &'static str> {
        self.check_active(now)?;
        // Poll cancellation while waiting on the synchronous root-result channel.
        Ok(self.finish_until.saturating_duration_since(now).min(Duration::from_millis(10)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_and_retries_do_not_extend_the_budget() {
        let start = Instant::now();
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            Duration::from_millis(120),
            Duration::from_secs(3),
            ManualCancel::default(),
        );
        assert!(!control.stop_transactions(start + Duration::from_millis(329)));
        assert!(control.stop_transactions(start + Duration::from_millis(330)));
        assert!(control.check_active(start + Duration::from_millis(330)).is_ok());
        let retry = control.clone();
        assert!(retry.check_active(start + Duration::from_secs(3)).is_err());
        assert_eq!(retry.finish_until, start + Duration::from_millis(450));
    }

    #[test]
    fn slot_deadline_and_reserve_bound_transaction_selection() {
        let start = Instant::now();
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            Duration::from_millis(120),
            Duration::from_millis(100),
            ManualCancel::default(),
        );
        assert!(control.stop_transactions(start));
        assert!(control.check_active(start).is_ok());
        assert!(control.check_active(start + Duration::from_millis(100)).is_err());
    }

    #[test]
    fn cancel_discards_instead_of_finalizing_and_slow_tx_cannot_extend_budget() {
        let start = Instant::now();
        let cancel = ManualCancel::default();
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            Duration::from_millis(120),
            Duration::from_secs(3),
            cancel.clone(),
        );
        assert_eq!(control.root_wait(start).unwrap(), Duration::from_millis(10));
        assert!(control.check_active(start + Duration::from_millis(451)).is_err());
        cancel.cancel();
        assert!(control.stop_transactions(start));
        assert!(control.root_wait(start).is_err());
    }
}
