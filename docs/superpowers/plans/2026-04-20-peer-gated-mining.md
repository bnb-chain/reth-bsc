# Peer-Gated Mining Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Refuse to produce blocks while the miner has zero connected peers, so an isolated validator cannot solo-mine a private fork that later desynchronises with the rest of the network.

**Architecture:** One file touched (`src/node/miner/bsc_miner.rs`). A new free function `is_network_ready_to_mine(tip_number)` centralises three gate checks (missing network handle, zero peers, backfill active). The existing 5-second `SYNC_GATE_TIMEOUT_SECS` bypass is deleted — without peers we never proceed, regardless of how long we've waited. Unit tests are intentionally omitted (see design doc `docs/superpowers/specs/2026-04-20-peer-gated-mining-design.md` → "Testing Strategy"); validation is via `cargo check` + `cargo clippy` and three manual integration scenarios on qanet.

**Tech Stack:** Rust, reth / reth-bsc, `NetworkInfo` + `SyncStateProvider` traits from reth-network, `tracing`.

**Related docs**
- `docs/superpowers/specs/2026-04-20-peer-gated-mining-design.md` — design decisions, scope, acceptance criteria.
- `docs/superpowers/specs/2026-04-18-pathdb-gap-fork-livelock-scenario.md` — problem context, preconditions P1–P3, reproduction recipe.

---

## File Structure

| File | Kind | Responsibility |
|------|------|----------------|
| `src/node/miner/bsc_miner.rs` | Modify | Add `is_network_ready_to_mine`; swap `try_new_work` call site; delete `SYNC_GATE_TIMEOUT_SECS` + `SYNC_GATE_FIRST_HIT`; prune `OnceLock` import |

No files created, no other files modified.

---

## Task 1: Replace sync-gate timeout with peer gate

**Files:**
- Modify: `src/node/miner/bsc_miner.rs` (imports lines 43–45, constants lines 51–62, `try_new_work` body lines ~438–463)

- [ ] **Step 1: Read the current state of the affected regions**

Run to orient:

```bash
sed -n '40,65p' src/node/miner/bsc_miner.rs
sed -n '428,465p' src/node/miner/bsc_miner.rs
```

Expected: lines 58–62 are the `SYNC_GATE_TIMEOUT_SECS` const + `SYNC_GATE_FIRST_HIT` static; lines 438–463 inside `try_new_work` are the `is_syncing` + timeout bypass block.

- [ ] **Step 2: Delete the `SYNC_GATE_TIMEOUT_SECS` and `SYNC_GATE_FIRST_HIT` items**

Remove these two items with their doc comments (lines 54–62 inclusive, plus the blank line between them):

```rust
/// After this many seconds of `is_syncing() == true` with no canonical events, allow mining
/// anyway. This breaks the deadlock that occurs when all validators restart simultaneously:
/// no one produces blocks → no FCU → is_syncing never clears → no mining → deadlock.
/// 5s ≈ 11 Fermi slots (450 ms each), enough time for a peer to send FCU if any are running.
const SYNC_GATE_TIMEOUT_SECS: u64 = 5;

/// Tracks when the miner first encountered the sync gate. Used for timeout-based deadlock
/// recovery when all validators restart simultaneously.
static SYNC_GATE_FIRST_HIT: OnceLock<Instant> = OnceLock::new();
```

After this step, `OnceLock` will no longer be referenced anywhere in this file, and `Instant` will only appear as the fully-qualified `std::time::Instant::now()` at line 744. Both the `OnceLock` and `Instant` imports must be pruned — handled in Step 3.

- [ ] **Step 3: Prune the now-unused imports**

Two one-line changes on lines 44 and 45.

Line 44 — remove `OnceLock`:

```rust
// before
use std::sync::{Arc, Mutex, OnceLock};
// after
use std::sync::{Arc, Mutex};
```

Line 45 — remove `Instant` (leaving `Duration`, which is still used unqualified at lines ~158 and ~1124–1125 for `Duration::from_secs` / `Duration::from_millis`):

```rust
// before
use std::time::{Duration, Instant};
// after
use std::time::Duration;
```

The remaining `Instant` usage in this file is `std::time::Instant::now()` at line ~744, which is fully qualified and does not need the import.

- [ ] **Step 4: Add the new `is_network_ready_to_mine` function**

Insert this function in `src/node/miner/bsc_miner.rs` immediately above `try_new_work` (i.e., just before the `async fn try_new_work<H>` signature). The exact code:

```rust
/// Returns `true` when network conditions allow block production.
///
/// Two gates prevent mining. If either fires, this returns `false` and
/// emits a `DEBUG` log naming the specific gate so a stuck validator
/// can be diagnosed from logs alone.
///
/// 1. **No connected peers.** Mining while alone produces a fork chain
///    that the rest of the network cannot accept back after reconnect:
///    the peer's pathdb disk layer is pinned at its own tip with no
///    diff layers retained, so it cannot execute blocks built on an
///    older common ancestor. See
///    `docs/superpowers/specs/2026-04-18-pathdb-gap-fork-livelock-scenario.md`
///    for the full scenario analysis.
///
/// 2. **Node is in backfill (`is_syncing`).** Local state is not yet
///    aligned with the network tip; mining here would also create a
///    fork, just a less dramatic one.
///
/// Intentional limitation: a fresh-genesis bootstrap where no peers
/// exist anywhere yet will skip mining forever. Bootstrapping a
/// brand-new network with this code is **not supported today**;
/// revisit if/when an explicit bootstrap mode is added.
///
/// Also returns `false` (skip) when the network handle is not yet
/// installed. That window exists briefly at startup; skipping during
/// it is safer than defaulting to "allow mining" when we cannot even
/// check peer count.
fn is_network_ready_to_mine(tip_number: u64) -> bool {
    let Some(network) = crate::shared::get_network_handle() else {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining: network handle not yet available"
        );
        return false;
    };

    use reth_network::NetworkInfo;
    if network.num_connected_peers() == 0 {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining: no peers connected"
        );
        return false;
    }

    use reth_network_p2p::sync::SyncStateProvider;
    if network.is_syncing() {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining: node is syncing (backfill active)"
        );
        return false;
    }

    true
}
```

- [ ] **Step 5: Swap the call site inside `try_new_work`**

In `try_new_work`, replace the entire `is_syncing` + timeout block (lines 438–463 — starts at the `// Gate mining on live sync:` comment and ends with the closing `}` of the `if let Some(network)` block) with this single guard:

```rust
if !is_network_ready_to_mine(tip.number()) {
    return;
}
```

The surrounding context should read:

```rust
async fn try_new_work<H>(&self, tip: &SealedHeader<H>)
where
    H: alloy_consensus::BlockHeader + Sealable,
{
    // Check if mining is disabled via miner_stop RPC
    if !crate::shared::is_mining_enabled() {
        debug!("Skip mining: mining is disabled via miner_stop RPC");
        return;
    }

    if !is_network_ready_to_mine(tip.number()) {
        return;
    }

    let parent_header = match self.provider.sealed_header_by_hash(tip.hash()) {
        Ok(Some(header)) => {
            // ...existing code unchanged...
```

- [ ] **Step 6: Build-check the file compiles**

Run:

```bash
cargo check -p reth_bsc
```

Expected: compiles with zero errors. There should be **zero** `unused import` warnings for `OnceLock`, `Instant`, or `Duration`. If any warning appears for this file, go back and inspect — the import pruning in Step 3 is wrong or incomplete.

- [ ] **Step 7: Run clippy**

Run:

```bash
cargo clippy -p reth_bsc -- -D warnings
```

Expected: no clippy errors in `src/node/miner/bsc_miner.rs`. Acceptable to have pre-existing warnings in *other* files — only new warnings introduced by this patch must be addressed.

- [ ] **Step 8: Commit**

```bash
git add src/node/miner/bsc_miner.rs
git commit -m "$(cat <<'EOF'
feat(miner): gate block production on peer connectivity

Replace the 5s sync-gate timeout bypass with a strict "at least one
connected peer" precondition for mining. Without this guard, a
validator restarted in isolation (or during the pre-handshake window
after a coordinated restart) can solo-mine a private fork chain that
the rest of the network later cannot reconcile — see
docs/superpowers/specs/2026-04-18-pathdb-gap-fork-livelock-scenario.md.

Introduces `is_network_ready_to_mine()` with inline documentation of
the known fresh-genesis-bootstrap limitation. Design rationale in
docs/superpowers/specs/2026-04-20-peer-gated-mining-design.md.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Smoke test on qanet (peer-gate + recovery)

**Prerequisites:** Two qanet validator machines reachable from each other, mining keys configured, both configured to peer each other via `--trusted-peers`. Running the `start_validator.sh` equivalent from `/server/reth-env/` (or your local analogue).

- [ ] **Step 1: Build release binary on both validator machines**

```bash
cargo build --release --bin reth-bsc
```

Deploy the new binary to both machines. Keep the previous binary as `reth-bsc.old` for quick rollback.

- [ ] **Step 2: Verify the skip-log fires at startup (single machine)**

On validator A (the one that starts first), tail logs for at least 30 seconds after process start:

```bash
grep -E 'Skip mining|no peers connected|network handle not yet available' logs/reth.log | head -20
```

Expected: at least one line matching `Skip mining: network handle not yet available` (during the very first moments) **or** `Skip mining: no peers connected` (between handle init and first peer handshake). The skip lines should stop once a peer is connected — confirm by checking `admin_peers` returns a non-empty list and subsequent mining attempts proceed (`Try off-turn mining` / `Try in-turn mining` appear).

- [ ] **Step 3: Confirm chain progression resumes after peer handshake**

Poll over RPC every 10 seconds for ~1 minute:

```bash
for i in $(seq 1 6); do
  curl -s -X POST -H "Content-Type: application/json" \
    --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
    http://127.0.0.1:8545 | jq -r '.result'
  sleep 10
done
```

Expected: block number increases across samples (Parlia block interval ≈ 450 ms, so even a slow network will advance multiple blocks per 10 s window once both validators are connected).

---

## Task 3: T0→T5 regression — the livelock must not re-emerge

Reproduces the exact sequence from `docs/superpowers/specs/2026-04-18-pathdb-gap-fork-livelock-scenario.md` → "Reproduction Recipe", and confirms the pre-patch failure path is now blocked.

- [ ] **Step 1: Bring both validators online at a shared tip H₀**

Start both, let them exchange a few blocks in normal rotation, then record the current head height from either node's `eth_blockNumber` — call this `H₀`.

- [ ] **Step 2: Stop node_B cleanly**

On validator B:

```bash
kill -TERM $(pgrep -f reth-bsc)
# wait for the process to exit; confirm pathdb flush completed
grep 'flush' logs/reth.log | tail -3
```

- [ ] **Step 3: Observe node_A for at least 3 minutes**

Poll `eth_blockNumber` every 15 seconds over 3 minutes.

**Expected with this patch applied**:
- `eth_blockNumber` remains at exactly `H₀` for the entire window.
- Logs show repeating `DEBUG bsc::miner: Skip mining: no peers connected` at roughly the mining tick rate.
- **No** `Try off-turn mining` / `Try in-turn mining` lines in that window.
- **No** `Sync gate timeout reached` warning (the log message was deleted — its absence is proof the bypass was removed).

**Contrast with pre-patch behaviour** (already observed in the incident logs `start-3.log.1`/`.log.2`): node_A would produce 10 blocks within about 5 seconds and log `Sync gate timeout reached, allowing mining to break potential all-validators-restart deadlock`.

- [ ] **Step 4: Restart node_B and confirm normal operation resumes**

Start node_B again. Within ~15 seconds of peer handshake completing:
- `Skip mining: no peers connected` stops appearing on node_A.
- Both validators begin producing blocks in rotation.
- `eth_blockNumber` on both nodes begins advancing in lock-step.

- [ ] **Step 5: Record the run**

Save the node_A log covering Steps 2–4 for reference. Name it e.g. `regression-T0-T5-<date>.log` and reference it in the commit or PR description.

---

## Task 4: Negative-control — fresh-genesis single-validator stays at genesis

Pins the **known** limitation so future maintainers recognise it as documented behaviour, not a regression.

- [ ] **Step 1: Wipe the datadir on a single validator**

```bash
# On the single test machine
kill -TERM $(pgrep -f reth-bsc)  # if running
rm -rf /server/reth-env/data_dir/db \
       /server/reth-env/data_dir/rust_eth_triedb \
       /server/reth-env/data_dir/static_files
# keep geth/nodekey, keystore/, bls/ — identity files stay
```

- [ ] **Step 2: Start that single validator with NO peers reachable**

Comment out or omit the `--trusted-peers` flag and ensure no discovery bootnode is reachable. Start the node.

- [ ] **Step 3: Verify it stays at genesis for ≥3 minutes**

```bash
curl -s -X POST -H "Content-Type: application/json" \
  --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  http://127.0.0.1:8545 | jq -r '.result'
# Expected: "0x0"
```

Observe logs: `Skip mining: no peers connected` should be the dominant miner-related line, repeating at the mining tick rate.

- [ ] **Step 4: Document the outcome**

This is **expected**, not a bug. Add a short note in the PR description:

> Negative control: fresh-genesis single-validator stays at genesis forever with `Skip mining: no peers connected`. This is the documented limitation from `2026-04-20-peer-gated-mining-design.md` ("Non-Goals"). Bootstrap support is deferred.

---

## Post-merge Monitoring (optional, informational)

For validators running in production / staging after this change merges, watch the existing alerting dashboards for the new signal:

- `Skip mining: no peers connected` (DEBUG, target `bsc::miner`): expected briefly at startup (seconds). If persistent ≥60 seconds on a validator that should have peers, that validator is isolated — treat as a paging-worthy peer-connectivity incident, not a miner bug.

No new metric is introduced; use log-based alerts on the message string if needed.

---

## Acceptance Summary

Implementation of this plan is complete when:

1. `cargo check -p reth_bsc` and `cargo clippy -p reth_bsc -- -D warnings` both succeed (Task 1, Steps 6 & 7).
2. Task 2 smoke test shows the skip log appears at startup and disappears once peers connect.
3. Task 3 T0→T5 regression passes: node_A stays at `H₀` with `Skip mining: no peers connected` while alone, and resumes normal rotation after node_B reconnects.
4. Task 4 negative control confirms the documented fresh-genesis limitation is reachable and logged clearly.
5. One commit on the branch implementing the change, with a descriptive message referencing both design docs.
