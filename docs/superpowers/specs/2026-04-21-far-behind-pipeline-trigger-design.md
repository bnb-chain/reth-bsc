# Far-Behind → Pipeline Sync Trigger: Design

**Status**: Proposed. Implementation pending.

**Related**:
- [`2026-04-17-p2p-fork-recovery-design.md`](./2026-04-17-p2p-fork-recovery-design.md) — this spec overrides that doc's explicit non-goal *"No fallback to staged sync"*; rationale in §"Relationship to the 2026-04-17 fork-recovery design" below.
- [`2026-04-18-pathdb-gap-fork-livelock-scenario.md`](./2026-04-18-pathdb-gap-fork-livelock-scenario.md) — Scenario V5 ("Fork deeper than `MAX_FORK_DEPTH`") documents the same symptom from the validator angle; this design addresses the simpler full-node-catch-up variant.
- [`2026-04-17-triedb-mdbx-startup-alignment-design.md`](./2026-04-17-triedb-mdbx-startup-alignment-design.md) — explains why the startup `gap=0 outcome=noop` log is not the bug.

---

## Background

reth-bsc has no consensus layer. In upstream reth, a CL drives sync by issuing `forkchoiceUpdated` with the remote head; engine-tree compares the target distance against `MIN_BLOCKS_FOR_PIPELINE_RUN` (default 32) and routes to the staged backfill pipeline when the gap is large, or to live/tree sync when small. BSC has no such driver, so reth-bsc substitutes `BscBlockImport::on_new_block_hashes` in `src/node/network/block_import/service.rs`: every `NewBlockHashes` broadcast from a peer is dispatched into a block-import path that is expected to both (a) pull missing ancestors and (b) steer the engine toward the announced head.

Between the initial reth-bsc implementation and commit `cfad327` (`feat(p2p): scaffold fork_recover module`, 2026-04-17), `on_new_block_hashes` simulated a CL by synthesizing an FCU per announced hash:

```rust
// pre-cfad327, service.rs::on_new_block_hashes
let forkchoice_state = ForkchoiceState {
    head_block_hash: hash_number.hash,
    safe_block_hash: B256::ZERO,
    finalized_block_hash: B256::ZERO,
};
engine.fork_choice_updated(forkchoice_state, None, V1).await
// comment: "Requesting block download by simulating FCU for NewBlockHashes"
```

Commit `cfad327` and its follow-ups replaced this with a fork-aware ancestor-walk primitive in `src/node/network/block_import/fork_recover.rs`, bounded at `MAX_FORK_DEPTH` (originally 256, later raised to 2048 in `753c67e`). That design explicitly chose not to fall back to staged sync:

> **Non-Goals** (`2026-04-17-p2p-fork-recovery-design.md:56`): "*No fallback to staged sync. The 256-block depth cap bounds recovery to fork depths that should be resolved via the fast path only.*"

That choice is correct for its stated scenario (live fork recovery on a mostly-caught-up node), but it silently removed the only mechanism by which an offline-for-hours node could catch up. The regression this spec fixes is the full-node, non-forked "long-offline restart" case that used to work by virtue of the FCU path.

## Problem

### Observed symptom

Reference log: `start-sync.log` (39 MB, 286 074 lines, window 2026-04-21 06:34:29 → 06:37:47 UTC).

| Fact | Value |
|---|---|
| Local canonical tip at startup | `19 143 662` |
| First peer-head seen | `19 148 958` |
| Peer head at log end | `19 149 348` |
| Gap at startup | ≈ 5 296 blocks |
| Gap at log end | ≈ 5 686 blocks (growing) |
| `fork_recover` invocations spawned | 391 |
| `BlocksByRange` round-trips | 141 224 |
| `Fork recovery failed` reports completed | 163 (100 % with identical error) |
| Blocks committed | **0** |

Every completed `fork_recover` attempt fails with exactly:

```
WARN bsc::block_import: Fork recovery failed
     head_hash=0x… head_num=19148959
     error=no common ancestor found within MAX_FORK_DEPTH=2048 blocks
```

No `Triedb pathdb gap`, no `signed recently`, no `Syncing mid-chain` messages — i.e., this is not the V3/V4 livelock variants from `2026-04-18-pathdb-gap-fork-livelock-scenario.md` and it is not a validator recent-signer issue. It is a plain full-node whose datadir was offline long enough that the peer chain moved more than `MAX_FORK_DEPTH` blocks ahead, and `fork_recover` cannot help because it is specifically designed to stop looking past 2048 ancestors.

### Preconditions

This bug fires when *all* of the following hold at startup:

1. `peer_best.head_num - local.canonical_head_num > MAX_FORK_DEPTH` (currently 2048).
2. The node has no live CL (BSC always; not a function of operational mode).
3. `fork_recover` is the only routing path from `NewBlockHashes` into the engine (true since `cfad327`).

There is no dependency on TrieDB, mining, fork topology, or validator count. At 3 s blocks on BSC mainnet, precondition (1) is reached after about 1 h 42 min of offline time. Any clean restart after a longer outage lands in this state.

### Why the existing retry paths cannot resolve it

- `FailedHeadsCooler` (30 s cooldown, `fork_recover.rs:48`) only throttles retries on identical heads. Peers advance their heads every block, so each new announcement is a fresh head and the cooldown is not engaged.
- `recover_ancestors` Phase 1 (`fork_recover.rs::discover_fork_blocks`) walks at most 2048 blocks backward from `head_hash` before giving up with `ForkTooDeep`. A 5000-block gap cannot be closed; a 1 000 000-block gap cannot be closed. Raising the depth cap buys more time but does not change the class of the problem (see `##non-goals`).
- Startup alignment (`align_mdbx_to_triedb_at_startup`) finishes with `outcome="noop"` because MDBX and pathdb already agree at the local tip — the two backends are internally consistent; they just aren't the network tip. This is the correct behaviour for alignment and should not change.
- `check_pipeline_consistency_under_triedb` returns `Ok(None)` because all stages are aligned at the local tip, emitting `Pipeline sync progress is consistent and backends are aligned; starting live sync` (`consensus::engine` target). There is no stage-checkpoint skew to trigger pipeline backfill here; backfill would have to be triggered externally (i.e., from FCU).

## Root Cause

A regression. Two commits are the inflection point:

| Commit | Date | Change |
|---|---|---|
| `cfad327` | 2026-04-17 | Scaffolds `fork_recover` module; replaces the FCU-per-announcement call site in `on_new_block_hashes` with `spawn_fork_recover`. |
| `753c67e` | (later) | Raises `MAX_FORK_DEPTH` from 256 to 2048. Expands the operating range but does not add a fallback for gaps > 2048. |

Before `cfad327`, the FCU path carried two responsibilities that were subsequently split unevenly: short-fork recovery moved to `fork_recover` correctly, but the long-distance catch-up case was orphaned. Nothing in the current code routes "gap >> MAX_FORK_DEPTH" to the staged pipeline.

## Why the Old FCU Path Works for Large Gaps

Verified in pinned reth rev `ef46a48` (BSC fork of reth, `bnb-chain/reth`), checkout at `~/.cargo/git/checkouts/reth-8428740b6850f139/ef46a48/`.

Engine-tree's FCU handling (`crates/engine/tree/src/tree/mod.rs:1106` `on_forkchoice_updated`):

1. Validate the forkchoice state (reject zero head, reject invalid-ancestor heads). `:1118`.
2. `handle_canonical_head` — return VALID if target is already canonical. `:1123`, `:1196`.
3. `apply_chain_update` — try to find target in tree-state. `:1129`, `:1248`.
4. `handle_missing_block` — emit `DownloadRequest::single_block(target)` and return Syncing. `:1134`, `:1354`.

After `handle_missing_block` pulls the target, engine-tree calls `backfill_sync_target` (`:2432`) to decide whether to stay in one-block-at-a-time download mode or kick the staged pipeline. The decision uses `exceeds_backfill_run_threshold` (`:2411`) with threshold `DEFAULT_MIN_BLOCKS_FOR_PIPELINE_RUN = 32` (`crates/engine/primitives/src/config.rs:72`). For BSC, the FCU carries `finalized_block_hash = B256::ZERO`, which triggers the **optimistic-sync** branch at `mod.rs:2483-2496`:

```text
// OPTIMISTIC SYNCING
//
// It can happen when the node is doing an
// optimistic sync, where the CL has no knowledge of the finalized hash,
// but is expecting the EL to sync as high
// as possible before finalizing.
//
// This usually doesn't happen on ETH mainnet since CLs use the more
// secure checkpoint syncing.
//
// However, optimism chains will do this. The risk of a reorg is however
// low.
return Some(state.head_block_hash)
```

Engine-tree therefore accepts the head hash itself as the backfill target, and the pipeline is started via `backfill.rs:134-154` `try_spawn_pipeline` → `pipeline.run_as_fut(Some(target))`. The `head_block_hash` does not need to be pre-downloaded; the pipeline's headers stage walks backward from the target via P2P.

This is the path the pre-`cfad327` code relied on. The mechanism still exists, intact, in the pinned rev.

## Why Pipeline Forward Is Viable Under TrieDB

The scare concern is that TrieDB mode disables merkle stages and the pipeline might only update MDBX while leaving pathdb behind. Direct reading of `crates/stages/stages/src/stages/execution.rs` in the pinned rev disproves this:

`execution.rs:480-548` contains an explicit `if is_triedb_active()` branch in the forward-execution path:

1. Read current `(triedb_block, triedb_root) = triedb.latest_persist_state()`. `:484-485`
2. If `triedb_block < start_block - 1`, return `StageError::TrieDBBehind` (unwinds cleanly). `:490-499`
3. Build `HashedPostState` from the execution result, convert to `triedb_hashed_post_state`. `:520-522`
4. `triedb.intermediate_and_commit_hashed_post_state(...)` → returns `(new_root, difflayer)`. `:534-541`
5. Assert `new_root == header.state_root` (from the block we just executed). `:543`
6. `triedb.flush(stage_progress, new_root, Some(difflayer))`. `:546-548`

The same branch exists symmetrically for unwind (`:604-645`). So ExecutionStage in the pinned rev writes pathdb inline with MDBX during forward batch execution; there is no deferred reconciliation step required.

Parlia header validation during the Headers stage flows through `Arc<dyn FullConsensus>` (= `BscConsensus`) wired at `crates/node/builder/src/setup.rs:120`, which `ReverseHeadersDownloaderBuilder` uses during batch header fetch (same file, earlier in `build_pipeline`). Seals and extra-data are therefore validated as the pipeline downloads headers in bulk.

## Relationship to the 2026-04-17 fork-recovery design

The earlier design (`2026-04-17-p2p-fork-recovery-design.md`) states as a non-goal:

> "No fallback to staged sync. The 256-block depth cap bounds recovery to fork depths that should be resolved via the fast path only."

Reading that document end to end, every Problem-section scenario it analyses is a **live fork** between nodes whose tips are close (gap ≪ depth cap). Those scenarios are correctly handled by `fork_recover`'s sequential `new_payload` approach, and nothing in this spec changes that path. The design above did not consider the "offline for hours, gap vastly exceeds depth cap" scenario, which is not a fork at all — it is straightforward catch-up. The two call sites merged into a single primitive in 2026-04-17 were both live-fork sites; neither was explicitly a long-offline-restart site (that case used to work by coincidence, because the pre-`cfad327` code still went through FCU).

This spec therefore does not contradict the earlier design — it restores a concern the earlier design did not have to account for (because pre-`cfad327` FCU still handled it implicitly), by adding a distance-based router *above* the existing `fork_recover` primitive. The `fork_recover` module itself, including `MAX_FORK_DEPTH = 2048` and the sequential-import discipline, is retained untouched.

## Goal

Restore long-offline-restart catch-up by adding a small distance-based router in `on_new_block_hashes`:

- Gap ≤ `PIPELINE_TRIGGER_DELTA` → existing `fork_recover` path (unchanged).
- Gap > `PIPELINE_TRIGGER_DELTA` → synthesize FCU to engine; skip `fork_recover` for this announcement.

Engine-tree's optimistic-sync branch then drives the staged pipeline to close the gap. When the gap shrinks to ≤ `PIPELINE_TRIGGER_DELTA`, subsequent announcements naturally revert to the `fork_recover` fast path.

## Non-Goals

- **Raising `MAX_FORK_DEPTH` further.** Unbounded growth makes every fork-recovery attempt O(depth) in peer requests (the reference log shows 141 K `BlocksByRange` in 3 min — fork_recover is not designed for large gaps and will not scale by tuning). Raising the cap trades one cliff for another.
- **Modifying `fork_recover.rs`.** The depth cap, `FailedHeadsCooler`, `RecoveringHeadGuard`, and `recover_ancestors` sequencing are all correct for their intended scenario (live fork, gap ≪ depth cap). No changes.
- **Modifying engine-tree, backfill, or any stage.** The pipeline trigger mechanism already exists in the pinned rev and is well exercised. This design only adds a caller.
- **Lowering `MIN_BLOCKS_FOR_PIPELINE_RUN` from 32.** The engine-tree threshold is independently sensible; we layer our 2048-scale threshold on top of it rather than tuning it.
- **Addressing pathdb-gap livelock (spec 2026-04-18 V1-V4).** Those require validator-side remediation. V5 (fork-too-deep) is partially addressed here for the non-forked case; the forked case remains out of scope.
- **Peer-head validation before FCU.** See §"Trust model" below for rationale.

## Design

### Routing in `on_new_block_hashes`

Location: `src/node/network/block_import/service.rs` around the existing `on_new_block_hashes` (current lines ~489-568).

Conceptual pseudocode (not literal diff):

```rust
fn on_new_block_hashes(&mut self, hashes: NewBlockHashes, peer_id: PeerId) {
    let local_tip = /* canonical head number from forkchoice_engine.provider */;

    for hash_number in hashes.0 {
        // existing dedup / cooldown / recovering-heads guards stay as-is

        let delta = hash_number.number.saturating_sub(local_tip);

        if delta > PIPELINE_TRIGGER_DELTA {
            self.spawn_fcu_for_backfill(hash_number.hash, hash_number.number);
            continue;
        }

        // existing fork_recover spawn path
        self.spawn_fork_recover(peer_id, hash_number);
    }
}
```

Where `spawn_fcu_for_backfill` wraps the old simulated-FCU logic, executed on a `tokio::spawn`ed task so the import loop is not blocked on the engine response:

```rust
fn spawn_fcu_for_backfill(&self, head_hash: B256, head_num: u64) {
    let engine = self.engine.clone();
    // Per-head dedup: use the same `processed_blocks`/`recovering_heads`
    // plumbing so we don't fire overlapping FCUs for the same head.
    // Choose exactly one of these to mark; both work, processed_blocks is
    // lighter since it already exists with the right LRU semantics.
    self.processed_blocks.insert(head_hash);

    tokio::spawn(async move {
        let state = ForkchoiceState {
            head_block_hash: head_hash,
            safe_block_hash: B256::ZERO,
            finalized_block_hash: B256::ZERO,
        };
        match engine.fork_choice_updated(state, None, EngineApiMessageVersion::V1).await {
            Ok(ret) => tracing::info!(
                target: "bsc::block_import",
                head_hash = %head_hash,
                head_num,
                status = ?ret.payload_status.status,
                "Pipeline-trigger FCU dispatched"
            ),
            Err(err) => tracing::warn!(
                target: "bsc::block_import",
                head_hash = %head_hash,
                error = %err,
                "Pipeline-trigger FCU failed"
            ),
        }
    });
}
```

### Threshold selection

`PIPELINE_TRIGGER_DELTA` is defined at the top of `src/node/network/block_import/service.rs` (or `fork_recover.rs`, adjacent to `MAX_FORK_DEPTH`).

Recommended value: **`PIPELINE_TRIGGER_DELTA = MAX_FORK_DEPTH` (2048)**.

- Below 2048, `fork_recover` is designed to succeed; switching earlier would divert legitimate short-fork work to the heavier pipeline path.
- At 2048, `fork_recover` is at its declared operational limit; anything beyond will fail.
- Above 2048 (e.g., 4096 for hysteresis), we knowingly let `fork_recover` fail once or twice before switching. Harmless but wasteful; not worth the extra knob.

Equality case (gap == 2048): route to `fork_recover`. Matches the module's invariant (`:179` `if walked >= MAX_FORK_DEPTH`).

### Concurrency suppression

The reference log shows 141 K `BlocksByRange` round-trips while the node was stalled — most of them `fork_recover` attempts on blocks the node could never reach. Once we enter pipeline-trigger mode, we must not also spawn `fork_recover` for announcements on the same (far-away) head region.

The existing per-hash dedup (`processed_blocks`, `recovering_heads`, `failed_heads` cooldown at `service.rs:491-512`) already dedups identical heads. But peers advance heads every block, so each new head bypasses dedup. Two options:

**Option A (minimal, recommended):** Rely on `processed_blocks` LRU marking after each FCU dispatch. Every head that triggered an FCU is then skipped on subsequent announcements. Coverage is LRU-bounded (100 entries, `LRU_PROCESSED_BLOCKS_SIZE`), which is enough at BSC block intervals (100 blocks ≈ 5 min cadence).

**Option B (stricter):** Add a `pipeline_active: Arc<AtomicBool>` set while any pipeline-trigger FCU is in-flight (or while engine is in backfill mode). When true, `on_new_block_hashes` drops announcements > `PIPELINE_TRIGGER_DELTA` without emitting further FCUs; announcements ≤ `PIPELINE_TRIGGER_DELTA` can still go to `fork_recover` for close-in heads. Requires hooking the pipeline start/finish signals from engine-tree or inferring from FCU return status.

Option A is sufficient for the reference incident and introduces no new state. Ship A; escalate to B only if we observe FCU thrash empirically.

### Trust model

The `head_block_hash` received from `NewBlockHashes` is unvalidated at the point of FCU dispatch. This matches the pre-`cfad327` behaviour and is acceptable because:

1. The pipeline's Headers stage downloads and validates each header batch via `Arc<dyn FullConsensus> = BscConsensus` (Parlia seal + extra-data). A malicious peer cannot inject invalid blocks; the stage rejects them.
2. Engine-tree's optimistic-sync branch is BSC-agnostic — it trusts the EL caller to have made a reasonable choice, which we are in a position to make given we only sent this FCU because the peer announced this head in `NewBlockHashes`. A malicious peer can cause wasted pipeline work (a few seconds of validation against its forged headers), but cannot pollute canonical state.
3. If multiple peers announce different remote heads far ahead, we may fire several FCUs. The engine handles this by letting later FCUs preempt earlier ones; the pipeline retargets as needed. No new race is introduced.

If future work warrants, a pre-FCU filter "require ≥ 2 peers to announce the same head" would harden this, but it is not required for correctness on mainnet/testnet where Parlia consensus already bounds the set of acceptable heads to one at each height (modulo short reorgs).

### Exit back to live sync

When the pipeline catches up to approximately the announced head (within `MIN_BLOCKS_FOR_PIPELINE_RUN = 32`), engine-tree's `on_backfill_sync_finished` (`mod.rs:1570`) resets tree-state to the backfilled height and clears `canonical_in_memory_state`. Subsequent FCUs then flow through `handle_canonical_head` (:1196) — which will match, because canonical has advanced — and `fork_recover` / live-sync resume for the residual few-block gap between pipeline exit and live tip.

No new code is required to drive this transition; it is entirely an engine-tree concern.

### Interaction with `on_new_payload` (BSC block path)

`on_new_block` in `service.rs` (the full-block path, not the hashes path) is unchanged. A full block that arrives and cannot be imported (`new_payload` returns Syncing) remains handled by `fork_recover` via its Phase-2 logic (`fork_recover.rs:292`). Full-block ingestion is typically for small reorgs or nearby new blocks; if a remote node pushes a full block ≫ local tip, the `new_payload` path will Syncing, and the follow-up `NewBlockHashes` announcement (which always accompanies such pushes in BSC) will trip the distance router and start pipeline sync. This is belt-and-braces; no changes needed.

## Testing / Acceptance

### Regression reproduction (must fail before fix, pass after)

1. Sync a reth-bsc node with `--statedb.triedb` to a recent tip on bsc-mainnet or a testnet.
2. `SIGTERM` the node. Confirm clean shutdown (pathdb flush log).
3. Wait **≥ 2 hours** of wall-clock time so the network's head moves more than 2048 blocks ahead.
4. Restart the node. Observe:
   - Startup log `Startup alignment: backends already in sync gap=0 outcome="noop"` (unchanged).
   - Startup log `Pipeline sync progress is consistent and backends are aligned; starting live sync` (unchanged).
5. **Before fix**: node emits `Fork recovery failed ... error=no common ancestor found within MAX_FORK_DEPTH=2048 blocks` indefinitely; canonical tip does not advance.
6. **After fix**: within ≤ 10 s of the first `NewBlockHashes` from a connected peer, log shows `Pipeline-trigger FCU dispatched`; then engine-tree emits `Setting head hash as an optimistic backfill target` (target: `engine::tree`); then stage logs appear (`sync::stages::headers`, `sync::stages::bodies`, `sync::stages::execution`, and specifically `Begin update triedb` / `End update triedb` in the execution stage). Local tip advances in bulk.

### Unit/smoke tests

- In `service.rs`, add a test that a synthesised `NewBlockHashes` with `head_num - local_tip > 2048` dispatches an FCU (observable via a mock `ConsensusEngineHandle`) and does **not** spawn `fork_recover`.
- Add a test for `head_num - local_tip == 2048` → `fork_recover` path (i.e., equality goes to existing path).
- Add a test for `head_num - local_tip = 2049` → FCU path.
- `fork_recover.rs` tests are unchanged.

### Manual verification

Run on a qanet or testnet with an artificial 4 h downtime:

- Confirm total peer requests during catch-up drop from ~140 K to O(a few thousand per 1 000 blocks) (pipeline batches vs one-block-at-a-time).
- Confirm triedb persist root advances (`triedb.latest_persist_state()` returns a higher block after catch-up).
- Confirm no duplicate FCUs storm the engine: `grep "Pipeline-trigger FCU"` should show one per peer-head advance cycle during catch-up, not one per announcement.

### Cross-scenario guardrails

- Re-run the `2026-04-18` pathdb-gap livelock repro on the patched code. This fix must not change behaviour when `gap ≤ 2048` (the livelock scenario's "m" and "n" are both ≤ 10). The router short-circuits to `fork_recover` in that range.
- Re-run the `2026-04-17-p2p-fork-recovery-design.md` test suite. No regressions expected (fork_recover path untouched).

## Open Questions

### OQ1. Pipeline resumability if the process crashes mid-backfill

If the node crashes during a large pipeline run, does `check_pipeline_consistency_under_triedb` on the next start correctly resume from the highest consistent stage checkpoint? Design expectation: yes (that function exists precisely for pipeline-interrupt recovery). Verification: needed via a crash-and-restart test. Not a blocker for this design — worst case is that startup alignment (R2 in the 2026-04-18 spec) reports an unrecoverable backward gap and the operator wipes, same as today.

### OQ2. Multi-peer divergent announcements during catch-up

If two peers announce different heads `peer_A.head_num = local + 4000` and `peer_B.head_num = local + 3999`, we will fire two FCUs. Engine-tree's pipeline retargets on each FCU. Expected: retargeting is cheap (pipeline resumes from current stage checkpoint against the new target). Needs confirmation that pipeline retarget does not restart from block 0.

### OQ3. EVN / proxy-mode interaction

The EVN feature (`src/node/network/evn.rs`) is off by default and activates only when head-timestamp lag < `BSC_EVN_SYNC_LAG_SECS = 30 s` (per CLAUDE.md). During catch-up via pipeline, head lag is large, so EVN is inactive. Expected: no interaction. Worth re-reading `evn.rs` once during implementation to confirm no latent assumption is broken.

## Reference Evidence

- **Log**: `start-sync.log` (Apr 21 2026, 06:34:29–06:37:47 UTC; 39 MB; 286 074 lines). Reproduces the full symptom in 3 min of runtime.
- **Regression commits** (on branch `fix/mining-timestamp-drift`, repo `reth-bsc`):
  - `cfad327 feat(p2p): scaffold fork_recover module with error types and constants` (2026-04-17) — introduces `fork_recover`; drops FCU path in `on_new_block_hashes`.
  - `753c67e chore: update MAX_FORK_DEPTH` — raises cap from 256 to 2048.
- **Pinned reth rev**: `ef46a48` (`bnb-chain/reth`). All engine-tree / stages / builder citations in this spec reference that checkout.
- **Key upstream citations**:
  - `crates/engine/tree/src/tree/mod.rs:1106` — `on_forkchoice_updated`
  - `crates/engine/tree/src/tree/mod.rs:2411` — `exceeds_backfill_run_threshold`
  - `crates/engine/tree/src/tree/mod.rs:2483-2496` — optimistic-sync branch (the workhorse of this design)
  - `crates/engine/tree/src/backfill.rs:134-154` — `try_spawn_pipeline`
  - `crates/engine/primitives/src/config.rs:72` — `DEFAULT_MIN_BLOCKS_FOR_PIPELINE_RUN = 32`
  - `crates/stages/stages/src/stages/execution.rs:480-548` — TrieDB-aware forward-execution branch
  - `crates/node/builder/src/setup.rs:120` — `Arc<dyn FullConsensus> = BscConsensus` wiring into headers downloader
- **reth-bsc citations**:
  - `src/node/network/block_import/service.rs:489-567` — current `on_new_block_hashes` implementation
  - `src/node/network/block_import/fork_recover.rs:26` — `MAX_FORK_DEPTH = 2048`
  - `src/node/network/block_import/fork_recover.rs:56,179` — `ForkTooDeep` failure and its trigger

## Acceptance of This Spec

This spec is accepted when:

1. A reader familiar with reth-bsc but unfamiliar with this incident can, from this document alone, (a) understand the distinction between "live fork" and "long-offline catch-up," and (b) identify the exact call site and routing rule that this design introduces.
2. A reviewer can verify, against the pinned reth checkout, each file:line citation in §"Why the Old FCU Path Works" and §"Why Pipeline Forward Is Viable Under TrieDB".
3. The regression-reproduction procedure in §"Testing / Acceptance" can be executed on a spare datadir and produces the stated "before" and "after" log patterns.
