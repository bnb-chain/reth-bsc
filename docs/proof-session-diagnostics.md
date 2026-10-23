# Proof session diagnostics

Reth now records named proof-session queue/run time and sampled task ownership, plus
worker dispatch/init and post-execution root phases. This is instrumentation only;
scheduler, cancellation and pool sizing are unchanged.

- `reth_executor_named_session_queue_wait_seconds{runtime,worker,origin}`:
  submission to named closure start (not time since root creation).
- `reth_executor_named_session_run_seconds{runtime,worker,origin}`:
  session lifetime including broadcast and teardown, not CPU time.
- `reth_trie_diagnostics_proof_worker_dispatch_seconds{origin,kind}`:
  named session start to each worker's run entry.
- `reth_trie_diagnostics_phase_seconds{origin,phase,outcome}`:
  storage_init/account_init (provider/cursors), final_root (root_with_updates).
- `reth_trie_diagnostics_finish_marker_seconds{origin,phase}`:
  hash_input/root_input processing delays for the final update marker.
- `reth_trie_diagnostics_proofs_pending_at_finish{origin}`:
  outstanding proof batches when the root consumes the finish marker.
- `reth_trie_diagnostics_finish_drain_seconds{origin}`:
  finish marker consumed to all pending proofs/trie updates drained (completed drains).
- `reth_trie_diagnostics_post_finish_seconds{origin,result}`:
  finish marker sent to root return, including success/cancelled/failed results.

WARN `Sampled named session queued/started/exited` identifies root_task_id, block_number,
origin and runtime/worker. Queued samples include the currently running owner's ID/block
and age; start samples include actual queue_ms and last completed traced session ID.
Samples are bounded to one new selection per worker/runtime per 5 seconds, with related
start/exit records for selected tasks. IDs are log-only, never labels. This is not an
exhaustive audit: the owner at enqueue is not necessarily the sole source of queue delay.

Use INFO file logging (WARN is included); no DEBUG requirement. Existing named_worker
queued_tasks/active_tasks gauges also show unfinished queues, which are censored from
queue-wait histograms until a session actually starts. Independent stage means have
different populations and must not be added as an exact per-task decomposition.
