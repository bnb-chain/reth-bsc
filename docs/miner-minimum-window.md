# Minimum transaction window for the late-finish experiment

With `BSC_MINING_ALLOW_LATE_FINISH=1`, in-turn jobs now receive at least 50 ms
from the existing job start to select transactions. For the **first block in each
turn**, the first attempt to enter the user-transaction selection/execution loop
also receives at least 50 ms from that entry. Permit waits, provider/EVM setup and
prewarm setup do not consume this first-in-turn floor. The floor applies after
slot, margin, budget-cap and reserve deductions. Jobs with a larger window keep
their existing deadlines. A recovered window also retains the configured root
reserve before the soft completion deadline; late-finish may overrun that deadline.

The existing opt-in and in-turn restriction are reused. Disabled and out-of-turn
jobs retain the old budget. Non-first in-turn jobs retain their existing job-start
floor. All attempt clones share a one-time selection deadline; retries cannot
renew it, including clones created before the first attempt finishes preparing.
The root reserve moves with an extended transaction deadline. Parent changes and shutdown still cancel the job, including
while waiting for its root. The existing late-finish candidate collection already
waits for normal completion and does not require another collection policy.

The initial job-window metric still includes preparation and permit waits. The
actual `execution_window_seconds` metric uses the effective selection-entry
deadline. Prewarming does not expire the pending first-in-turn floor during setup;
after selection starts it observes the same deadline as the transaction loop.
Outer timers recheck this shared deadline rather than returning on a stale timer.
If the initial timer expires before preparation ends, the existing late-finish
collector waits for the normal attempt and still honors aborts.

This is a wall-clock opportunity to select and execute transactions, not a promise
of 50 ms of CPU service, available executable transactions, or a nonempty canonical block. This
experiment can execute and complete past the original slot, so validation must
compare canonical empty rate, throughput, out-of-turn blocks and reorgs, rather
than only successful local candidates. No header, gas accounting, transaction
validation, Reth dependency or provider/cache behavior changes.

Startup diagnostics already label an increased window as `recovery_floor`.
For recovered jobs the soft completion deadline also moves, so late-finish
overrun counters cannot be compared to the old threshold as if it were unchanged.

The original unapplied draft in the separate performance worktree was not used
wholesale: it applied the floor to all jobs and extended the general collection
policy. This version restricts recovery to the existing in-turn QA experiment.

The locally available go-bsc worker snapshot stops starting transaction filling
when its computed delay is nonpositive (`miner/worker.go`, `commitWork`). This
floor is an intentional Reth QA policy, not a claim of matching geth timing.

Local tests cover expired and short slots, preparation longer than the initial
window, the 50 ms boundary, unchanged healthy budgets, first-block/opt-in gating,
deadline sharing with prewarmers and retries, cancellation, effective-window
metrics and the existing late-finish
candidate-selection tests. No deployment or new load test is part of this change.
