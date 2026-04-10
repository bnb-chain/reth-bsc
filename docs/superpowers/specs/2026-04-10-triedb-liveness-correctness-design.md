# TrieDB Liveness & Correctness Design

## Background

6-node BSC validator network using reth-bsc with triedb (pathdb) mode. Validators are network-isolated across regions, connected via reth-bsc fullnodes (TX forwarding) and geth-bsc sentry nodes (block forwarding).

After a simultaneous restart of all 6 validators, the network experienced block production failure due to `mismatched block state root (triedb validate)` errors caused by pathdb/MDBX inconsistency during reorgs.

Three fixes were already implemented:
1. **Miner guard** (`payload.rs`) — refuses to build when pathdb gap detected
2. **Engine tree skip** (`payload_validator.rs`) — skips triedb validation on pathdb gap
3. **Persistence reorg rewind** (`persistence.rs`) — rewinds pathdb during `on_remove_blocks_above`

This review identified that fix #2 violates data integrity: it writes unverified blocks (with `difflayer: None`) into the tree state, which can be flushed to pathdb without state_root verification.

## Goal

Ensure the entire validator network maintains liveness and data correctness under all runtime scenarios: normal operation, restart, simultaneous all-validator restart, reorgs, and competing forks.

## Design Principles

1. **No unverified data in pathdb**: A block's state_root MUST be independently verified via triedb before it can be written to pathdb
2. **Reject what you can't verify**: If triedb cannot validate a block (pathdb gap), reject it — don't skip validation
3. **Sequential recovery via P2P**: After restart, blocks are validated sequentially from pathdb disk layer, rebuilding difflayers naturally
4. **Trust the miner locally, verify via network**: Miner's own triedb computation is trusted locally; other validators provide independent verification

## Architecture

Two code changes in the reth engine tree, plus confirmation that existing code (miner guard, persistence reorg, fork choice, P2P) is correct.

---

## Change 1: Pathdb Gap Check in `insert_block_or_payload`

**File**: `crates/engine/tree/src/tree/mod.rs`, function `insert_block_or_payload` (~line 2823)

**What**: Before calling the block validator, check whether triedb can validate this block. If not (pathdb gap), buffer the block as `Disconnected` and return `Syncing`.

**Why**: The error handling path for block validation errors in `on_insert_block_error` has only two outcomes: (a) cache block as permanently invalid (`ConsensusError`), or (b) crash the engine (`ProviderError`/fatal). Neither is appropriate for a pathdb gap, which is a transient condition where the block may be valid but we cannot verify it right now.

**Where**: After the `state_provider_builder` check confirms parent state exists, but before calling `execute()`:

```rust
// After line ~2823: Ok(Some(_)) => {}
// Before line ~2830: let is_fork = ...

if rust_eth_triedb::triedb_manager::is_triedb_active() {
    let difflayers = self.state.tree_state.merged_difflayer_by_hash(block_id.parent);
    if difflayers.is_none() {
        let triedb = rust_eth_triedb::triedb_manager::get_global_triedb();
        if let Ok((persist_block, persist_root)) = triedb.latest_persist_state() {
            let parent_header = self.sealed_header_by_hash(block_id.parent)?;
            if parent_header.map(|h| h.state_root()) != Some(persist_root) {
                warn!(
                    target: "engine::tree",
                    block = ?block_num_hash,
                    parent = ?block_id.parent,
                    pathdb_block = persist_block,
                    ?persist_root,
                    "Triedb pathdb gap: no difflayers and parent state root != pathdb disk layer. \
                     Buffering block as Disconnected."
                );
                let block = convert_to_block(self, input)?;
                let missing_ancestor = block.parent_num_hash();
                self.state.buffer.insert_block(block);
                return Ok(InsertPayloadOk::Inserted(BlockStatus::Disconnected {
                    head: self.state.tree_state.current_canonical_head,
                    missing_ancestor,
                }));
            }
        }
    }
}
```

**Effect on P2P recovery**:

```
Disconnected → PayloadStatusEnum::Syncing
  → import service triggers GetBlocksByRange from peer
  → downloads blocks sequentially from pathdb disk layer
  → block N+1 (parent == disk layer) validates normally → difflayer created
  → block N+2 validates (difflayer from N+1 exists)
  → ...
  → try_connect_buffered_blocks dequeues the originally buffered block
```

**When this check fires**: Only in abnormal states (pathdb gap). On normal restart, pathdb == MDBX, so `parent_state_root == persist_root` and the check is transparent.

---

## Change 2: Remove `skip_triedb_root` from Payload Validator

**File**: `crates/engine/tree/src/tree/payload_validator.rs`

**What**: Delete the entire `skip_triedb_root` mechanism (detection + early return path).

**Lines to remove**:
- Lines 402-434: `skip_triedb_root` flag computation
- Lines 500-523: Early return when `skip_triedb_root` is true

**Why**: This code path accepts blocks without triedb state_root verification and sets `difflayer: None`. Such blocks can be flushed to pathdb by `save_blocks`, violating the "no unverified data in pathdb" principle. The pathdb gap case is now handled earlier in `insert_block_or_payload` (Change 1).

**After removal**: If execution reaches `validate_block_with_state_with_triedb`, the block WILL be fully validated via triedb. The only way to skip validation is by being buffered as Disconnected before reaching the validator.

---

## Reviewed and Confirmed: No Changes Needed

### Miner Guard (`payload.rs`)

**Status**: Correct, no changes.

The guard at lines 362-386 (and 888-912 for `build_empty_payload`):
```rust
if is_triedb_active() && triedb_parent_difflayers.is_none() {
    if parent_header.state_root() != persist_root {
        return Err("triedb pathdb gap...");
    }
}
```

Verified scenarios:
- **Normal restart** (pathdb == MDBX == N): `state_root(N) == persist_root(N)` → guard passes → mining proceeds from disk layer → correct
- **All-validator restart**: sync gate (5s timeout) → guard passes → all 6 produce competing N+1 → fork choice resolves → no livelock
- **Pathdb gap**: guard fires → mining skipped → correct (abnormal state, manual intervention)
- **Transient difflayer fetch failure**: guard passes if no gap → mining without prefetcher (slower, correct)

No livelock risk: the guard only blocks mining when pathdb is genuinely inconsistent with MDBX.

### Persistence Reorg Rewind (`persistence.rs`)

**Status**: Correct, no changes.

`on_remove_blocks_above` at lines 125-244 correctly handles:
- **pathdb > new_tip**: extracts changeset → reverses → rewinds pathdb → validates root → flushes
- **pathdb <= new_tip**: standard MDBX cleanup only
- **Same-height reorg**: `find_disk_reorg` correctly finds fork point, rewind covers it
- **Multi-block reorg**: changeset extraction handles multiple blocks
- **Rewind failure** (root mismatch): returns fatal error → persistence service exits → node stops (correct for data corruption)

### Fork Choice Convergence

**Status**: Correct, no livelock risk.

BSC's Parlia fork choice rule (`forkchoice_rule.rs`) is deterministic:
1. `justified_num` (fast finality) → higher wins
2. Total difficulty → higher wins
3. Same TD: shorter chain → earlier timestamp → hash comparison → random (same-block tiebreak)

Backoff mechanism (`consensus.rs:459-596`): deterministic shuffled delays per validator prevent simultaneous block proposals.

Reorg oscillation: both chains' blocks stay in tree state (`reinsert_reorged_blocks`), fork choice deterministically selects one. Each new block height reduces ambiguity.

### P2P Block Sync

**Status**: Correct, no changes.

- `NewBlock` → EVN peers first, then ValidHeader/ValidBlock announcements
- `GetBlocksByRange` → downloads up to 64 missing ancestors on `Syncing` status
- Stale block filter (>64 blocks behind) prevents resource waste
- No peer ban for `Invalid` blocks (PoSA design: Invalid often means timing issue during reorg)
- `try_connect_buffered_blocks` dequeues buffered blocks when their parent becomes available

### InsertExecutedBlock (Miner Submission)

**Status**: No additional validation needed.

When `BSC_SUBMIT_BUILT_PAYLOAD=true`, miner's block bypasses payload_validator. Protection comes from:
1. Miner guard prevents building on pathdb gap
2. Miner's own triedb computation produces correct state_root + difflayer
3. Other 5 validators independently verify via triedb
4. If miner's block is wrong → others reject → reorg → persistence rewind fixes pathdb

---

## Scenario Walkthroughs

### Normal Restart (Single Node)

```
t=0: restart, pathdb=N, MDBX=N, no in-memory difflayers
t=1: block N+1 arrives from P2P
     → insert_block_or_payload: difflayers=None, parent_root==persist_root → no gap
     → validator executes block, triedb computes root from disk layer
     → state_root verified ✓, difflayer created
     → block inserted as Valid
t=2: block N+2 arrives
     → difflayer for N+1 exists → normal validation → Valid
     → chain progresses normally
```

### All 6 Validators Restart Simultaneously

```
t=0: all restart, pathdb=N, MDBX=N
t=0-5s: sync gate blocks mining (is_syncing=true, no FCU)
t=5s: SYNC_GATE_TIMEOUT_SECS expires, mining allowed
     → all 6 attempt block N+1
     → miner guard: state_root(N)==persist_root(N) → passes
     → all 6 produce correct N+1 blocks
t=6s: blocks propagate via P2P/EVN
     → fork choice: deterministic selection → one block wins
     → canonical event fires → miners start N+2
     → chain progresses, no livelock
```

### Restart with Out-of-Order Block Arrival

```
t=0: restart, pathdb=N, MDBX=N
t=1: block N+5 arrives (skipped N+1..N+4)
     → insert_block_or_payload: difflayers=None, parent_root(N+4)!=persist_root(N)
     → pathdb gap detected → block buffered as Disconnected → Syncing
     → import service triggers GetBlocksByRange(N+1..N+5)
t=2: N+1 arrives → parent N == disk layer → validates → difflayer
t=3: N+2 arrives → difflayer(N+1) exists → validates
...
t=5: N+4 arrives → validates
     → try_connect_buffered_blocks: N+5 dequeued → validates → Valid
```

### Reorg During Operation

```
Chain A: blocks A100, A101 (canonical)
Chain B: blocks B100 arrives, fork choice prefers B

In-memory:
  → on_new_head: Reorg { new: [B100], old: [A100, A101] }
  → canonical switches to B100
  → A100, A101 reinserted into tree state

Persistence:
  → If A100,A101 were flushed: find_disk_reorg detects mismatch
  → on_remove_blocks_above(99): rewinds pathdb to fork point
  → save_blocks for B100: flushes B100 difflayer to pathdb
  → Consistent state restored
```

---

## Out of Scope

- **Crash consistency** (pathdb RocksDB and MDBX non-atomic writes): requires startup consistency check, deferred to separate design
- **Startup auto-recovery** for pathdb != MDBX: depends on crash consistency design
- **Async persistence window** (Change 3 from brainstorming): disk layer at chain A while validating chain B. Window is millisecond-level, state_root mismatch catches errors. Risk accepted as extremely low.

---

## Summary of Changes

| File | Change | Lines |
|------|--------|-------|
| `tree/mod.rs` (`insert_block_or_payload`) | Add pathdb gap → Disconnected/Syncing | +~20 |
| `tree/payload_validator.rs` | Remove `skip_triedb_root` logic | -~30 |

Total: ~20 lines added, ~30 lines removed. Net reduction in code complexity.
