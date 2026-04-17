# P2P Livelock Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Wire the dormant `announce_interval` in `ImportService` into the poll loop so every 3 seconds the node sends `NewBlockHashes(local_head)` to each connected peer whose known head is not more than 64 blocks ahead — breaking the validator livelock where two forked, blocked-from-producing validators never learn of each other's heads.

**Architecture:** Single file changed (`src/node/network/block_import/service.rs`). A pure planner function decides per-peer whether to announce based on a 64-block stale guard (mirroring the existing receiver-side `MAX_STALE_BLOCK_DISTANCE`). The `Future::poll` implementation drives the existing 5s `tokio::time::Interval` (retuned to 3s); each tick spawns a detached task that reads the local head, calls `Peers::get_all_peers()` via the existing `crate::shared::get_network_handle()` global, filters with the planner, and sends `PeerMessage::NewBlockHashes`. Unit tests cover the pure planner without needing to mock `NetworkHandle`.

**Tech Stack:** Rust, tokio, reth-bsc 0.1, reth (bnb-chain fork rev `27bbd6b`) — `reth_network_api::{Peers, PeerInfo}`, `reth_network::message::PeerMessage`, `reth_eth_wire_types::broadcast::{NewBlockHashes, BlockHashNumber}`.

**Spec:** `docs/superpowers/specs/2026-04-17-fix-p2p-livelock-design.md`

---

## File Map

- **Modify**: `src/node/network/block_import/service.rs`
  - Retune `announce_interval` from 5s to 3s at ~line 138.
  - Add a new private function `plan_head_announcements` (pure, unit-tested).
  - Add a new method `ImportService::spawn_head_announcement` that spawns the async broadcast task.
  - Add one poll arm to the `Future::poll` implementation at ~line 610 that drives `announce_interval`.
  - Add unit tests for `plan_head_announcements` in the existing `#[cfg(test)] mod tests` (~line 659).

No other files change. The struct signature, constructor, and mod.rs wiring are untouched because the service already uses the global `crate::shared::get_network_handle()` (see existing usages at service.rs:448, 578).

---

## Task 1: Add pure planner function with unit tests (TDD)

**Files:**
- Modify: `src/node/network/block_import/service.rs` (add function above the existing `impl<Provider> Future for ImportService` block, and tests inside the `#[cfg(test)] mod tests`)

The planner decides, given the local head number and a list of `(peer_id, peer_best_number)` pairs, which peers to announce to. A peer is skipped when its known best number is more than 64 blocks ahead of our head — matching the receiver-side `MAX_STALE_BLOCK_DISTANCE = 64` check (`block_number + 64 < info.best_number`). This mirrors the receiver's strict `<` so a peer exactly 64 ahead is still announced to.

- [ ] **Step 1.1: Add imports needed for the planner**

At the top of `src/node/network/block_import/service.rs`, confirm these imports exist (some already do). Add any missing:

```rust
use reth_network_api::PeerId;          // already present (line 32)
// No new imports needed for the planner itself.
```

Planner only takes `(PeerId, Option<u64>)`; it does not depend on `PeerInfo` directly (this is deliberate, so tests don't have to construct a `PeerInfo`).

- [ ] **Step 1.2: Write the failing unit tests for `plan_head_announcements`**

Insert inside the existing `#[cfg(test)] mod tests` block (at the end of the file). Add the test module import for the planner and the tests:

```rust
    use super::plan_head_announcements;

    fn peer(tag: u8, best_number: Option<u64>) -> (PeerId, Option<u64>) {
        // `PeerId` is `alloy_primitives::B512`. Build a deterministic 64-byte
        // value from the tag via the `From<[u8; 64]>` impl.
        let mut bytes = [0u8; 64];
        bytes[0] = tag;
        (PeerId::from(bytes), best_number)
    }

    #[test]
    fn planner_announces_when_we_are_ahead() {
        let peers = vec![peer(1, Some(100))];
        let result = plan_head_announcements(200, &peers);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], peers[0].0);
    }

    #[test]
    fn planner_announces_at_exact_64_gap_boundary() {
        // Receiver drops only on strict `num + 64 < peer_best`, so gap == 64 is still fine.
        let local = 100;
        let peers = vec![peer(1, Some(local + 64))];
        let result = plan_head_announcements(local, &peers);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn planner_skips_peer_more_than_64_ahead() {
        let local = 100;
        let peers = vec![peer(1, Some(local + 65))];
        let result = plan_head_announcements(local, &peers);
        assert!(result.is_empty());
    }

    #[test]
    fn planner_announces_when_peer_best_number_unknown() {
        // best_number is None before any head info has been observed; announce is the right default.
        let peers = vec![peer(1, None)];
        let result = plan_head_announcements(100, &peers);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn planner_mixes_skip_and_announce_across_peers() {
        let local = 1000;
        let p_ahead = peer(1, Some(local + 65));   // skipped
        let p_at_boundary = peer(2, Some(local + 64)); // announced
        let p_behind = peer(3, Some(local - 10));  // announced
        let p_unknown = peer(4, None);             // announced
        let peers = vec![p_ahead.clone(), p_at_boundary.clone(), p_behind.clone(), p_unknown.clone()];
        let result = plan_head_announcements(local, &peers);
        assert_eq!(result.len(), 3);
        assert!(result.contains(&p_at_boundary.0));
        assert!(result.contains(&p_behind.0));
        assert!(result.contains(&p_unknown.0));
        assert!(!result.contains(&p_ahead.0));
    }

    #[test]
    fn planner_returns_empty_on_no_peers() {
        let result = plan_head_announcements(100, &[]);
        assert!(result.is_empty());
    }
```

- [ ] **Step 1.3: Run the tests to confirm they fail**

Run:
```bash
cargo test -p reth_bsc --lib node::network::block_import::service::tests::planner_ -- --nocapture
```

Expected: **FAIL with "cannot find function `plan_head_announcements` in this scope"**.

- [ ] **Step 1.4: Implement the planner**

Insert the following function just above the `impl<Provider> Future for ImportService<Provider>` block (~line 597, just after the last `impl ImportService` method closing brace and before `impl<Provider> Future for ...`):

```rust
/// Decide which peers to send `NewBlockHashes(local_head)` to.
///
/// A peer is skipped when its known `best_number` is more than
/// `MAX_STALE_BLOCK_DISTANCE` (64) blocks ahead of the local head: announcing a
/// stale hash to such a peer would be dropped and trigger a `BadAnnouncement`
/// reputation penalty on us.
///
/// A peer with `best_number = None` (head not yet observed) is announced to:
/// there's no evidence it is ahead, and the worst case is the peer ignores the
/// hint.
fn plan_head_announcements(
    local_head: u64,
    peers: &[(PeerId, Option<u64>)],
) -> Vec<PeerId> {
    const MAX_STALE_BLOCK_DISTANCE: u64 = 64;
    peers
        .iter()
        .filter_map(|(peer_id, peer_best)| match peer_best {
            Some(peer_best) if local_head + MAX_STALE_BLOCK_DISTANCE < *peer_best => None,
            _ => Some(*peer_id),
        })
        .collect()
}
```

- [ ] **Step 1.5: Run the tests to confirm they pass**

Run:
```bash
cargo test -p reth_bsc --lib node::network::block_import::service::tests::planner_ -- --nocapture
```

Expected: **all 6 planner_* tests PASS**.

- [ ] **Step 1.6: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "$(cat <<'EOF'
feat(p2p): add pure planner for periodic head announcement

Introduces plan_head_announcements() that mirrors the receiver-side
MAX_STALE_BLOCK_DISTANCE = 64 guard on the sender side. Pure function with
unit tests; not yet wired into poll loop.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Retune announce interval to 3s and wire poll arm

**Files:**
- Modify: `src/node/network/block_import/service.rs`
  - Change `Duration::from_secs(5)` → `Duration::from_secs(3)` at ~line 138.
  - Add `spawn_head_announcement` method on `ImportService`.
  - Add poll arm driving `announce_interval` in the `Future::poll` impl at ~line 610.

- [ ] **Step 2.1: Retune the announce interval from 5s to 3s**

Edit `src/node/network/block_import/service.rs` at ~line 138:

Change:
```rust
            announce_interval: {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval
            },
```

To:
```rust
            announce_interval: {
                // 3s ≈ 6-7 BSC slots (450ms each). Fast enough to break fork
                // livelocks, slow enough to be negligible overhead.
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(3));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                interval
            },
```

- [ ] **Step 2.2: Add `spawn_head_announcement` method on `ImportService`**

Add this method inside the existing `impl<Provider> ImportService<Provider>` block, placed next to the other `fn transfer_to_evn_peers` style helpers (before the closing `}` of that impl block; use the existing `transfer_to_evn_peers` location as a reference — it's near the end of the impl):

```rust
    /// Read local head and spawn a detached task that announces it to every
    /// connected peer that is not more than 64 blocks ahead of us.
    ///
    /// Runs on every `announce_interval` tick. This is the livelock-breaking
    /// mechanism for the case where two validators are forked and both are
    /// blocked from producing new blocks: without this, neither learns of the
    /// other's head after the initial handshake.
    fn spawn_head_announcement(&self) {
        let provider = self.forkchoice_engine.provider.clone();

        tokio::spawn(async move {
            // Resolve local head.
            let num = match provider.best_block_number() {
                Ok(n) if n > 0 => n,
                Ok(_) => {
                    tracing::trace!(target: "bsc::block_import", "Skip head announce: local best_block_number is 0");
                    return;
                }
                Err(e) => {
                    tracing::trace!(target: "bsc::block_import", error = %e, "Skip head announce: failed to read best_block_number");
                    return;
                }
            };
            let hash = match provider.block_hash(num) {
                Ok(Some(h)) => h,
                Ok(None) => {
                    tracing::trace!(target: "bsc::block_import", num, "Skip head announce: no hash for best_block_number");
                    return;
                }
                Err(e) => {
                    tracing::trace!(target: "bsc::block_import", num, error = %e, "Skip head announce: block_hash lookup failed");
                    return;
                }
            };

            // Resolve network handle.
            let net = match crate::shared::get_network_handle() {
                Some(n) => n,
                None => {
                    tracing::trace!(target: "bsc::block_import", "Skip head announce: network handle not yet initialized");
                    return;
                }
            };

            // Query peers.
            let peers = match net.get_all_peers().await {
                Ok(p) => p,
                Err(e) => {
                    tracing::trace!(target: "bsc::block_import", error = %e, "Skip head announce: get_all_peers failed");
                    return;
                }
            };
            if peers.is_empty() {
                return;
            }

            let peer_tuples: Vec<(PeerId, Option<u64>)> =
                peers.iter().map(|p| (p.remote_id, p.best_number)).collect();
            let targets = plan_head_announcements(num, &peer_tuples);

            if targets.is_empty() {
                return;
            }

            let hashes = NewBlockHashes(vec![BlockHashNumber { hash, number: num }]);
            let target_count = targets.len();
            for peer_id in targets {
                net.send_eth_message(peer_id, PeerMessage::NewBlockHashes(hashes.clone()));
            }
            tracing::trace!(
                target: "bsc::block_import",
                local_num = num,
                sent_to = target_count,
                total_peers = peers.len(),
                "Announced head to peers"
            );
        });
    }
```

**Import check**: confirm the following are already imported at the top of the file (all present at lines 22-32):
- `reth_eth_wire::BlockHashNumber`
- `reth_eth_wire_types::broadcast::NewBlockHashes`
- `reth_network::message::PeerMessage`
- `reth_network_api::PeerId`
- `reth_network_api::Peers` — **check this one**. If not present, add it; `get_all_peers` and `send_eth_message` require it in scope via the `Peers` trait and `NetworkHandle` inherent method respectively. Add at the top if missing:

```rust
use reth_network_api::Peers;
```

(`send_eth_message` is an inherent method on `NetworkHandle` — no trait import needed.)

- [ ] **Step 2.3: Add the poll arm that drives `announce_interval`**

Edit the `Future::poll` implementation for `ImportService` at ~line 610. Immediately before the final `Poll::Pending` return at ~line 655, insert:

```rust
        // Drive periodic head announcement to break forked-validator livelocks.
        // Each tick spawns a detached task so we never block the poll loop on
        // the async `get_all_peers()` query.
        while this.announce_interval.poll_tick(cx).is_ready() {
            this.spawn_head_announcement();
        }
```

Final layout of the bottom of `poll` should be:

```rust
        // ...existing pending_imports arm ends here...

        // Drive periodic head announcement ...
        while this.announce_interval.poll_tick(cx).is_ready() {
            this.spawn_head_announcement();
        }

        Poll::Pending
    }
}
```

- [ ] **Step 2.4: Build the crate**

Run:
```bash
cargo build -p reth_bsc
```

Expected: **clean build, no warnings about unused `announce_interval`** (the field was previously dead code; it is now polled).

If there are errors:
- `get_all_peers` not found → add `use reth_network_api::Peers;`.
- Lifetime / Send errors on the spawned future → the future captures only `Provider` (`Clone + Send + Sync + 'static`) and owned values; re-check that clones are taken before the `async move`.

- [ ] **Step 2.5: Re-run all service unit tests (regression check)**

Run:
```bash
cargo test -p reth_bsc --lib node::network::block_import::service
```

Expected: **all tests pass**, including the 6 new planner tests from Task 1 and the pre-existing service tests.

- [ ] **Step 2.6: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "$(cat <<'EOF'
feat(p2p): wire periodic head announcement into import service

Drives the previously-dormant announce_interval at 3s cadence. Each tick
spawns a detached task that reads local head, queries connected peers via
NetworkHandle::get_all_peers(), filters out peers more than 64 blocks ahead
(stale guard), and sends PeerMessage::NewBlockHashes to the rest. Breaks
validator livelock when two forked validators are both blocked from
producing new blocks.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Manual verification

**Goal:** Confirm the periodic announce runs end-to-end without regression on a real running node.

- [ ] **Step 3.1: Build the full binary**

Run:
```bash
cargo build --release -p reth_bsc --bin reth-bsc
```

Expected: clean release build.

- [ ] **Step 3.2: Start a single node and watch for the trace log**

Run the node in the usual way with tracing enabled for `bsc::block_import`:
```bash
RUST_LOG="bsc::block_import=trace,info" ./target/release/reth-bsc node <your-usual-args>
```

Once connected to at least one peer, within 3–6 seconds you should see log lines like:
```
TRACE bsc::block_import: Announced head to peers local_num=<N> sent_to=<K> total_peers=<M>
```

If no peers connect, the loop is still firing (no log because of the early `peers.is_empty()` return — that is intentional; add a temporary extra `trace!` if you need to see ticks during solo startup).

- [ ] **Step 3.3: Run for 5+ minutes, confirm no peer bans or abnormal disconnects**

Check the logs for:
- Absence of `peer banned` / `disconnect_peer` events beyond the normal churn baseline.
- Block import latency unchanged (compare `latest_block=...` cadence in the `reth::cli: Status` info lines against a pre-change run).

- [ ] **Step 3.4: Commit (no code — optional note commit)**

No code commit. If you want to document the manual verification, amend the previous commit message or add a note to `docs/superpowers/plans/2026-04-17-fix-p2p-livelock.md` in a follow-up.

---

## Self-Review Notes

Spec coverage check:

| Spec requirement | Task |
|---|---|
| Extend `ImportService` access to `NetworkHandle` | Task 2 (via existing `crate::shared::get_network_handle()` — no struct change needed) |
| Retune interval 5s → 3s | Task 2 Step 2.1 |
| Poll arm drives `announce_interval` | Task 2 Step 2.3 |
| `num == 0` guard | Task 2 Step 2.2 (`Ok(n) if n > 0` match arm) |
| `block_hash(num)` fail guard | Task 2 Step 2.2 (`Ok(None)` / `Err(_)` arms) |
| Per-peer stale guard (strict `num + 64 < peer_best`) | Task 1 (planner) + tests 1.2 |
| `best_number = None` announces | Task 1 Step 1.2 test |
| `MissedTickBehavior::Skip` preserved | Task 2 Step 2.1 (retained) |
| Unit tests without mocking `NetworkHandle` | Task 1 (planner is pure) |
| Manual integration check | Task 3 |
| Explicitly NOT touched: `status.blockhash`, ETH69 range, chainSync loop | Not in plan — deferred per spec Non-Goals |

No placeholders. No vague "add error handling" — each guard has its explicit arm and trace message. Types are consistent: `plan_head_announcements(u64, &[(PeerId, Option<u64>)]) -> Vec<PeerId>` used identically in tests and in `spawn_head_announcement`.
