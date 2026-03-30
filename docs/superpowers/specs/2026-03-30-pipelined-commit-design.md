# Speculative Miner Pipeline Overlap

**Date**: 2026-03-30
**Status**: Approved
**Branch**: `pipelined-commit`
**Base**: `a849233` (`perf-compare-fix-review-findings`)

## Problem

The current production miner still behaves as a serial pipeline even when the payload builder can
reuse in-memory state:

1. Build block `N`
2. Submit block `N`
3. Wait for block `N` to become canonical and durable
4. Start building block `N+1`

That means the next build is still gated by the previous block's import and MDBX commit path. The
10M-account target is dominated by `finish + commit + total`, so a meaningful improvement needs to
overlap the next build with the previous block's persistence rather than only shaving isolated
sub-phases.

## Goal

Move the miner to a one-block speculative pipeline:

- Build block `N+1` immediately after locally submitting block `N`
- Execute block `N+1` against block `N`'s in-memory post-state
- Keep persistence and canonical import on their existing path
- Fall back cleanly to canonical-triggered mining on any mismatch or delay

The intended steady-state shape is:

- current: `build(N) + import/commit(N) + build(N+1) + ...`
- target: `max(build(N+1), import/commit(N))`

## Non-Goals

- No multi-block speculative chain
- No triedb difflayer chaining in the first landing
- No change to transaction selection, block assembly, or consensus rules
- No benchmark-only shortcuts in the production miner path

## Architecture

The change has four production components.

### 1. Speculative Trigger Point

`ResultWorkWorker` becomes the earliest safe trigger for the next local build.

After `finalize_payload()` and after the locally mined block is accepted by the import-service
channel, the miner derives the next `MiningContext` immediately instead of waiting for the next
canonical-state notification. This is the step that creates real overlap; `BundleStateOverlay`
alone is not sufficient because the current canonical notification arrives only after persistence.

### 2. Explicit Durable Base + Overlay State

Speculative execution cannot read from `state_by_block_hash(parent_hash)` when the speculative
parent has not been committed to MDBX yet.

`MiningContext` must therefore carry two separate parent concepts:

- `logical_parent_header` / `logical_parent_snapshot`
  These describe the parent block the new block is being built on.
- `state_base_hash`
  This is the latest durable block hash that definitely exists in MDBX.

`build_payload()` opens the state provider from `state_base_hash`, then layers the speculative
parent's in-memory post-state on top via `BundleStateOverlay`.

The read stack becomes:

`CachedReads -> BundleStateOverlay -> StateProviderDatabase(MDBX at state_base_hash)`

### 3. One-Block Pending Local Head

The miner tracks at most one locally submitted but not-yet-canonical block:

```text
PendingLocalHead {
  block_number,
  block_hash,
  parent_hash,
  cached_reads,
  bundle_state,
  derived_snapshot,
  durable_base_hash,
}
```

This is not a queue. It is a single guardrail that allows one speculative child build and prevents
the miner from chaining unbounded overlays or running multiple uncommitted heads ahead of import.

### 4. Derived Snapshot For The Speculative Parent

The next speculative build must use the speculative parent's Parlia snapshot, not the durable
base's snapshot.

The miner already has enough information at finalize time to derive that snapshot:

- finalized sealed header
- previous snapshot
- validator-set update captured during execution
- turn-length update captured during execution

The speculative path derives the next parent snapshot directly and caches the finalized header
immediately so snapshot reconstruction and consensus helpers can operate before the canonical
notification arrives.

## Data Flow

### Canonical Path

The existing path remains the source of truth:

1. canonical notification arrives
2. `NewWorkWorker` derives `MiningContext`
3. `MainWorkWorker` starts a payload job
4. `ResultWorkWorker` finalizes and submits the mined block
5. import service inserts the executed block and fork-choice progresses

### Speculative Local Path

The new fast path runs only after a successful local submission:

1. `ResultWorkWorker` finalizes a locally built payload
2. It submits the payload to the import service
3. On successful handoff, it records `PendingLocalHead`
4. It derives a speculative `MiningContext` for child block `N+1`
5. `MainWorkWorker` starts the speculative payload job immediately
6. The later canonical notification for block `N` either confirms the speculative path or cancels it

### Required Context Shape

The speculative `MiningContext` needs:

- speculative parent header = finalized block `N`
- speculative parent snapshot = snapshot derived from block `N`
- `cached_reads` from block `N`
- `prev_bundle_state` from block `N`
- `state_base_hash` = latest durable canonical block before block `N`
- a marker that the context is speculative rather than canonical

## Correctness Rules

Speculative mining is allowed only under strict rules.

### Rule 1: Only One Block Ahead

The miner may build on top of one locally submitted pending head. If import has not caught up by
the time another speculative child would be needed, the miner stops speculating and falls back to
the canonical path.

### Rule 2: Successful Local Submission Required

If payload submission to the import service fails, no speculative child context is created.

### Rule 3: Canonical Context Always Wins

If a canonical mining context arrives while a speculative build is in flight, the canonical context
supersedes it. The speculative job is aborted unless it is already building on the same confirmed
head.

### Rule 4: Reorg Or Head Mismatch Clears Speculation

When the next canonical notification does not match the pending local head, the miner clears:

- pending local head tracking
- speculative overlay state
- speculative cached reads

and resumes ordinary canonical-triggered mining.

### Rule 5: Durable Base Must Stay Explicit

The payload builder must never infer the MDBX base from the speculative parent hash. It must use
the explicit durable base carried in the mining context.

## Production Changes

### `src/node/miner/bsc_miner.rs`

- Extend `MiningContext` with `state_base_hash` and a speculative/canonical source marker
- Add a small `PendingLocalHead` tracker owned by the miner side
- Teach `ResultWorkWorker` to derive and enqueue speculative child work after successful local
  submission
- Teach `NewWorkWorker` and `MainWorkWorker` to cancel or supersede speculative contexts when
  canonical state disagrees

### `src/node/miner/payload.rs`

- Change payload building to open the state provider from `state_base_hash`
- Keep the overlay path, but make it explicit that the overlay parent may differ from the durable
  MDBX base
- Preserve existing `finish_with_difflayer()` behavior without enabling consecutive-block
  difflayer chaining yet

### `src/node/evm/overlay.rs`

- Keep `BundleStateOverlay`
- Treat it as production infrastructure for speculative execution rather than a benchmark helper

### `src/node/network/block_import/service.rs`

- No persistence semantics change in the first landing
- Import remains the authoritative path for executed block insertion and fork-choice advancement
- The miner's speculative trigger starts only after the import channel has accepted the locally
  mined payload

## Benchmark Strategy

The benchmark is the detector, not the design source of truth.

Validation should happen in two steps:

1. fast iteration on cached 1M-account runs to detect regressions quickly
2. final confirmation on cached 10M-account runs with the required 500M-gas custom genesis

The benchmark harness must be updated only as needed so it exercises the same production behavior:

- explicit durable base hash
- speculative next-block trigger timing
- one-block pending head behavior

The benchmark should report at least:

- tx execution
- finish
- commit
- total
- speculative trigger / handoff timing
- fallback or drop counts for speculative contexts

## Testing

### Unit / Integration

- `BundleStateOverlay` serves account/storage reads from the overlay before MDBX
- speculative `MiningContext` derives the correct durable base and logical parent
- derived snapshots match the snapshot later observed through the canonical path
- reorg or canonical mismatch clears pending speculative state
- no second speculative level is created while one pending local head already exists

### Benchmark Validation

Primary workload:

- 100 blocks
- 6000 tx/block
- 5000 funded accounts
- 10,000,000 background accounts
- 1 storage slot/account
- `--triedb`

Required benchmark flags:

- `--genesis /Users/user/.config/superpowers/worktrees/reth-bsc/bench-tmp/genesis_local_gas500m.json`
- `--cache-dir ...`
- `--reuse-genesis-db`
- `--reuse-post-setup-db`

## Risks

### Trigger Too Early

If the speculative trigger runs before the payload is safely handed to import, the miner can build
on a block that was never accepted. The fix is to trigger only after a successful send to the
import path.

### Snapshot Drift

If the speculative snapshot derivation disagrees with the eventual canonical snapshot, the miner
can produce invalid timing decisions or headers. The fix is block-by-block validation against the
canonical path and aggressive fallback on mismatch.

### Speculative State Leak

If pending local head state survives a reorg or failed submission, later builds may read stale
state. The fix is to centralize speculative state in one tracker and clear it on any mismatch.

### Ceiling Lower Than Theoretical Maximum

Even with overlap, production orchestration may limit improvement before reaching the ideal
`max(build, commit)` bound. The benchmark will determine how much of the theoretical gain is
actually realized.

## Expected Outcome

This design is the highest-upside production path because it attacks the real serialization point:
the gap between local block completion and the next block's start.

It should materially reduce steady-state `total` if production behavior is still commit-bound after
speculation is introduced. If the node becomes limited by build orchestration instead, the new
timings will make that explicit and define the next optimization target.
