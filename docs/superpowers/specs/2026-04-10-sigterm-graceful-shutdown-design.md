# SIGTERM Graceful Shutdown Design

## Problem

When reth-bsc receives SIGTERM (normal `kill`), `run_until_ctrl_c` immediately returns from its `tokio::select!` without triggering engine tree's `finish_termination`. In-memory blocks are lost, and with `--db.sync-mode safe-no-sync`, MDBX commits that haven't been fsynced are also lost. This causes pathdb (RocksDB, always fsynced) to be ahead of MDBX on restart.

## Root Cause

`run_until_ctrl_c` (reth core, `crates/cli/runner/src/lib.rs:283`) catches SIGTERM and returns. The `EngineShutdown` mechanism exists but is only callable via RPC — it's never invoked on SIGTERM.

## Fix

In reth-bsc's `main.rs`, after `exit_future.await` returns (SIGTERM received), call `engine_shutdown.shutdown()` to trigger graceful persistence before the process exits.

### Code Change

**File**: `/Users/jiaqiwang/workspace/reth-bsc/src/main.rs`

Replace:
```rust
exit_future.await
```

With:
```rust
exit_future.await;

// Graceful shutdown: persist all remaining in-memory blocks to MDBX + pathdb
// before exit. Without this, `safe-no-sync` mode can lose unfsynced MDBX data
// while pathdb (RocksDB) retains it, causing a gap on restart.
//
// finish_termination runs synchronously inside the engine tree's event handler,
// so no new blocks are accepted during persistence. The 30s timeout prevents
// hanging if persistence is stuck.
tracing::info!(target: "reth::cli", "SIGTERM received, persisting remaining blocks before exit...");
let engine_shutdown = node.add_ons_handle.engine_shutdown.clone();
if let Some(done_rx) = engine_shutdown.shutdown() {
    match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        done_rx,
    ).await {
        Ok(Ok(())) => {
            tracing::info!(target: "reth::cli", "Graceful shutdown complete — all blocks persisted");
        }
        Ok(Err(_)) => {
            tracing::warn!(target: "reth::cli", "Engine shutdown channel dropped before completion");
        }
        Err(_) => {
            tracing::error!(target: "reth::cli", "Engine shutdown timed out after 30s — some blocks may not be persisted");
        }
    }
} else {
    tracing::debug!(target: "reth::cli", "Engine shutdown already triggered, skipping");
}
```

### Safety Guarantees

1. **No inflight request flooding**: `finish_termination` runs inside the engine tree's event handler synchronously. While it runs, the engine tree does NOT process any new events (new_payload, FCU, etc.). New requests queue in channels and are dropped when the engine exits.

2. **Bounded completion time**: 30s timeout. If persistence hangs (e.g., RocksDB stuck), the process exits anyway.

3. **Idempotent**: `engine_shutdown.shutdown()` returns `None` if already called. Safe to call multiple times.

4. **No reth core changes**: Entirely in reth-bsc layer (`main.rs`).
