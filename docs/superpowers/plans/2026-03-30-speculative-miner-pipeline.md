# Speculative Miner Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the production miner start building block `N+1` immediately after locally submitting block `N`, using an explicit durable MDBX base plus an in-memory overlay, then validate the result with `miner-bench`.

**Architecture:** Add a one-block speculative state tracker in the miner, extend `MiningContext` so logical parent state is decoupled from the durable MDBX base, and trigger speculative child work from `ResultWorkWorker` after successful local submission. Keep the import/persistence path authoritative, then mirror the same timing model in the benchmark harness so cached 1M and 10M runs measure the real production behavior instead of a benchmark-only shortcut.

**Tech Stack:** Rust, tokio `mpsc`, reth payload builder, revm `BundleState`, Parlia `Snapshot`, `miner-bench`

**Spec:** `docs/superpowers/specs/2026-03-30-pipelined-commit-design.md`

---

## File Structure

| File | Action | Responsibility |
|------|--------|----------------|
| `src/node/miner/speculative.rs` | Create | `PendingLocalHead`, `MiningContextSource`, and pure helper functions for speculative reconciliation |
| `src/node/miner/mod.rs` | Modify | Register the new speculative helper module |
| `src/node/miner/bsc_miner.rs` | Modify | Track pending speculative head, derive speculative child work, and reconcile canonical vs speculative contexts |
| `src/node/miner/payload.rs` | Modify | Carry `state_base_hash`, return enough finalized metadata to derive speculative snapshots, and open state from the durable base |
| `src/node/miner/bid_simulator.rs` | Modify | Keep build-argument construction consistent with the new `state_base_hash` plumbing |
| `src/node/evm/overlay.rs` | Modify | Add focused overlay tests for speculative reads |
| `src/bench/runner.rs` | Modify | Mirror the production one-block speculative timing model in `miner-bench` |
| `src/bench/report.rs` | Modify | Report speculative-handoff timing / fallback counters |

---

## Task 1: Speculative State Helper

**Files:**
- Create: `src/node/miner/speculative.rs`
- Modify: `src/node/miner/mod.rs`

This task isolates the speculative-head state machine from `bsc_miner.rs` so the worker code only
orchestrates decisions instead of embedding reconciliation rules inline.

- [ ] **Step 1: Write the failing tests for one-block speculative state**

Add tests in `src/node/miner/speculative.rs` for:

```rust
#[test]
fn pending_local_head_allows_only_one_speculative_child() {
    let mut tracker = PendingLocalHead::default();
    assert!(tracker.record_submitted_head(example_pending_head(100, 99)).is_none());
    assert!(tracker.can_spawn_child(101));
    assert!(!tracker.can_spawn_child(102));
}

#[test]
fn canonical_mismatch_clears_speculative_state() {
    let mut tracker = PendingLocalHead::from(example_pending_head(100, 99));
    let decision = tracker.reconcile_canonical_head(example_hash(200), 100);
    assert_eq!(decision, ReconcileDecision::ClearPending);
    assert!(tracker.current().is_none());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test --features bench-test speculative::tests:: -- --nocapture
```

Expected: FAIL because `PendingLocalHead`, `ReconcileDecision`, and helper constructors do not
exist yet.

- [ ] **Step 3: Implement the helper module with the smallest API the workers need**

Start with a focused surface:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MiningContextSource {
    Canonical,
    Speculative,
}

#[derive(Debug, Clone)]
pub struct PendingLocalHead {
    pub block_number: u64,
    pub block_hash: B256,
    pub parent_hash: B256,
    pub durable_base_hash: B256,
    pub child_spawned: bool,
}

pub enum ReconcileDecision {
    KeepPending,
    ClearPending,
}
```

Keep it pure: no provider calls, no channels, no worker references.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test --features bench-test speculative::tests:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/node/miner/speculative.rs src/node/miner/mod.rs
git commit -m "feat(miner): add speculative head state helper"
```

---

## Task 2: Explicit Durable Base + Overlay Read Semantics

**Files:**
- Modify: `src/node/miner/payload.rs`
- Modify: `src/node/miner/bid_simulator.rs`
- Modify: `src/node/evm/overlay.rs`
- Modify: `src/node/miner/bsc_miner.rs`

The approved design requires the payload builder to read from an explicit durable base hash rather
than inferring MDBX state from the speculative parent hash.

- [ ] **Step 1: Write failing tests for overlay precedence and explicit base selection**

Add focused tests in `src/node/evm/overlay.rs`:

```rust
#[test]
fn overlay_returns_present_storage_before_inner_db() {
    let overlay = BundleStateOverlay::new(bundle_with_slot(U256::from(7), U256::from(9)), fake_db());
    assert_eq!(overlay.storage_ref(example_address(), U256::from(7)).unwrap(), U256::from(9));
}

#[test]
fn destroyed_account_storage_falls_back_to_zero_not_inner_db() {
    let overlay = BundleStateOverlay::new(destroyed_bundle(), fake_db_with_slot(U256::from(7), U256::from(99)));
    assert_eq!(overlay.storage_ref(example_address(), U256::from(7)).unwrap(), U256::ZERO);
}
```

Add a payload-side helper test in `src/node/miner/payload.rs`:

```rust
#[test]
fn speculative_build_uses_state_base_hash_not_parent_hash() {
    let args = example_build_args().with_state_base_hash(example_hash(99));
    assert_eq!(resolve_state_provider_hash(&args, example_hash(100)), example_hash(99));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test --features bench-test overlay_returns_present_storage_before_inner_db -- --nocapture
cargo test --features bench-test speculative_build_uses_state_base_hash_not_parent_hash -- --nocapture
```

Expected: FAIL because the helper does not exist yet and the overlay tests should expose missing
coverage.

- [ ] **Step 3: Plumb `state_base_hash` through the build arguments**

Modify:

- `MiningContext` in `src/node/miner/bsc_miner.rs`
- `BscBuildArguments` in `src/node/miner/payload.rs`
- `build_payload()` and `build_empty_payload()` in `src/node/miner/payload.rs`
- `BscBuildArguments` construction in `src/node/miner/bid_simulator.rs`

Add a small helper in `payload.rs` so the read-base rule is testable:

```rust
fn resolve_state_provider_hash(
    args: &BscBuildArguments<EthPayloadBuilderAttributes>,
    parent_hash: B256,
) -> B256 {
    args.state_base_hash.unwrap_or(parent_hash)
}
```

Then open the state provider from `resolve_state_provider_hash(...)` instead of `parent_hash`.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test --features bench-test overlay_returns_present_storage_before_inner_db -- --nocapture
cargo test --features bench-test destroyed_account_storage_falls_back_to_zero_not_inner_db -- --nocapture
cargo test --features bench-test speculative_build_uses_state_base_hash_not_parent_hash -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/node/miner/payload.rs src/node/miner/bid_simulator.rs src/node/evm/overlay.rs src/node/miner/bsc_miner.rs
git commit -m "feat(miner): use explicit durable base for speculative payload builds"
```

---

## Task 3: Derive The Speculative Child Context After Local Submission

**Files:**
- Modify: `src/node/miner/speculative.rs`
- Modify: `src/node/miner/bsc_miner.rs`
- Modify: `src/node/miner/payload.rs`

This task creates the real overlap. The key complication is that `finalize_payload()` currently
consumes validator and turn-length deltas that are also needed to derive the speculative next
snapshot.

- [ ] **Step 1: Write the failing tests for child-context derivation**

Extend `src/node/miner/speculative.rs` with tests that cover:

```rust
#[test]
fn derive_speculative_child_context_keeps_durable_base_on_parent() {
    let ctx = derive_speculative_child_context(example_submit_ctx(100, 99)).unwrap();
    assert_eq!(ctx.parent_header.number(), 100);
    assert_eq!(ctx.state_base_hash, Some(example_hash(99)));
    assert_eq!(ctx.source, MiningContextSource::Speculative);
}

#[test]
fn reconcile_confirmed_canonical_head_preserves_matching_pending_state() {
    let mut tracker = PendingLocalHead::from(example_pending_head(100, 99));
    let decision = tracker.reconcile_canonical_head(example_hash(100), 100);
    assert_eq!(decision, ReconcileDecision::KeepPending);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test --features bench-test derive_speculative_child_context_keeps_durable_base_on_parent -- --nocapture
cargo test --features bench-test reconcile_confirmed_canonical_head_preserves_matching_pending_state -- --nocapture
```

Expected: FAIL because the child-context derivation function does not exist yet.

- [ ] **Step 3: Refactor finalization so snapshot derivation still has access to execution deltas**

Modify `finalize_payload()` in `src/node/miner/payload.rs` to return a small artifact struct:

```rust
pub struct FinalizedPayloadArtifacts {
    pub finalized_hash: B256,
    pub pending_validators: Option<(Vec<Address>, Vec<VoteAddress>)>,
    pub pending_turn_length: Option<u8>,
}
```

Use that artifact plus `SubmitContext.mining_ctx.parent_snapshot` to derive the speculative parent
snapshot before the values are dropped on the floor.

- [ ] **Step 4: Implement speculative triggering in `ResultWorkWorker`**

Update `ResultWorkWorker` so that after a successful local send to the import service it:

1. records `PendingLocalHead`
2. derives a speculative child `MiningContext`
3. sends that context to `MainWorkWorker`

Do not trigger speculation on send failure.

- [ ] **Step 5: Run the tests to verify they pass**

Run:
```bash
cargo test --features bench-test speculative::tests:: -- --nocapture
cargo test --features bench-test payload::tests:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/node/miner/speculative.rs src/node/miner/bsc_miner.rs src/node/miner/payload.rs
git commit -m "feat(miner): trigger speculative child builds after local submission"
```

---

## Task 4: Canonical Reconciliation And Abort Rules

**Files:**
- Modify: `src/node/miner/bsc_miner.rs`
- Modify: `src/node/miner/speculative.rs`

The speculative path is only safe if canonical notifications can cancel or supersede it cleanly.

- [ ] **Step 1: Write the failing tests for canonical precedence**

Add tests that cover:

```rust
#[test]
fn canonical_context_wins_over_stale_speculative_context() {
    let decision = choose_next_context(example_canonical_ctx(100), Some(example_speculative_ctx(100)));
    assert_eq!(decision, ContextDecision::UseCanonical);
}

#[test]
fn mismatched_canonical_tip_clears_pending_head_and_aborts_child() {
    let mut tracker = PendingLocalHead::from(example_pending_head(100, 99));
    assert_eq!(
        on_canonical_tip(&mut tracker, example_hash(500), 100),
        ContextDecision::ClearAndAbortSpeculative
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test --features bench-test canonical_context_wins_over_stale_speculative_context -- --nocapture
cargo test --features bench-test mismatched_canonical_tip_clears_pending_head_and_aborts_child -- --nocapture
```

Expected: FAIL because the precedence helpers and abort logic do not exist yet.

- [ ] **Step 3: Wire the worker reconciliation path**

In `src/node/miner/bsc_miner.rs`:

- clear pending speculative state on canonical mismatch
- allow canonical mining contexts to supersede speculative ones
- keep speculative work only when the confirmed canonical head matches the pending local head

Make the worker code call the pure reconciliation helpers from `speculative.rs` instead of
duplicating the rules inline.

- [ ] **Step 4: Run the tests to verify they pass**

Run:
```bash
cargo test --features bench-test speculative::tests:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/node/miner/bsc_miner.rs src/node/miner/speculative.rs
git commit -m "feat(miner): reconcile speculative builds with canonical head updates"
```

---

## Task 5: Align `miner-bench` With The Production Speculative Path

**Files:**
- Modify: `src/bench/runner.rs`
- Modify: `src/bench/report.rs`

The benchmark is the performance detector, so it must follow the same one-block speculative timing
model as production rather than only reusing overlay state.

- [ ] **Step 1: Write the failing benchmark helper tests**

Add/extend tests in `src/bench/runner.rs`:

```rust
#[test]
fn bench_pipeline_keeps_durable_base_one_commit_behind_speculative_parent() {
    let mut state = BenchSpeculativeState::new(example_hash(99));
    state.on_submitted_parent(example_hash(100));
    assert_eq!(state.state_base_hash(), example_hash(99));
}

#[test]
fn bench_pipeline_advances_durable_base_after_commit_result() {
    let mut state = BenchSpeculativeState::new(example_hash(99));
    state.on_commit_finished(example_hash(100));
    assert_eq!(state.state_base_hash(), example_hash(100));
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run:
```bash
cargo test --features bench-test bench::runner::tests:: -- --nocapture
```

Expected: FAIL because the benchmark helper/state machine does not exist yet.

- [ ] **Step 3: Update the benchmark loop to match the production speculative model**

Modify `src/bench/runner.rs` so each build attempt tracks:

- logical parent hash
- durable base hash
- one pending speculative head
- commit completion that advances the durable base

Add report fields in `src/bench/report.rs` for speculative handoff timing and drop/fallback counts.

- [ ] **Step 4: Run the benchmark tests to verify they pass**

Run:
```bash
cargo test --features bench-test bench::runner::tests:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/bench/runner.rs src/bench/report.rs
git commit -m "feat(bench): mirror speculative miner timing in benchmark harness"
```

---

## Task 6: Verification And Performance Validation

**Files:** None required unless a verification-driven fix is needed.

- [ ] **Step 1: Run focused Rust tests**

Run:
```bash
cargo test --features bench-test speculative::tests:: -- --nocapture
cargo test --features bench-test payload::tests:: -- --nocapture
cargo test --features bench-test bench::runner::tests:: -- --nocapture
```

Expected: PASS.

- [ ] **Step 2: Run a cached 1M-account benchmark smoke**

Run:
```bash
cargo run --features bench-test --bin miner-bench -- run \
  --genesis /Users/user/.config/superpowers/worktrees/reth-bsc/bench-tmp/genesis_local_gas500m.json \
  --num-blocks 100 \
  --txs-per-block 6000 \
  --funded-accounts 5000 \
  --background-accounts 1000000 \
  --storage-slots-per-account 1 \
  --triedb \
  --cache-dir /tmp/reth-bsc-bench-cache-1m \
  --reuse-genesis-db \
  --reuse-post-setup-db \
  --output /tmp/reth-bsc-speculative-1m.csv
```

Expected: completes successfully and shows whether speculative overlap moves steady-state `total`
in the right direction before paying the 10M runtime.

- [ ] **Step 3: Compare the 1M run against the current compare branch**

Run:
```bash
cargo run --features bench-test --bin miner-bench -- compare \
  --baseline /tmp/reth-bsc-current-1m.csv \
  --optimized /tmp/reth-bsc-speculative-1m.csv
```

Expected: `total` improves without a regression large enough to erase the overlap win.

- [ ] **Step 4: Run the cached 10M-account benchmark**

Run:
```bash
cargo run --features bench-test --bin miner-bench -- run \
  --genesis /Users/user/.config/superpowers/worktrees/reth-bsc/bench-tmp/genesis_local_gas500m.json \
  --num-blocks 100 \
  --txs-per-block 6000 \
  --funded-accounts 5000 \
  --background-accounts 10000000 \
  --storage-slots-per-account 1 \
  --triedb \
  --cache-dir /tmp/reth-bsc-bench-cache-10m \
  --reuse-genesis-db \
  --reuse-post-setup-db \
  --output /tmp/reth-bsc-speculative-10m.csv
```

Expected: completes successfully using the cached setup path.

- [ ] **Step 5: Compare the 10M run against the provided baseline**

Run:
```bash
cargo run --features bench-test --bin miner-bench -- compare \
  --baseline /Users/user/.config/superpowers/worktrees/reth-bsc/bench-results/bench_2000tps_5m_g500m_bg10m_slot1.csv \
  --optimized /tmp/reth-bsc-speculative-10m.csv
```

Expected: steady-state `finish`, `commit`, and especially `total` show whether the production
pipeline overlap reaches the target range.

- [ ] **Step 6: Commit the final code changes**

```bash
git add src/node/miner/speculative.rs src/node/miner/bsc_miner.rs src/node/miner/payload.rs src/node/miner/bid_simulator.rs src/node/evm/overlay.rs src/bench/runner.rs src/bench/report.rs docs/superpowers/plans/2026-03-30-speculative-miner-pipeline.md
git commit -m "feat(miner): pipeline speculative next-block builds"
```

---

## Summary

| Task | What | Main Risk |
|------|------|-----------|
| 1 | Add speculative head helper | Wrong reconciliation API shape |
| 2 | Plumb explicit durable base into payload builds | Reading MDBX from the wrong parent hash |
| 3 | Trigger speculative child work after local submission | Losing validator/turn-length data needed for snapshot derivation |
| 4 | Reconcile canonical and speculative contexts | Stale speculative state surviving a reorg |
| 5 | Align `miner-bench` with production timing | Benchmark no longer matching the real miner path |
| 6 | Verify and benchmark | Runtime too long without using the cached setup path |
