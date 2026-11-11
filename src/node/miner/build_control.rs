//! One monotonic budget shared by all attempts for a local payload job.

use reth_tasks::ManualCancel;
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

// Bound overlap while an obsolete synchronous transaction is returning to a cancellation check.
pub(crate) static LOCAL_BUILD_PERMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

const MIN_LATE_TRANSACTION_WINDOW: Duration = Duration::from_millis(50);

/// Separates stopping transaction selection from discarding an invalid or expired candidate.
#[derive(Debug, Clone)]
pub struct BuildControl {
    transactions_until: Instant,
    finish_until: Instant,
    cancel: ManualCancel,
    allow_late_finish: bool,
    /// The first selection entry fixes the shared deadline for every attempt in this job.
    selection_deadline: Option<Arc<OnceLock<Instant>>>,
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
            allow_late_finish: false,
            selection_deadline: None,
        }
    }

    /// QA experiment: allow late completion and give a late parent at least 50 ms
    /// from job creation. First-in-turn jobs can defer their floor to selection entry.
    /// Preserve the completion reserve and cancellation.
    /// Call once at job creation, only for opted-in in-turn jobs; retries clone it.
    pub(crate) fn with_late_finish(
        mut self,
        enabled: bool,
        started: Instant,
        reserve: Duration,
    ) -> Self {
        self.allow_late_finish = enabled;
        if enabled {
            let minimum = started + MIN_LATE_TRANSACTION_WINDOW;
            if self.transactions_until < minimum {
                self.transactions_until = minimum;
                self.finish_until = minimum + reserve;
            }
        }
        self
    }

    pub(crate) fn allows_late_finish(&self) -> bool {
        self.allow_late_finish
    }

    pub(crate) fn stop_transactions(&self, now: Instant) -> bool {
        // Prewarming starts during preparation, before the first selection entry arms its floor.
        if self.cancel.is_cancelled() {
            return true;
        }
        match &self.selection_deadline {
            Some(deadline) => deadline.get().is_some_and(|deadline| now >= *deadline),
            None => now >= self.transactions_until,
        }
    }

    pub(crate) fn check_active(&self, now: Instant) -> Result<(), &'static str> {
        if self.cancel.is_cancelled() {
            Err("payload build canceled")
        } else if !self.allow_late_finish && now >= self.finish_until() {
            Err("payload build completion deadline exceeded")
        } else {
            Ok(())
        }
    }

    pub(crate) fn root_wait(&self, now: Instant) -> Result<Duration, &'static str> {
        self.check_active(now)?;
        // Poll cancellation while waiting on the synchronous root-result channel.
        let poll = Duration::from_millis(10);
        Ok(if self.allow_late_finish {
            poll
        } else {
            self.finish_until().saturating_duration_since(now).min(poll)
        })
    }

    /// Arm one preparation-free floor for the first block of an opted-in in-turn job.
    /// Call only at job creation; retries must clone this control instead.
    pub(crate) fn with_first_transaction_window(mut self, first_in_turn: bool) -> Self {
        if self.allow_late_finish && first_in_turn {
            self.selection_deadline = Some(Arc::new(OnceLock::new()));
        }
        self
    }

    /// Start the one shared selection window after permit acquisition and preparation.
    /// Later attempts observe the first deadline instead of renewing it.
    pub(crate) fn start_transaction_selection(&self, now: Instant) -> Result<(), &'static str> {
        if let Some(deadline) = &self.selection_deadline {
            self.check_active(now)?;
            deadline.get_or_init(|| self.transactions_until.max(now + MIN_LATE_TRANSACTION_WINDOW));
        }
        Ok(())
    }

    pub(crate) fn transactions_until(&self) -> Instant {
        self.selection_deadline
            .as_ref()
            .and_then(|deadline| deadline.get().copied())
            .unwrap_or(self.transactions_until)
    }

    pub(crate) fn finish_until(&self) -> Instant {
        self.transactions_until() + self.finish_until.duration_since(self.transactions_until)
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

    #[test]
    fn late_finish_keeps_transaction_deadline_and_cancellation() {
        let start = Instant::now();
        let cancel = ManualCancel::default();
        let control = BuildControl::new(
            start,
            Duration::from_millis(270),
            Duration::from_millis(120),
            Duration::from_millis(420),
            cancel.clone(),
        )
        .with_late_finish(true, start, Duration::from_millis(120));
        assert!(!control.stop_transactions(start + Duration::from_millis(149)));
        assert!(control.stop_transactions(start + Duration::from_millis(150)));
        // Both the internal finish deadline and the original slot can pass.
        assert!(control.check_active(start + Duration::from_secs(1)).is_ok());
        // Avoid a zero-timeout busy loop after the old deadline.
        assert_eq!(
            control.root_wait(start + Duration::from_secs(1)).unwrap(),
            Duration::from_millis(10)
        );
        cancel.cancel();
        assert!(control.check_active(start).is_err());
        assert!(control.root_wait(start).is_err());
    }

    #[test]
    fn late_parent_gets_minimum_window_without_renewing_retries() {
        let start = Instant::now();
        // Fully expired, sub-reserve, seven-ms, boundary and healthy slots.
        for (slot_ms, reserve_ms, window_ms) in [
            (0, 0, 50),
            (0, 30, 50),
            (20, 30, 50),
            (37, 30, 50),
            (79, 30, 50),
            (80, 30, 50),
            (81, 30, 51),
            (0, 120, 50),
            (127, 120, 50),
        ] {
            let reserve = Duration::from_millis(reserve_ms);
            let control = BuildControl::new(
                start,
                Duration::from_millis(450),
                reserve,
                Duration::from_millis(slot_ms),
                ManualCancel::default(),
            )
            .with_late_finish(true, start, reserve);
            let window = Duration::from_millis(window_ms);
            assert_eq!(control.transactions_until, start + window);
            // The reserve that remains after transaction selection is not shortened.
            assert_eq!(control.finish_until, control.transactions_until + reserve);
            assert!(!control.stop_transactions(start + window - Duration::from_millis(1)));
            assert!(control.stop_transactions(start + window));
            let retry = control.clone();
            assert_eq!(retry.transactions_until, control.transactions_until);
            assert_eq!(retry.finish_until, control.finish_until);
            assert!(retry.stop_transactions(start + Duration::from_secs(1)));
        }
    }

    #[test]
    fn minimum_window_requires_opt_in_and_never_overrides_cancellation() {
        let start = Instant::now();
        let cancel = ManualCancel::default();
        let original = BuildControl::new(
            start,
            Duration::from_millis(450),
            Duration::from_millis(30),
            Duration::ZERO,
            cancel.clone(),
        );
        let disabled = original.clone().with_late_finish(false, start, Duration::from_millis(30));
        assert_eq!(disabled.transactions_until, original.transactions_until);
        assert_eq!(disabled.finish_until, original.finish_until);
        assert!(disabled.stop_transactions(start));
        assert!(disabled.check_active(start).is_err());

        cancel.cancel();
        let enabled = original.with_late_finish(true, start, Duration::from_millis(30));
        assert!(enabled.stop_transactions(start));
        assert!(enabled.check_active(start).is_err());
        assert!(enabled.root_wait(start).is_err());
    }

    #[test]
    fn first_selection_recovers_preparation_time_once_across_preexisting_clones() {
        let start = Instant::now();
        let reserve = Duration::from_millis(30);
        for slot in [Duration::ZERO, Duration::from_millis(82)] {
            let job = BuildControl::new(
                start,
                Duration::from_millis(450),
                reserve,
                slot,
                ManualCancel::default(),
            )
            .with_late_finish(true, start, reserve)
            .with_first_transaction_window(true);
            let attempt = job.clone();
            let prewarm = job.clone();
            let retry = job.clone();
            let entry = start + Duration::from_millis(200);
            // Preparation and its prewarmer must not consume the pending floor.
            assert!(!prewarm.stop_transactions(entry));
            attempt.start_transaction_selection(entry).unwrap();
            let deadline = entry + MIN_LATE_TRANSACTION_WINDOW;
            assert_eq!(job.transactions_until(), deadline);
            assert_eq!(prewarm.transactions_until(), deadline);
            assert_eq!(job.finish_until(), deadline + reserve);
            assert!(!attempt.stop_transactions(deadline - Duration::from_millis(1)));
            assert!(attempt.stop_transactions(deadline));
            assert!(prewarm.stop_transactions(deadline));

            retry.start_transaction_selection(deadline + Duration::from_millis(80)).unwrap();
            assert_eq!(retry.transactions_until(), deadline);
            assert_eq!(retry.finish_until(), deadline + reserve);
            assert!(retry.stop_transactions(deadline + Duration::from_millis(80)));
        }
    }

    #[test]
    fn first_selection_keeps_a_healthy_deadline() {
        let start = Instant::now();
        let reserve = Duration::from_millis(30);
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            reserve,
            Duration::from_millis(450),
            ManualCancel::default(),
        )
        .with_late_finish(true, start, reserve)
        .with_first_transaction_window(true);
        control.start_transaction_selection(start + Duration::from_millis(100)).unwrap();
        assert_eq!(control.transactions_until(), start + Duration::from_millis(420));
        assert_eq!(control.finish_until(), start + Duration::from_millis(450));
    }

    #[test]
    fn selection_entry_floor_requires_first_in_turn_and_late_finish() {
        let start = Instant::now();
        let reserve = Duration::from_millis(30);
        for (late_finish, first_in_turn) in [(false, true), (true, false), (false, false)] {
            let control = BuildControl::new(
                start,
                Duration::from_millis(450),
                reserve,
                Duration::ZERO,
                ManualCancel::default(),
            )
            .with_late_finish(late_finish, start, reserve)
            .with_first_transaction_window(first_in_turn);
            let original = control.transactions_until();
            let entry = start + Duration::from_millis(200);
            assert!(control.stop_transactions(entry));
            control.start_transaction_selection(entry).unwrap();
            assert_eq!(control.transactions_until(), original);
        }
    }

    #[test]
    fn selection_entry_floor_never_revives_a_canceled_job() {
        let start = Instant::now();
        let cancel = ManualCancel::default();
        let reserve = Duration::from_millis(30);
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            reserve,
            Duration::ZERO,
            cancel.clone(),
        )
        .with_late_finish(true, start, reserve)
        .with_first_transaction_window(true);
        let original = control.transactions_until();
        cancel.cancel();
        assert!(control.stop_transactions(start));
        assert_eq!(
            control.start_transaction_selection(start + Duration::from_secs(1)),
            Err("payload build canceled")
        );
        assert_eq!(control.transactions_until(), original);
        assert_eq!(control.root_wait(start), Err("payload build canceled"));
    }

    #[test]
    fn cancellation_still_interrupts_the_recovered_selection_window() {
        let start = Instant::now();
        let cancel = ManualCancel::default();
        let reserve = Duration::from_millis(30);
        let control = BuildControl::new(
            start,
            Duration::from_millis(450),
            reserve,
            Duration::ZERO,
            cancel.clone(),
        )
        .with_late_finish(true, start, reserve)
        .with_first_transaction_window(true);
        let entry = start + Duration::from_millis(200);
        control.start_transaction_selection(entry).unwrap();
        assert!(!control.stop_transactions(entry));
        cancel.cancel();
        assert!(control.stop_transactions(entry));
        assert_eq!(control.check_active(entry), Err("payload build canceled"));
        assert_eq!(control.root_wait(entry), Err("payload build canceled"));
    }
}
