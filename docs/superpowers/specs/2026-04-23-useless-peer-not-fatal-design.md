# `DiscUselessPeer` Not Fatal + Peer-Lifecycle Observability: Design

**Status**: Approved in brainstorming session 2026-04-23. Implementation plan to follow.
**Scope**: A behavior fix plus observability improvements in upstream reth's peer manager. Four self-contained changes in `bnb-chain/reth`; no `reth-bsc` code changes except bumping the pinned rev afterward.
**Target tree**: `/Users/jiaqiwang/workspace/reth` on branch `cross-region` (currently at `ef46a482a`, matching the rev pinned in `reth-bsc/Cargo.toml`).

## Related

- **reth-bsc TD / sync plan (open)**: `docs/superpowers/plans/2026-04-10-fix-td-and-p2p-sync.md` — covers three TD correctness issues (status TD, active fetch on missing parent, periodic announce). Partially landed via commits `daa254f / f4a1cbf / b727a47 / 56b60ef / b7881ba / cf212c3`.
- **Peer-gated mining**: `docs/superpowers/specs/2026-04-20-peer-gated-mining-design.md` — a different but adjacent peer-isolation mitigation.
- **P2P livelock fix**: `docs/superpowers/specs/2026-04-17-fix-p2p-livelock-design.md` — periodic head announcement to break forked-validator livelocks.

## Background

### Observed symptom

In a closed BSC QA deployment of 10 nodes (4 reth-bsc validators, 3 geth-bsc sentries, 3 reth-bsc fullnodes, split across US and EU regions), applying sustained transaction load caused every reth-bsc validator to drop 3–4 of its 6–9 peers within seconds. Grafana showed the loss was sticky: dropped peers did not return, and geth-bsc static-dial retries visibly failed at a ~35 s cadence (`dialHistoryExpiration` in `p2p/dial.go`). The pattern persisted until the affected reth-bsc nodes were restarted.

### Causal chain, verified end-to-end

Using a 1-minute `RUST_LOG=debug,net::session=trace,net::peers=trace` capture on one validator (reth-bsc) and the parallel `bsc.log` from one sentry (geth-bsc v1.6.3-799d6b50), a single peer drop at `08:08:48.843711Z` was reconstructed:

1. **geth-bsc downloader decides reth-bsc is stalling.** In `eth/downloader/downloader.go` the `checkStalling(td)` function compares the peer's advertised total difficulty against the local chain head *after* one sync round. If local TD has not caught up, `errStallingPeer` is returned. This is **a logical check, not a timeout** — the per-request TTL is a separate mechanism that emits `errBadPeer`.

2. **geth calls `dropPeer(id)` → `handler.removePeer(id)` → `peer.Peer.Disconnect(p2p.DiscUselessPeer)`.** See `eth/handler.go:815-821`. `DiscUselessPeer` is emitted on the wire.

3. **reth receives `P2PStreamError::Disconnected(UselessPeer)`.** `eth-wire/src/errors/p2p.rs:71` returns this variant when the remote sends a DISCONNECT frame.

4. **reth classifies this as `is_fatal_protocol_error == true`.** `crates/net/network/src/error.rs:136-180` lists `DisconnectReason::UselessPeer` alongside `ProtocolBreach` and `IncompatibleP2PProtocolVersion` in the fatal set.

5. **`on_connection_failure` takes the fatal path.** `crates/net/network/src/peers.rs:649-672` removes the peer from the peers table (non-trusted) and calls `ban_peer(peer_id)`. `ban_peer` writes the peer into `ban_list` with the default `ban_duration = 12 hours` (`network-types/src/peers/config.rs:112`).

6. **geth-bsc immediately static-dials back.** A fresh inbound session establishes on reth's side.

7. **reth kicks the inbound in ~28 μs.** `on_incoming_session_established` at `peers.rs:354` sees the peer in `ban_list` and queues `PeerAction::DisconnectBannedIncoming`, which propagates as `StateAction::Disconnect { reason: None }` (via `state.rs:389-393`) and is sent on the wire as `DisconnectRequested` (after `ActiveSessionHandle::disconnect(None)` unwraps at `session/active.rs:601`). The session lives for a few hundred microseconds before closing.

8. **geth logs this second drop with `req=true err="disconnect requested"`.** It does **not** add reth to any geth-side blacklist (`p2p/server.go`'s `disconnectEnodeSet` guard is `!pd.requested && pd.err == DiscRequested`, which only fires when geth itself locally calls `peer.Disconnect(DiscRequested)` — an admin-initiated path, not the downloader path). So geth continues static-dialing every 35 s, and reth continues kicking each attempt until the 12 h ban expires.

### The single event ban is not a BSC-specific bug

The same 1-minute log shows sentry3 emitting `"peer is stalling"` for **three different peers within ~1.1 seconds**:

```
08:08:48.532  peer=40bb9fa2…  name=Geth/v1.6.3…       err="peer is stalling: withheld headers: advertised 19544009, delivered 19544003"
08:08:48.809  peer=2ac0acc…   name=reth-bsc/v0.1.0…   err="peer is stalling"
08:08:49.633  peer=f5df9f3c…  name=Geth/v1.6.3…       err="peer is stalling: withheld headers: advertised 19544009, delivered 19544006"
```

Two of them are geth-bsc, not reth-bsc. All three advertised the same TD (`39064615`). The sentry's `checkStalling` is triggering on the entire peer set whenever a head advances faster than propagation + persistence allows. This is known geth downloader behavior.

**The classification bug is that a single transient signal emitted routinely by the reference Ethereum client is treated by reth as grounds for a 12-hour ban.** In practice, one propagation-lag moment is sufficient to isolate a reth node from every peer in its view.

### Comparison with the reference client

From `p2p/peer.go:397-446` and `p2p/server.go` `delpeer` handling:

| Client | Reaction on received `DiscUselessPeer` |
|---|---|
| go-ethereum / geth / geth-bsc | Close session; `dialScheduler` may retry after 35 s (`dialHistoryExpiration = inboundThrottleTime + 5 s`). **No blacklist, no per-peer cooldown beyond 35 s, no ban_list.** |
| reth (upstream current) | `peer.remove()` + `ban_list` 12 h, subsequent inbound kicked immediately on establishment. |

`go-ethereum` is the de-facto reference implementation of devp2p. Treating `DiscUselessPeer` as a transient signal is the ecosystem norm. reth's current classification is an overclassification bug, not a design choice.

### Observability gap (independent issue uncovered during diagnosis)

The entire causal chain above was reconstructed only because `net::session=trace` and `net::peers=trace` were manually enabled. At the default `INFO` level:

- **Reputation changes** are logged only at `TRACE` (`peers.rs` `apply_reputation_change` uses `trace!`).
- **Bans** (`ban_peer`) emit no user-visible log at all.
- **Inbound-rejected-because-banned** (`DisconnectBannedIncoming` path at `peers.rs:354-363, 372-374`) emits no log.
- **The session-level "failed to receive message err=disconnected"** at `session/active.rs:734` does not include the `DisconnectReason`, because `P2PStreamError::Disconnected`'s `Display` is just `"disconnected"` (`eth-wire/src/errors/p2p.rs:71`).

An operator watching an `INFO`-level log during a peer-drop event sees nothing actionable. Even a developer with `net::peers=trace` cannot tell why a peer was banned without reading source.

## Goal

Make reth tolerate transient `DiscUselessPeer` signals from peers without permanently isolating itself, and make every peer-lifecycle decision visible at default log levels.

**Primary purpose**: remove a self-amplifying failure mode where one propagation-lag event on any upstream peer cascades into reth-bsc losing its entire peer set for 12 hours.

**Secondary purpose**: ensure that when peer drops do happen (including the remaining fatal classifications that this spec intentionally leaves alone), the reason, actor, and outcome land in logs at levels operators already run at.

## Non-Goals

- **Not a root-cause fix for the geth-side `checkStalling` behavior.** Geth's downloader is entitled to emit `DiscUselessPeer` at any time; this spec is about reth's response to it, not about preventing the signal.
- **Not a fix for reth-bsc's `GetBlockHeaders` serving speed under load.** Under-delivery is one of several ways to make `checkStalling` fail; serving speed will be addressed in a separate spec.
- **Not a fix for reth-bsc's `NewBlock` TD broadcast.** `bsc_miner.rs:1074-1080` reads `header_td_by_number(parent_number)` with `unwrap_or_default()`, which can return `0` when the parent is in the engine-tree but not yet DB-persisted. This TD correctness work is already partially landed (`daa254f`, `56b60ef`, `b7881ba`) and continues under the TD plan.
- **Not a fix for engine-tree / provider visibility.** Whether upstream reth's `BlockchainProvider` correctly exposes in-memory engine-tree blocks to the `EthRequestHandler` is out of scope.
- **Not a CLI / env / config knob.** No `--ban-duration`, no `--disable-fatal-useless-peer`. The classification should be correct by default; making it tunable would preserve the footgun.
- **Not a change to `ReputationChangeWeights` values.** The `Dropped = -4096` figure, the 13-event path to `BANNED_REPUTATION`, and the 12 h `ban_duration` remain upstream defaults.
- **Not a metrics addition.** `disconnect_metrics` already exists upstream and is orthogonal to this work.
- **Does not change the behavior of `ProtocolBreach` or `IncompatibleP2PProtocolVersion`.** Those remain in the fatal set. They represent actual protocol-level incompatibilities; a 12 h ban for them is acceptable.
- **Does not touch `reth-bsc` source.** Only the pinned `rev` in `reth-bsc/Cargo.toml` needs a follow-up bump after the upstream change lands.

## Design Principles

1. **Fix the default, do not add a knob.** Treating `DiscUselessPeer` as a transient signal is the correct behavior for every reth deployment, public or closed. A CLI flag would institutionalize the bug.
2. **Keep punishing behavior available through a different path.** A peer that repeatedly causes us to drop sessions will still accumulate `Dropped(-4096)` reputation and eventually hit `ReputationChangeOutcome::Ban` after ~13 events. The ban mechanism is preserved; only the one-shot-fatal shortcut is removed.
3. **No behavior change for genuinely fatal reasons.** `ProtocolBreach` (remote observed us violating the protocol) and `IncompatibleP2PProtocolVersion` (remote cannot talk our devp2p version) stay fatal. Those represent connections that are genuinely doomed.
4. **Observability must be load-bearing.** Every decision that can make a peer disappear (`ban_peer`, `DisconnectBannedIncoming`, `on_connection_failure` fatal path) is logged at a level an operator runs at by default.
5. **Independent, revertable commits.** Each of the four changes stands on its own so a reviewer can accept or reject them independently. The classification fix (Change 1) is the behavioral fix; Changes 2–4 are observability improvements that are valuable even without Change 1.

## Architecture

Four self-contained changes in `bnb-chain/reth`, all under `crates/net/`.

### Change 1 — Reclassify `DiscUselessPeer` as non-fatal

**File:** `crates/net/network/src/error.rs`, `is_fatal_protocol_error` implementation for `EthStreamError` (roughly `L136-L180`).

Remove `DisconnectReason::UselessPeer` from the two disjunctions inside the `matches!` expression:

- `P2PStreamError::HandshakeError(P2PHandshakeError::Disconnected(…))` arm (around L145-L149)
- `P2PStreamError::Disconnected(…)` arm (around L154-L158)

`DisconnectReason::IncompatibleP2PProtocolVersion` and `DisconnectReason::ProtocolBreach` stay in both arms. No other arms in the function change.

Total diff in this change: two single-line removals.

**Effect on control flow:**

- `on_active_session_dropped` → `on_connection_failure(err, ReputationChangeKind::Dropped)` (`peers.rs:604-610`).
- `err.is_fatal_protocol_error()` now returns `false` for `UselessPeer`, so the fatal branch at `peers.rs:649` is skipped.
- The non-fatal else branch applies backoff (via `err.should_backoff()`, which currently maps `UselessPeer` to `BackoffKind::High`, a 15-minute outbound retry cooldown) and applies `ReputationChangeKind::Dropped` (`-4096`).
- Peer stays in the `peers` table with state `PeerConnectionState::Idle`, ready to accept a new inbound without rejection.

Peers that keep stalling will still eventually be banned — `Dropped(-4096)` accumulates, reputation recovers at `+1/sec` via `tick()`, and at steady-state ~35 s between events the net loss is ~4061/event. Thirteen events in rapid succession reach `BANNED_REPUTATION = -51200` and trigger the reputation-based `ReputationChangeOutcome::Ban` path. This takes on the order of minutes, not milliseconds, and correctly reflects a peer that is genuinely failing to cooperate.

### Change 2 — Reputation change logging

**File:** `crates/net/network/src/peers.rs`, `apply_reputation_change` (roughly `L495-L531`).

Current behavior: a single `trace!(target: "net::peers", …)` at entry named `"applied reputation change"`. Because the default `RUST_LOG` for a reth-bsc production node is `INFO`, this line is invisible.

New behavior: one structured log per successful reputation change, with the level determined by the outcome:

```rust
// Pseudocode for review; real implementation uses tracing::event! with a Level variable.
let level = match outcome {
    ReputationChangeOutcome::None   => Level::DEBUG,
    ReputationChangeOutcome::Ban
    | ReputationChangeOutcome::DisconnectAndBan
    | ReputationChangeOutcome::Unban => Level::INFO,
};
tracing::event!(level, target: "net::peers",
    ?peer_id,
    kind = ?reputation_change,
    delta = rep_weight,
    new_reputation = peer.reputation,
    ?outcome,
    "reputation change applied",
);
```

Rationale:

- `DEBUG`: every change is logged when an operator asks for it (`RUST_LOG=…,net::peers=debug`), but production `INFO` level stays quiet under normal vote / already-seen-transaction churn.
- `INFO`: any outcome that changes the peer's ban state is visible to default operators without them needing to know about trace targets. Ban is a rare event (seconds-per-day in a healthy cluster), so this does not spam the log.

The existing `trace!` line is replaced, not added to. No duplicate logging.

### Change 3 — `ban_peer` and `DisconnectBannedIncoming` warning logs

**File:** `crates/net/network/src/peers.rs`.

#### 3a. `ban_peer` (roughly `L410-L423`)

No user-visible log today. Add a `warn!` on entry:

```rust
// Pseudocode for review.
tracing::warn!(target: "net::peers",
    ?peer_id,
    duration = ?ban_duration,
    trusted = peer.map_or(false, |p| p.is_trusted() || p.is_static()),
    "banning peer",
);
```

`warn!` because a ban is an event an operator should notice in default logs, even if it turns out to be justified.

#### 3b. `on_connection_failure` fatal path (roughly `L649-L672`)

Current behavior: `trace!` on entry (`"handling failed connection"`) and a second `trace!` inside the fatal branch (`"fatal connection error"`). Both invisible at `INFO`.

New behavior: keep the existing traces, add a `warn!` **on the fatal branch only**, just before the `ban_peer` call:

```rust
tracing::warn!(target: "net::peers",
    ?peer_id,
    ?remote_addr,
    err = %err,
    "removing and banning peer on fatal protocol error",
);
```

Operators reading `INFO` will see exactly one line explaining that a peer was both removed and banned, with the error. Combined with Change 4 below, the `err =` value includes the `DisconnectReason` when the error was an incoming DISCONNECT frame.

#### 3c. `on_incoming_session_established` reject paths

Three separate early-return paths each queue a kick action with no log. Each gets its own `warn!` with a distinguishable `reason` label, so an operator can tell which condition tripped without reading source:

| Line | Condition | Action queued | `reason` label |
|---|---|---|---|
| `peers.rs:354-356` | `ban_list.is_banned_peer(peer_id)` — explicit ban_list entry | `DisconnectBannedIncoming` | `"banned_by_list"` |
| `peers.rs:361-363` | `trusted_nodes_only && !is_trusted` | `DisconnectUntrustedIncoming` | `"trusted_nodes_only"` |
| `peers.rs:372-374` | `peer.is_banned()` — reputation below `BANNED_REPUTATION` | `DisconnectBannedIncoming` | `"reputation_below_threshold"` |

```rust
// Pseudocode shape for all three sites.
tracing::warn!(target: "net::peers",
    ?peer_id,
    ?addr,
    reason = "<label from table above>",
    "rejecting established inbound session",
);
```

Operators will see exactly why a freshly-established inbound was terminated. Especially important because the wire DISCONNECT sent is `DisconnectRequested` with no further detail, so geth-side logs can only show `err="disconnect requested"` without a reason.

### Change 4 — Include `DisconnectReason` in `P2PStreamError::Disconnected`'s `Display`

**File:** `crates/net/eth-wire/src/errors/p2p.rs:71` (and the analogous `P2PHandshakeError::Disconnected` at `L118`, which already has the right shape).

Current:

```rust
#[error("disconnected")]
Disconnected(DisconnectReason),
```

Change to:

```rust
#[error("disconnected: {0}")]
Disconnected(DisconnectReason),
```

No code changes at log sites. `session/active.rs:734` (`failed to receive message err=%err`) automatically renders `err="disconnected: useless peer"`, `err="disconnected: protocol breach"`, etc.

Zero behavior change. Pure observability.

## Behavior delta

| Scenario | Before | After |
|---|---|---|
| Remote sends `DiscUselessPeer` | `peer.remove()` + `ban_list` 12 h + `Dropped(-4096)`. No `INFO` log. | `Dropped(-4096)` only; peer stays `Idle`; outbound-only `BackoffKind::High=15 min`. `DEBUG` log; `INFO` only if that reputation change itself triggers ban. |
| Remote sends `DiscProtocolBreach` | Fatal path (remove + ban 12 h). No `INFO` log. | Same fatal path, **plus** `warn!` log naming the peer and error. |
| Remote sends `DiscIncompatibleP2PProtocolVersion` | Fatal path. No `INFO` log. | Same fatal path, plus `warn!` log. |
| Remote sends `DiscTooManyPeers` / `DiscRequested` / `PingTimeout` | `BackoffKind::Low=30 s`, no ban. No `INFO` log. | Same behavior. `DEBUG` log from reputation change. |
| Reputation grinds to `BANNED_REPUTATION` via accumulated `Dropped` / `BadMessage` | Peer banned, no `INFO` log. | Peer banned, `INFO` log with outcome, plus `warn!` from `ban_peer`. |
| Trusted peer hits `Ban` outcome | Ban duration is `backoff_durations.low / 2 = 15 s`. No `INFO` log. | Same 15 s ban. `INFO` log (`ReputationChangeOutcome::Ban`) + `warn!` from `ban_peer` (with `trusted=true`). |
| Banned peer re-dials inbound | Silent rejection with wire reason `DisconnectRequested`. No `INFO` log. | Wire reason unchanged. `warn!` names `peer_id` and `reason = "banned"`. |
| Session closes on `P2PStreamError::Disconnected(reason)` | `err=disconnected` in logs (reason lost). | `err="disconnected: <reason>"` in logs. |

## Testing

### Unit tests — `crates/net/network/src/error.rs`

The existing test module already covers `is_fatal_disconnect` / `is_fatal_protocol_error`. Add:

- `test_useless_peer_is_not_fatal_post_handshake` — construct `EthStreamError::P2PStreamError(P2PStreamError::Disconnected(DisconnectReason::UselessPeer))`; assert `is_fatal_protocol_error() == false`.
- `test_useless_peer_is_not_fatal_during_handshake` — same for the `HandshakeError(P2PHandshakeError::Disconnected(UselessPeer))` variant.
- `test_protocol_breach_remains_fatal` — regression. Same test for `DisconnectReason::ProtocolBreach` asserting `== true`.
- `test_incompatible_version_remains_fatal` — regression.
- `test_useless_peer_backoff_is_high` — assert `err.should_backoff() == Some(BackoffKind::High)` for `UselessPeer`. (Verifies the fallback path.)

### Unit tests — `crates/net/eth-wire/src/errors/p2p.rs`

- `test_disconnected_display_includes_reason` — `format!("{}", P2PStreamError::Disconnected(DisconnectReason::UselessPeer))` equals `"disconnected: useless peer"`.

### Integration test — `crates/net/network/tests/it/`

Follow the pattern in existing `connect.rs` tests:

- `test_useless_peer_does_not_ban` — start two test nodes. Establish a session. Have peer A send `DisconnectReason::UselessPeer` to peer B. Assert on peer B:
  - `peers.ban_list.is_banned_peer(a_id) == false`
  - peer A remains in `peers` map with `state == Idle`
  - peer A's reputation is `DEFAULT_REPUTATION - 4096 = -4096`
  - a second inbound from A establishes successfully

- `test_protocol_breach_still_bans` — regression. Same setup with `DisconnectReason::ProtocolBreach`; assert ban is present.

### Logging validation

The new `warn!` / `info!` lines should appear under `cargo test --features <default>` in tests that exercise the ban path. Add `tracing-test` capture or `tracing_subscriber::fmt::TestWriter` in one of the integration tests to assert the log text contains `"banning peer"`, `"removing and banning peer on fatal protocol error"`, and `"rejecting established inbound session"` at the expected peer-manager moments.

### Manual reproduction

After merging, on the cross-region QA cluster:

1. Start all reth-bsc validators with `RUST_LOG=info`.
2. Apply the tx load that triggered the original symptom (see Background).
3. Expected: no validator loses more than a single peer to any one stalling event; peer counts recover within ~35 s (geth's static-dial interval). Any peer that *does* get banned appears in logs as a `warn!` from `peers.rs` with an operator-readable reason.

## Upstream strategy

One PR to `bnb-chain/reth` from a branch named `fix/useless-peer-not-fatal`. Four separable commits so reviewers can accept or reject them independently:

1. `fix(net): do not treat DiscUselessPeer as fatal protocol error`
2. `feat(net): log reputation changes at debug/info levels`
3. `feat(net): warn on peer ban and banned-incoming rejections`
4. `chore(eth-wire): include DisconnectReason in P2PStreamError::Disconnected Display`

PR description references this spec. Commit 1 is the behavioral fix and stands alone; commits 2–4 are observability improvements valuable even if commit 1 is rejected.

After merge to `bnb-chain/reth`, follow up in `reth-bsc`:

- Bump every `reth-*` dependency's `rev` in `Cargo.toml` in lockstep (per `CLAUDE.md` convention: mismatched revs produce duplicate-crate build failures).
- No functional changes in `reth-bsc` source.

Whether to also upstream to `paradigmxyz/reth` is left to the bnb-chain/reth maintainers. The classification argument applies equally to mainnet reth, so convergence is likely desirable but not a prerequisite for shipping this fix.

## Risks

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| A genuinely malicious mainnet peer abuses `DiscUselessPeer` to stay connected | Low | Marginal — after 13 such events the reputation path still bans them | No mitigation needed; the reputation path is the intended general-purpose ban mechanism |
| `BackoffKind::High = 15 min` for `UselessPeer` is too long for some topologies | Low | Slow outbound re-dial after disconnect | Backoff affects only our outbound dials; inbound re-dial from the peer works immediately. Tunable separately in a later spec if needed |
| `warn!` log volume under sustained incident | Low | Log spam during a legitimate outage | Bans are the only `warn!`-triggering path and are bounded by the `max_backoff_count` / ban cadence. Reputation `DEBUG` logs are default-suppressed |
| Downstream forks of reth that depend on the current fatal classification | Unknown | Behavior change without warning | CHANGELOG entry on the bnb-chain/reth fork; mention in the PR that this is a behavior fix, not a compatibility change |
| `Display` format change for `P2PStreamError::Disconnected` breaks log-parsing tooling | Very low | Tool needs regex update | Change is `"disconnected"` → `"disconnected: <reason>"`; any consumer matching the prefix still works |
