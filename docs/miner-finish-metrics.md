# Miner finish diagnostics

Miner-only, no transaction or block labels and no new per-block logs. Existing build
budget and candidate-selection behavior is unchanged. Labels `kind` and `turn` use the
existing regular/rebuild/speculative/empty and inturn/outofturn values. Import, RPC and
bid simulation pass no labels and do not contribute.

- `reth_bsc_build_diagnostics_finish_stage_seconds{kind,turn,stage,outcome}`:
  sequential foreground phase wall time; outcome is completed or interrupted. Includes
  time spent in the final interrupted phase on error or unwind. Completed phase does
  not mean successful candidate. Untaken phases produce no sample.
- `reth_bsc_build_diagnostics_finish_total_seconds{kind,turn,outcome}`:
  entire finish call, including error/panic attempts (unlike existing root_seconds).
- `reth_bsc_build_diagnostics_finish_exits_total{kind,turn,stage,outcome,budget}`:
  exit count, outcome completed/error/panic, attributed to final phase. Budget is status
  at exit: active/deadline_elapsed/cancelled/none. It is NOT the recorded cause of the
  error; a different error can happen after the budget expires.

| Stage | Included work |
| --- | --- |
| entry | Initial completion-budget check |
| system_transactions | executor.finish() and EVM extraction |
| close_state_stream | Close execution hook and check budget |
| root_wait | Handle lock and receiver wait, including deadline checks |
| root_result | Unwrap/clone returned trie updates and store result |
| merge_state | Extract system transactions, merge state transitions, check budget |
| hash_state | Generate hashed post state |
| root_select | Read precomputed root; post-root budget check on fast path |
| sync_root | Fallback deadline guards, clone hashed state, synchronous root, budget check |
| assemble | Prepare transactions/senders, assemble body, cache sinks, recovered block |

These are not CPU times or full background task lifetimes. Final header signing,
choosing the candidate and submitting it happen outside finish and are not included.

Average phase milliseconds, including interrupted samples:

```promql
1000 * sum by (instance, stage) (
  rate(reth_bsc_build_diagnostics_finish_stage_seconds_sum{job="opbnb-qa",kind="regular",turn="inturn"}[2m])
) / sum by (instance, stage) (
  rate(reth_bsc_build_diagnostics_finish_stage_seconds_count{job="opbnb-qa",kind="regular",turn="inturn"}[2m])
)
```

Failed exits (count, not ratio), split by phase and exit-time budget status:

```promql
sum by (instance, stage, budget) (
  increase(reth_bsc_build_diagnostics_finish_exits_total{job="opbnb-qa",kind="regular",turn="inturn",outcome="error"}[5m])
)
```

Phase means have different populations if attempts stop early; do not add them and
present the result as an exact mean total. Use finish_total_seconds for the total.
