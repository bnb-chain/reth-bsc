# P2P Livelock Fix: Periodic Head Announcement Design

## Background

In reth-bsc, a peer's current best block is communicated to the network in only two moments:

1. **eth Status handshake** — exchanged once at connection time (and even this is partially broken: `status.blockhash` is left at the genesis hash due to a known TODO in `src/node/network/mod.rs:332`).
2. **NewBlock / NewBlockHashes broadcast** — triggered only when the node *imports a new block* (either produced locally or received from a peer).

No mechanism keeps peers informed of the local head between those events.

## Problem

**Livelock scenario**: two validators drift onto different fork chains and both become unable to produce new blocks:

- Parlia's "signed recently" rule blocks a validator from signing two blocks within a small window.
- Path-based state gap (`triedb pathdb gap: no difflayers and parent state root != pathdb disk layer root`) can block block production during recovery.

When both sides are blocked from producing, neither triggers `NewBlock` / `NewBlockHashes`. Because no other mechanism surfaces the local head to peers, neither side learns of the other's fork, and the network stays stuck indefinitely.

For comparison, geth-bsc has four independent mechanisms that jointly prevent this livelock:
- `chainSync.loop()` with a 10s `forceSyncCycle` timer (`eth/sync.go`)
- `blockRangeLoop()` on ETH69 (`eth/handler.go:1180`)
- BSC vote broadcasting, which continues during standby
- `peer.SetHead()` updates on every received announcement

reth-bsc has only the last of these, and only when announcements actually flow.

## Goal

Add the minimal mechanism needed to break the livelock: a periodic head announcement loop that informs peers of the local head even when no block is being imported. The loop must not make the node *worse off* — in particular, it must not spam peers with stale hashes that would trigger reputation penalties.

## Non-Goals (This Spec)

- Fixing the `status.blockhash` / `fork_id` mismatch at handshake time. The periodic announce loop covers fresh-connection scenarios within ~3 seconds of connection, making the handshake fix non-critical. It can be done in a follow-up.
- A full `chainSyncLoop` / `peerWithHighestTD` polling loop (geth-bsc's mechanism #1).
- ETH69-style `blockRangeLoop` re-announcement on reorg (geth-bsc's mechanism #2).
- Any change to the BSC subprotocol or vote flow.

## Design Principles

1. **Only announce when announcing helps.** If the local head is already >64 blocks behind a peer's known head, announcing a stale hash to that peer cannot trigger useful sync and will incur a `BadAnnouncement` reputation penalty (defined in `src/node/network/block_import/service.rs`, which enforces `MAX_STALE_BLOCK_DISTANCE = 64`). Skip those peers.
2. **Asymmetric fix is sufficient.** The livelock resolves as long as *at least one* side (the more-advanced one, or a side of equal height with a different hash) successfully announces. Both sides running this loop guarantees the condition is met.
3. **Use existing half-built scaffolding.** `ImportService` already has an `announce_interval` field initialized to a 5s `tokio::time::Interval` but never polled. This spec wires it up (and retunes it).

## Architecture

Two changes, both confined to `src/node/network/block_import/service.rs`:

1. **Extend `ImportService` with a `NetworkHandle`** so the announce path can query peer info and send peer messages.
2. **Wire `announce_interval` into the poll loop** so it ticks every 3 seconds and triggers a guarded broadcast.

No changes to `src/node/network/mod.rs` (other than passing the handle into `ImportService::new`), no changes to the BSC subprotocol, no new message types, no upstream reth changes.

---

## Change 1: Thread `NetworkHandle` into `ImportService`

**File**: `src/node/network/block_import/service.rs`, `ImportService` struct (~line 78) and `ImportService::new` (~line 111).

**What**: Add `network: NetworkHandle` as a field. Pass it in via the constructor.

**Why**: The announce path needs both `Peers::get_all_peers()` (to query per-peer `best_number`) and `NetworkHandle::send_eth_message()` (to send `PeerMessage::NewBlockHashes` to a specific peer). `NetworkHandle` provides both.

**Call site**: `src/node/network/mod.rs:~320` — the spawn that constructs `ImportService` already has access to the network handle; thread it through.

**Retune the interval**: change `Duration::from_secs(5)` to `Duration::from_secs(3)` at `service.rs:138`. Rationale: BSC block time is 450ms, so 3s ≈ 6-7 block slots — enough to stay responsive to fork events without being noisy.

---

## Change 2: Poll `announce_interval` and Broadcast Guarded

**File**: `src/node/network/block_import/service.rs`, the `Future::poll` implementation (or equivalent async loop driving the service).

**What**: Add a new arm in the poll loop:

```
while self.announce_interval.poll_tick(cx).is_ready() {
    self.spawn_head_announcement();
}
```

`spawn_head_announcement` must not block the poll loop. Since `Peers::get_all_peers()` is async (returns via a oneshot), the broadcast runs in a detached `tokio::spawn` task. The task body:

1. Read local head:
   - `num = provider.best_block_number()?`
   - If `num == 0` → return (node not past genesis, nothing useful to announce)
   - `hash = provider.block_hash(num)??` — if either fails, return
2. Query connected peers: `peers = network.get_all_peers().await`
3. For each `peer` in `peers`:
   - If `peer.best_number` is `Some(peer_best)` **and** `num + 64 < peer_best` → skip this peer (stale, would incur `BadAnnouncement`)
   - Otherwise, send:
     ```
     let msg = NewBlockHashes(vec![BlockHashNumber { hash, number: num }]);
     network.send_eth_message(peer.remote_id, PeerMessage::NewBlockHashes(msg));
     ```

**Why the per-peer guard is safe even when `peer.best_number` is `None`**: `best_number` is only `None` before any head info has been observed for that peer (pre-first-announce, and pre-block-number-resolution). In that window we have no evidence the peer is ahead, so announcing is the right default — the peer will either use the hint or ignore it without penalty.

**Why the guard correctly prevents self-harm**: `MAX_STALE_BLOCK_DISTANCE = 64` is the threshold the receiver uses to drop + penalize stale announcements. The receiver's check is `block_number + 64 < info.best_number` (strict `<`). Mirroring this exact comparison (`num + 64 < peer_best`) on the sender side guarantees we never send something the peer will treat as stale — including the boundary case of `gap == 64`, which is still acceptable to the receiver.

---

## Data Flow (Livelock Resolution)

```
T=0        Two validators A and B are each stuck on their own fork.
           A.head.number ≈ B.head.number (both recently stuck, small gap).
           No NewBlock / NewBlockHashes flows normally.

T=0..3s    A's announce_interval ticks:
             → peers query → B's best_number known, within 64 of A's head
             → A sends NewBlockHashes(A.head) to B
           B's announce_interval ticks:
             → A's best_number within 64 of B's head
             → B sends NewBlockHashes(B.head) to A

T=3..4s    A receives NewBlockHashes(B.head):
             → hash differs from A's local head at that number
             → A issues GetBlockHeaders to B
             → reth's existing block import / fork choice resolves the fork
           (Symmetric on B.)

T≈4s+      Parlia / fork choice picks canonical; losing side reorgs.
           Livelock broken.
```

## Edge Cases

| Situation | Behavior |
|---|---|
| Local `best_block_number == 0` (pre-sync) | Skip entire tick |
| `block_hash(num)` fails | Skip entire tick (log at `trace`) |
| Missed tick (poll was busy) | `MissedTickBehavior::Skip` (already set at `service.rs:139`) — do not burst |
| Zero connected peers | `get_all_peers()` returns empty → no-op |
| Peer's `best_number == None` | Announce (see "safe even when None" above) |
| Local head >64 behind a peer | Skip *that peer* only, continue with others |
| Node is in historical sync (far behind most peers) | Per-peer guard naturally skips ahead-peers; the loop becomes a no-op for them. No extra sync-state check needed. |

## Testing

**Unit tests** (new, in `src/node/network/block_import/`):
- Mock `NetworkHandle`; drive the announce tick; assert `send_eth_message` is called once per non-stale peer with a correctly-populated `NewBlockHashes`.
- Peer with `best_number = Some(local + 64)` → **is** announced to (edge of threshold; matches receiver's `<` comparison).
- Peer with `best_number = Some(local + 65)` → **skipped**.
- Peer with `best_number = None` → **is** announced to.
- `num == 0` → no send calls at all.

**Integration / manual**:
- Two-node local topology. Induce a fork (two miners, partition for a few slots, reconnect). Verify both nodes emit `NewBlockHashes` within one tick window and the fork resolves within a second tick.
- Regression: run a single node producing normally for a few minutes; confirm no peer is banned and no regression in block import latency. The periodic announce overlaps harmlessly with the existing on-import broadcast because peers de-duplicate announcements by hash.

## Rollback

The change is fully local to `ImportService`. Reverting is a single commit that removes the `NetworkHandle` field and the added poll arm; the previously-dormant `announce_interval` field can remain or be dropped independently.

## Follow-Up Work (Explicitly Deferred)

- Fix `status.blockhash` at handshake time, with corresponding `fork_id` recomputation. Tracked by the existing TODO at `src/node/network/mod.rs:332`.
- Consider a `forceSyncCycle`-style peer-head polling loop (`GetBlockHeaders` against highest-TD peer) as a defense-in-depth layer if this fix proves insufficient in production.
