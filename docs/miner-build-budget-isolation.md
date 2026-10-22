# Miner build budget and state-root isolation

Speculative out-of-turn builds previously ran during backoff before the job timer
started. Their streaming root task could hold the import runtime's single
`sparse-trie` worker while waiting for more execution updates, causing import's
2-second root timeout.

All local attempts now share a monotonic deadline starting before speculation.
The completion budget is capped by the block interval and slot deadline; the
adaptive root reserve is deducted before selecting transactions. Backoff still
delays publication, but does not reset the execution budget. Expired or canceled
candidates are discarded; reaching the transaction cutoff still allows system
transactions and root completion. Synchronous EVM/provider calls cannot be forcibly
interrupted, so checks reject overruns when those calls return.

The miner owns a persistent, bounded runtime including root/hash session workers,
account/storage proof pools and its overlay pool. Each proof pool uses 1–4 threads,
the overlay pool uses 2. Import retains the engine runtime. A single local-build
permit bounds overlap while an old transaction returns to a cancellation check.
No mutable trie cache is shared between candidates.

For worker metrics, `runtime="miner"` identifies the miner; existing engine worker
labels remain unchanged. Compare named-worker queue/run time, root channel wait
(`reth_trie_diagnostics_root_channel_wait_seconds`), build stop reasons
(`reth_bsc_miner_build_stop_total`) and execution-budget overrun
(`reth_bsc_miner_execution_budget_overrun_seconds`). Root wait includes cancellation/error
exits. Attempt and canonical/finalized out-of-turn metrics retain their existing meanings.

Validation: deterministic budget/cancellation tests, real import-root completion
while the miner update stream remains open, root correctness, and worker namespace
isolation. Performance validation is a matched before/after run at **500M gas limit**;
it has not been run locally. Compare import latency/timeouts/fallbacks, effective TPS,
canonical out-of-turn ratio and CPU/IO, without assuming queue isolation removes
physical resource contention.
