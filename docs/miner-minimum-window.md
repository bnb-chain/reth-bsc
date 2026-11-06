# Minimum transaction window for the late-finish experiment

With `BSC_MINING_ALLOW_LATE_FINISH=1`, in-turn jobs now receive at least 50 ms
from the existing job start to select transactions. The floor applies after
slot, margin, budget-cap and reserve deductions. Jobs with a larger window keep
their existing deadlines. A recovered window also retains the configured root
reserve before the soft completion deadline; late-finish may overrun that deadline.

The existing opt-in and in-turn restriction are reused. Disabled and out-of-turn
jobs retain the old budget. Retries clone the job deadlines rather than applying
the floor again. Parent changes and shutdown still cancel the job, including
while waiting for its root. The existing late-finish candidate collection already
waits for normal completion and does not require another collection policy.

The window includes preparation and waiting for the local build permit. It is
not a guarantee of 50 ms of EVM execution or a nonempty canonical block. This
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

Local tests cover expired and short slots, the 50 ms boundary, unchanged healthy
budgets, opt-out, retry deadline reuse, cancellation and the existing late-finish
candidate-selection tests. No deployment or new load test is part of this change.
