# Performance task diagnostics

This instrumentation is based on BSC PR #478 at `9387de707bfd337d8542a4072ccec9b49fb9f04b`
and its Reth dependency `84e2676cab0800bac971b4251445a2213815bfb1`.
It does not change build budgets, speculative scheduling, cancellation decisions, cache reuse,
consensus rules, or block selection. The paired Reth patch is required.
The manifest pins that patch to `3361695910c91adc500ef7bddf89b82dea84bbbc`
([Reth PR #242](https://github.com/bnb-chain/reth/pull/242)).

## What is established, and what remains a hypothesis

The import path waits for the sparse root after EVM execution. A configured 2-second timeout
submits a serial fallback and races it against the original root task. The timeout itself does
not close the consumer's cancellation channel. Therefore "timeout frees resources, allowing the
root to complete" is a hypothesis requiring the actual fallback/consumer/worker timeline.

`storage-workers` and `account-workers` are named, single-thread worker queues. Each queued
closure broadcasts a session of proof workers and waits until they exit. Those workers hold
providers and run until their input channels close. A later session can queue behind an older
session. This is a concrete dependency to observe, not proof that it caused the incident.

## Timeout snapshots

Normal operation adds counters/histograms and relaxed atomic progress updates, not new per-block
or per-transaction log lines. Existing debug logs receive `root_task_id` attribution.
At most one newly timed-out task per process per 30 seconds is selected for WARN snapshots.
The selected task also logs its remaining lifecycle transitions (a bounded number of events,
not per worker or proof). A second process-wide cap of 32 lines per 30-second window
bounds bursts when old sampled tasks resume together; `suppressed_log_events_total` counts
filtered follow-up events. Other timeouts still increment counters.

```sh
grep -RIn 'Sampled state-root timeout progress' /server/reth2.0/data_dir/.local/node/logs
```

Each record includes `root_task_id`, `origin`, `block_number`, `parent_hash`, `age_us`, named
progress counters and named `stages_us`. Timestamp slots are microseconds since task creation
plus one; zero means not observed. IDs are log fields, never Prometheus labels.

| Observation at timeout | Next path to inspect |
| --- | --- |
| `root_started=0` | Sparse-trie named worker is not yet running this closure. Check its queue. |
| `account_session_started=0` or `storage_session_started=0` | Corresponding proof session has not started; another session may hold the named worker. |
| Session started, worker-start count below expected | Broadcast workers are waiting for pool capacity. |
| Worker-start count greater than worker-ready count | Provider/cursor initialization is incomplete. Inspect overlay/provider dependencies or thread stacks. |
| Jobs started greater than jobs exited | Workers are executing or waiting inside proof computation; stacks are needed to distinguish lock, storage subtask, or IO waits. |
| `finish_sent_us>0`, `hashing_started=0` | Authoritative end marker was sent, but hashing closure is still queued. |
| `finish_hashed_us>0`, `finish_received_us=0` | Hashing observed the marker, but root has not consumed it. |
| End marker received, proof batches sent exceed received | Root is waiting for proof results. |
| `root_result="success"` and `root_computed` precedes `consumer_dropped` | Root completion did not require that consumer's cancellation-channel closure. |
| Fallback won, consumer dropped, then workers/root exit | Consistent with cancellation/release unblocking work; still verify the blocked dependency with a sampled stack. |

`root_result` distinguishes successful root computation from `cancelled` and `failed`; a
cancelled task exiting after timeout must not be interpreted as successfully finishing its root.

Snapshots are best-effort observations across threads. Concurrent events can appear in different
log order; sender/receiver counters can briefly disagree. `consumer_dropped` is recorded just after
closing the cancellation channel. `root_exited` marks the outer closure body exiting; it is not a
claim that every deferred memory drop has completed. Worker exit counts include failures.

`reth_trie_diagnostics_stage_age_seconds{origin,phase}` measures age from root creation to the
named milestone. `root_computed` is background lifetime including queueing and overlapped EVM
work; it is not the post-execution wait or CPU time. Existing timeout/fallback counters remain.

## Queue visibility

Existing named-worker queue-wait histograms record only jobs that have started. New
`reth_executor_named_worker_queued_tasks{worker}` and `..._active_tasks{worker}` expose tasks
that are still queued/running, including sessions stuck before a queue-wait sample is emitted.
Failed enqueue, task panic, and normal exit balance these gauges.

Watch `worker=~"sparse-trie|trie-hashing|storage-workers|account-workers|serial-root"` alongside existing
`reth_executor_named_worker_queue_wait_duration_seconds` and worker-pool queue/duration metrics.
The progress counters cover proof worker initialization and internal job starts/exits; they do
not measure each internal channel's enqueue-to-dequeue latency separately.

## Build attempt costs

All new attempt metrics use `reth_bsc_build_diagnostics_` and fixed labels:

- `kind`: `speculative` (initial out-of-turn backoff build), `regular`, `rebuild`, `empty`.
- `turn`: the job's `inturn` or `outofturn`, separate from work kind.
- `outcome` on `finished_total`: `completed`, `completed_after_cancel`, `error`,
  `error_after_cancel`, `dropped_queued`, `dropped_running`.

`submitted_total`, `queued`, `active`, `queue_seconds`, `run_seconds` cover attempt lifecycle.
`execution_seconds`, `root_seconds`, `transactions`, `gas` describe successful build results.
Their costs are counted even when the result is never selected. Failed builds have no resulting
block, so their transaction/gas histograms are absent, not zero; run-time accounting still applies.
`run_seconds` and execution timings are wall-clock duration, not thread CPU time.

`selected_total` counts a successful local choice/finalization once despite payload clones.
`unselected_execution_seconds`, `unselected_gas`, and `unselected_transactions` record the
completed work of candidates that were never locally selected.
`unselected_completed_total` is emitted when the last reference to an unselected completed
candidate is dropped. This is unselected work, not necessarily wholly wasted work: it may warm
shared state. Selected candidates can still fail to reach the canonical chain. No unbounded
hash-to-attempt map is retained. Existing finalization logs include `attempt_id` and `attempt_kind`;
external bid candidates have neither field.

Example mean speculative execution duration, per node:

```promql
sum by (instance,kind) (rate(reth_bsc_build_diagnostics_execution_seconds_sum{kind="speculative"}[5m]))
/
sum by (instance,kind) (rate(reth_bsc_build_diagnostics_execution_seconds_count{kind="speculative"}[5m]))
```

Use the same `_sum / _count` pattern for `run_seconds`, `transactions`, and `gas`.
Compare all kinds without the speculative filter; retain `instance` and `turn` when appropriate.
Completed unselected counts are a useful lower bound on redundant work, not a full CPU-waste measure.

## Cache attribution

Existing cache metrics are preserved. Additive metrics:

- `reth_trie_diagnostics_cache_initializations_total{origin,result="fresh|reused"}`.
- `reth_trie_diagnostics_cache_wait_seconds{origin}` for waiting on a preserved trie.
- `reth_trie_diagnostics_cache_lookups_total{origin,cache="account|storage",result="hit|miss"}`
  records successful root tasks' cache lookups. Failed tasks are not included in this ratio.
- `reth_trie_diagnostics_overlay_cache_total{origin,result="fill|reuse"}`.
- `reth_trie_diagnostics_overlay_blocks{origin}` records the block path examined on a cache miss;
  it is not necessarily the number of blocks finally merged after parent-cache reuse.
- `reth_trie_diagnostics_overlay_compute_wall_seconds{origin}` includes compute queue/wait.
- `reth_trie_diagnostics_changeset_cache_total{origin,result="hits|misses"}`.

Root origins distinguish `miner`, `import`, `builder`. Overlay/changeset origins distinguish
`miner` from shared `engine` ownership; engine caches can serve both import and built-in payload
building, so their shared metrics must not be labeled as exclusively import work.
Compare cache miss rates and total work with the same workload, head progress and memory-chain
depth. A fresh trie count alone does not quantify performance regression.

## Out-of-turn blocks, at every gas limit

`reth_bsc_chain_diagnostics_blocks_total{stage,turn}` uses **validated block headers**:
`difficulty=2` is in-turn, `difficulty=1` is out-of-turn, other values are `unknown`.
No 500M filter. `gas_used{stage,turn}` supplies a separate distribution.

- `stage="canonical_added"`: blocks added by canonical notifications, including reorg replacements.
- `stage="canonical_removed"`: blocks displaced by reorg, including pure revert.
- `stage="finalized"`: newly finalized headers after the startup baseline, counted once by height.

Canonical additions are events, not permanently finalized unique blocks. Subtract removals to
measure net canonical change in a window, but use finalized counts for stable outcome ratios.
The existing `bsc.consensus.*turn_blocks_total` is a local production/submission-path metric and
is not interchangeable with these chain observations.

Finalized scanning starts from the current finalized head, does not backfill history, runs on a
one-second timer with at most 256 headers per blocking scan, and retries at the first unavailable header.
`finalized_baseline_height`, `finalized_scanned_height`, `finalized_backlog_blocks`,
`finalized_scan_errors_total`, and `finality_regressions_total` expose coverage. A finality regression
invalidates simple once-by-height accounting; investigate before using that interval's ratio.
`missed_notifications_total` flags incomplete canonical event counts; finalized scanning does not
depend on that broadcast queue. Use complete steady windows after startup/catch-up.

For a single node, finalized out-of-turn percentage:

```promql
100 *
(sum(increase(reth_bsc_chain_diagnostics_blocks_total{instance="validator-1:6060",stage="finalized",turn="outofturn"}[5m])) or vector(0))
/
sum(increase(reth_bsc_chain_diagnostics_blocks_total{instance="validator-1:6060",stage="finalized",turn=~"inturn|outofturn"}[5m]))
```

A zero denominator means no finalized sample, not a healthy 0%. Each node sees the same chain;
**do not sum the three nodes into a network block count**. Use deduplicated scrape series
or select one HA replica when calculating absolute counts. In controlled steady performance tests
with healthy, synchronized validators, out-of-turn production should be a separately flagged
abnormal interval. It is consensus-valid and is not by itself proof of a packing bug.

## Validation and rollout

The paired changes pass 69 targeted tests: named-worker accounting (30), parallel-trie
contracts and diagnostics (15), engine state-root strategy (14), BSC chain observations (8),
and BSC attempt accounting (2). Targeted Reth Clippy and the BSC library check also pass.

An optimized local ARM64 microbenchmark of the actual Reth diagnostic implementation,
with a Prometheus recorder installed, measured medians of about 3 microseconds for 11 root
lifecycle milestones, 2 nanoseconds per atomic progress increment, and 125 nanoseconds
aggregate wall time per proof-job guard with eight threads contending on the same task.
These measurements exclude database scanning, log I/O and full-node workload effects;
they support low instrumentation cost but do not establish a node throughput bound.

Keep all three validators on the same binary and compare identical workload/configuration first.
Collect node-local timeout snapshots, queue gauges, build costs and canonical/finalized ratios in
the same interval. Preserve the current 2-second timeout and packing policy while collecting
this evidence. Thread stacks remain necessary where the snapshots only locate a stalled stage.

This document describes new instrumentation, not preconfigured monitoring panels. Deploy the paired revisions together after review.

To validate against the Reth revision pinned in the manifest and lock file:

```sh
cargo check --locked --lib
cargo test --locked --lib node::reorg_metrics::tests
cargo test --locked --lib node::miner::build_diagnostics::tests
```
