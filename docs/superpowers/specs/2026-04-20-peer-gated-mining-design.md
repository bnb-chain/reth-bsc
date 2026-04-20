# Peer-Gated Mining: Design

**Status**: Approved in brainstorming session 2026-04-20. Implementation plan to follow.
**Scope**: Mitigation for the pathdb-gap fork livelock. Does not fix the underlying state-availability limitation.

## Related

- **Problem definition**: `2026-04-18-pathdb-gap-fork-livelock-scenario.md` — full scenario with preconditions P1–P3, triggering sequence T0–T6, and variations V1–V6.
- **Periodic head announce**: `2026-04-17-fix-p2p-livelock-design.md` — a different but adjacent livelock fix.

## Background

The scenario spec identifies three preconditions that must all hold for the livelock:

- **P1** — two nodes hold divergent canonical chains above a shared ancestor H₀.
- **P2** — one node's pathdb cannot serve state for H₀ (no diff layers, disk layer pinned at its own tip).
- **P3** — Parlia recent-signer rule saturates on both validators.

P1 is the direct cause of divergence. Its dominant real-world trigger is documented as **T2 (Early-mining bypass)** in the scenario spec: `src/node/miner/bsc_miner.rs` currently allows off-turn solo mining after a `SYNC_GATE_TIMEOUT_SECS = 5` second grace, even with zero peers connected. An all-validators-restart window of a few seconds is therefore sufficient for one validator to produce several blocks alone before the peer handshake completes, creating the divergence that P2 later renders permanent.

## Goal

**Primary purpose**: prevent a validator from producing blocks while it is network-isolated. Solo-mining during a network partition or a peer-handshake window creates a private fork chain that must be reconciled when connectivity is restored — each such reconciliation risks triggering the pathdb-gap livelock (scenario spec P1+P2+P3), and even when it does not trigger the livelock it produces avoidable work for the rest of the network.

The mechanism: refuse to mine while the node has no connected peers. In scenario-spec terms this eliminates T2 (early-mining bypass) as a divergence source, which in turn weakens P1 by removing its most common real-world trigger.

## Non-Goals

- **Does not fix P2.** pathdb still has no journal and no reverse-diff freezer. If divergence arises through any other mechanism (crash, network partition, operator error, etc.), the livelock can still occur. A proper fix for P2 is deferred to a future spec.
- **Does not address V1 (N > 2 generalization), V3 (symmetric pathdb gap), V4 (crash recovery), V5 (fork deeper than `MAX_FORK_DEPTH`), or V6 (pathdb flush mid-import).** These all require state-layer work.
- **Does not support fresh-genesis bootstrap** with a single validator. Making it work requires a dedicated bootstrap mode (CLI flag or env var); that is explicitly deferred.
- **Does not tighten `fork_recover.rs:292`** Phase-2 first-block Syncing semantics. That is a separate, independent improvement.
- **Does not add new metrics or persistent state.**
- **Does not gate mining on `NetworkHandle::is_syncing()`.** An earlier draft of this design included that gate, but it caused a cold-start deadlock in `N = 2` qanet where both validators restarted with aligned backends (no backfill target). `is_syncing` is set to `Syncing` at process start (`reth/crates/node/builder/src/launch/engine.rs:165`) and only clears to `Idle` on a `CanonicalChainCommitted` event (`reth/crates/engine/primitives/src/event.rs:40`) — i.e., after a block is committed. With the peer gate also blocking mining, neither validator could ever produce the first block that would clear the flag, and the network stayed frozen at the restart tip indefinitely. The trade-off of removing this gate: during a *real* backfill window the miner may briefly produce blocks on a stale tip; peers will reject those as out-of-date, so the observed cost is wasted work, not a correctness defect. If a future state-layer fix (e.g., pathdb journal) re-opens this question, the gate can be reintroduced behind a fresher signal such as "local tip within K blocks of the best peer head".

## Design Principles

1. **Prefer "do nothing" over "do the wrong thing".** Without peers, a validator cannot tell whether it is alone because peers haven't connected yet or because it's on a reachable partition. The safe default is to not mine; any mined block during that window has a high probability of becoming fork-chain waste.
2. **No new knobs.** One fewer timeout, one fewer env var. The existing operational surface (peer visibility via `admin_peers`, miner on/off via `miner_stop` RPC) is sufficient.
3. **Document the one unsupported case** in the code, not just in the spec. A future reader of `bsc_miner.rs` should learn about fresh-genesis bootstrap from the guard function's doc comment.

## Architecture

A single file change, confined to `src/node/miner/bsc_miner.rs`.

### New function: `is_network_ready_to_mine`

Introduce a free function in the same module that returns `true` when mining is safe to proceed. The function checks two conditions in order and emits a targeted DEBUG log on each skip path, so a stuck validator can be diagnosed from logs alone.

```rust
/// Returns `true` when network conditions allow block production.
///
/// One semantic gate plus a startup-safety skip. Each early-return emits
/// a `DEBUG` log naming the specific path so a stuck validator can be
/// diagnosed from logs alone.
///
/// **No connected peers.** Mining while alone produces a fork chain
/// that the rest of the network cannot accept back after reconnect:
/// the remote peer's pathdb disk layer is pinned at its own tip with no
/// diff layers retained, so it cannot execute blocks built on an older
/// common ancestor. See
/// `docs/superpowers/specs/2026-04-18-pathdb-gap-fork-livelock-scenario.md`
/// for the full scenario analysis.
///
/// Intentional limitations:
///
/// - A fresh-genesis bootstrap where no peers exist anywhere yet will
///   skip mining forever. Not supported today.
/// - This function intentionally does not gate on `is_syncing()`; the
///   full rationale lives in the Non-Goals section above. In short:
///   that flag never clears on a cold-start with aligned backends,
///   and gating on it produced a circular deadlock with the peer
///   gate. The trade-off is that during a real backfill window the
///   miner may briefly produce stale blocks that peers reject.
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

    use reth_network::PeersInfo;
    if network.num_connected_peers() == 0 {
        debug!(
            target: "bsc::miner",
            tip_number,
            "Skip mining: no peers connected"
        );
        return false;
    }

    true
}
```

### Call site: `try_new_work`

Replace the current 26-line sync-gate block (`src/node/miner/bsc_miner.rs:438-463`) with a single call:

```rust
// Existing check — unchanged.
if !crate::shared::is_mining_enabled() {
    debug!("Skip mining: mining is disabled via miner_stop RPC");
    return;
}

// New: peer guard.
if !is_network_ready_to_mine(tip.number()) {
    return;
}

// Continuing with existing parent_header lookup...
```

### Removals

The following items become unused after the change and must be deleted:

- `const SYNC_GATE_TIMEOUT_SECS: u64 = 5;` (`bsc_miner.rs:58`)
- `static SYNC_GATE_FIRST_HIT: OnceLock<Instant> = OnceLock::new();` (`bsc_miner.rs:62`)
- The `WARN "Sync gate timeout reached, allowing mining to break potential all-validators-restart deadlock"` emission (formerly `bsc_miner.rs:456-461`).
- Any now-unused `use` statements for `OnceLock`, `Instant`, `Duration` in this file — verify at implementation time.

### Behaviour matrix

| Network handle | Peer count | Result |
|----------------|------------|--------|
| None (startup) | —          | Skip (DEBUG: "network handle not yet available") |
| Some           | 0          | Skip (DEBUG: "no peers connected") |
| Some           | ≥1         | **Proceed with mining** |

`is_syncing()` is no longer consulted; see Non-Goals.

Rows 1 and 2 are skip paths; row 3 is the fast path. Previously the call site also had an `is_syncing()` gate with a 5-second `SYNC_GATE_TIMEOUT_SECS` self-override that emitted a WARN and proceeded; both the gate and the override are gone.

## Testing Strategy

### Unit tests

**Skipped by design.** Reasons documented in the brainstorming session:

- The function depends on `crate::shared::get_network_handle()`, a global `OnceLock`. Mocking it would require either a trait-injection refactor across `crate::shared` or a test-only override mechanism — both disproportionate to what the function does.
- The function has no internal control-flow complexity; each branch is a single equality check feeding a single `return false`.
- The currently-deleted `SYNC_GATE_*` constants have no existing tests depending on them.

A future change that modifies the gate conditions in a subtle way should reopen this decision.

### Integration tests (required)

1. **Smoke**: start a two-node devnet from a prepared shared tip; confirm both nodes resume producing blocks after peer handshake completes. Verify "Skip mining: no peers connected" appears briefly at startup and disappears after handshake.

2. **Scenario-spec T0→T5 regression (hardest test)**. Directly reproduces the problem this change addresses:
   - Setup: two validators aligned at a shared tip H₀ on qanet.
   - Stop node_B. Let node_A run alone.
   - **Before this change**: node_A waits 5 seconds, then solo-mines several blocks. Divergence established.
   - **After this change**: node_A stays at H₀ indefinitely, emitting "Skip mining: no peers connected" every mining tick. No divergence.
   - Restart node_B; both reconnect; normal rotation resumes.

3. **Negative control (documented limitation)**: a single validator on a fresh genesis. Expected behaviour: stays at genesis forever, emits "Skip mining: no peers connected" every tick. Purpose: pin the known limitation so no future maintainer mistakes it for a regression.

### Manual review checklist

- Deletion of `SYNC_GATE_*` items does not leave dangling imports.
- Log target `bsc::miner` is preserved on all skip messages.
- `tip_number` field is present on every DEBUG log for easy grep'ing.

## Rollout & Monitoring

### Log signals to watch

| Log message | Meaning |
|-------------|---------|
| `Skip mining: no peers connected` (DEBUG, `bsc::miner`) | Miner is gated on peer count. Expected briefly at startup; persistent ≥60 s indicates the node is isolated. |
| `Skip mining: network handle not yet available` (DEBUG, `bsc::miner`) | Startup-transient. Should disappear within seconds of node start. |

### Alerting

No new metrics introduced. Operators should extend existing "stuck validator" alerts to include the new skip signal if and only if it persists beyond a threshold. The scenario spec already documents the observable-signal fingerprint for the full livelock; this change does not alter that fingerprint — livelocks that existed before this code shipped will still look the same after.

## Rollback

- Single commit change. No persistence format, no IPC contract, no DB schema, no CLI flags introduced.
- `git revert <commit>` is the full rollback. No migration required.
- **When to consider rollback**: if a legitimate production scenario (real sentry outage, accidental datacenter partition, etc.) leaves validators unable to mine despite needing to. This would indicate the policy itself is too strict, not a bug in the code. In that case, either revert or proceed to a richer design that preserves this policy for the common case but adds an explicit escape hatch.

## What This Fix Eliminates, in Scenario-Spec Terms

| Scenario-spec item | Before | After |
|--------------------|--------|-------|
| T2 (early-mining bypass) | 5-second grace, then solo-mines | Does not mine without peers |
| P1 (divergent canonicals) in restart-triggered cases | Produced by T2 | T2 no longer produces it |
| P1 in other cases (partition, crash, etc.) | Possible | Still possible |
| P2 (pathdb gap) | Structural | **Unchanged** — still structural |
| P3 (recent-signer saturation) | Correct by design | Unchanged |
| V1/V3/V4/V5/V6 variations | All vulnerable | All still vulnerable to their respective triggers |

The matrix makes the scope deliberately narrow. Preserving that narrowness is a feature: any future fix for P2 can layer on top without entangling its logic with the mining gate.

## Acceptance

This design is accepted when:

1. A reviewer can read this doc and the code change together and confirm the two behaviour-matrix rows that currently read "Skip" all correspond to code paths in `is_network_ready_to_mine`, and the one "Proceed" row is the only path that returns `true`.
2. Integration test #2 (T0→T5 reproduction) passes on a qanet harness.
3. The doc comment on `is_network_ready_to_mine` explicitly names the fresh-genesis bootstrap limitation, so a new reader of `bsc_miner.rs` does not have to chase down `docs/superpowers/specs/`.
