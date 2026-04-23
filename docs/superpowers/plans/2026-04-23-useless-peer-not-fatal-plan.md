# DiscUselessPeer Not Fatal + Peer-Lifecycle Observability: Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Apply four independent changes in upstream reth so that a `DiscUselessPeer` disconnect no longer causes a 12 h ban and so that every peer-lifecycle decision is visible in logs at default levels.

**Architecture:** Four self-contained commits in the local `bnb-chain/reth` checkout at `/Users/jiaqiwang/workspace/reth`. Three touch `crates/net/network/src/peers.rs` and `crates/net/network/src/error.rs`; one touches `crates/net/eth-wire/src/errors/p2p.rs`. No `reth-bsc` source changes.

**Tech Stack:** Rust, upstream reth (bnb-chain fork, branch `cross-region` at `ef46a482a`), `tracing` for logs, `tokio::test` for async unit tests.

**Design spec:** `docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md` (in `reth-bsc` repo).

---

## Prerequisites

- **Target tree:** `/Users/jiaqiwang/workspace/reth` must be at branch `cross-region`, HEAD `ef46a482a`, working tree clean on tracked files. `git status --short` may list untracked `pkg/` directories and `AGENTS.md`; those are pre-existing and not touched by this plan.
- **Spec approved:** `docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md` in `reth-bsc` repo (committed as `af582c9`).
- **Baseline green:** `cargo test -p reth-network --lib` and `cargo test -p reth-eth-wire --lib` pass at `ef46a482a` (verified in Task 0).

## File Structure

All file paths below are relative to `/Users/jiaqiwang/workspace/reth` unless noted.

| File | Responsibility | Tasks that touch it |
|---|---|---|
| `crates/net/network/src/error.rs` | `EthStreamError::is_fatal_protocol_error` classifies disconnect reasons | Task 1 (remove `UselessPeer`), Task 1 (update existing test), Task 1 (add new tests) |
| `crates/net/network/src/peers.rs` | `PeersManager::apply_reputation_change` | Task 2 (upgrade log levels) |
| `crates/net/network/src/peers.rs` | `PeersManager::ban_peer` | Task 3 (add `warn!`) |
| `crates/net/network/src/peers.rs` | `PeersManager::on_connection_failure` fatal branch | Task 3 (add `warn!`) |
| `crates/net/network/src/peers.rs` | `PeersManager::on_incoming_session_established` three reject paths | Task 3 (add `warn!`) |
| `crates/net/network/src/peers.rs` tests module | `test_ban_on_active_drop`, `test_ban_on_pending_drop`, `test_dropped_incoming`, `test_reject_incoming_at_pending_capacity_trusted_peers` | Task 1 (substitute `UselessPeer` → `ProtocolBreach` where the test intent is "any fatal reason") |
| `crates/net/eth-wire/src/errors/p2p.rs` | `P2PStreamError::Disconnected` variant's `#[error(...)]` Display | Task 4 (add `: {0}`) |

## Commit Plan

Four separable commits, one per task body. Each task ends with a commit so the branch can be split or partially reverted during review.

| # | Commit title | Task | Spec § |
|---|---|---|---|
| 1 | `fix(net): do not treat DiscUselessPeer as fatal protocol error` | Task 1 | Change 1 |
| 2 | `feat(net): log reputation changes at debug/info levels` | Task 2 | Change 2 |
| 3 | `feat(net): warn on peer ban and banned-incoming rejections` | Task 3 | Change 3 |
| 4 | `chore(eth-wire): include DisconnectReason in P2PStreamError::Disconnected Display` | Task 5 | Change 4 |

Task 4 is an integration-level verification for Task 1 and Task 3 that does not itself produce a new commit — it simply runs the existing and new tests end-to-end.

---

## Task 0: Branch setup and baseline

**Files:** none modified.

- [ ] **Step 1: Switch to target tree and create branch**

```bash
cd /Users/jiaqiwang/workspace/reth
git status --short                              # confirm no tracked-file modifications
git rev-parse HEAD                              # must be ef46a482a or the pinned rev
git checkout -b fix/useless-peer-not-fatal
```

Expected: branch created off `cross-region`.

- [ ] **Step 2: Verify baseline test green**

```bash
cargo test -p reth-network --lib --quiet
cargo test -p reth-eth-wire --lib --quiet
```

Expected: both commands exit 0. (A warm `cargo check -p reth-network` first is fine; first build takes several minutes.)

If either suite fails here, stop — something in the working tree is already broken, and fixes for this plan will be indistinguishable from that prior breakage.

---

## Task 1: Reclassify `DiscUselessPeer` as non-fatal (spec Change 1)

**Files:**
- Modify: `crates/net/network/src/error.rs` (remove two `DisconnectReason::UselessPeer` match entries and update existing test)
- Modify: `crates/net/network/src/peers.rs` (update four existing tests whose intent is "any fatal disconnect reason triggers ban")

### Step 1: Add new unit tests that will fail today

- [ ] Append to the `#[cfg(test)] mod tests` block at `crates/net/network/src/error.rs:312-356`:

```rust
    #[test]
    fn test_useless_peer_not_fatal_during_handshake() {
        let err = PendingSessionHandshakeError::Eth(EthStreamError::P2PStreamError(
            P2PStreamError::HandshakeError(P2PHandshakeError::Disconnected(
                DisconnectReason::UselessPeer,
            )),
        ));
        assert!(!err.is_fatal_protocol_error());
    }

    #[test]
    fn test_useless_peer_not_fatal_post_handshake() {
        let err = EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
            DisconnectReason::UselessPeer,
        ));
        assert!(!err.is_fatal_protocol_error());
    }

    #[test]
    fn test_protocol_breach_remains_fatal_post_handshake() {
        let err = EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
            DisconnectReason::ProtocolBreach,
        ));
        assert!(err.is_fatal_protocol_error());
    }

    #[test]
    fn test_incompatible_p2p_version_remains_fatal_post_handshake() {
        let err = EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
            DisconnectReason::IncompatibleP2PProtocolVersion,
        ));
        assert!(err.is_fatal_protocol_error());
    }

    #[test]
    fn test_useless_peer_backoff_is_high() {
        let err = EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
            DisconnectReason::UselessPeer,
        ));
        assert_eq!(err.should_backoff(), Some(BackoffKind::High));
    }
```

### Step 2: Run the new tests — they must fail today

- [ ] Run:

```bash
cd /Users/jiaqiwang/workspace/reth
cargo test -p reth-network --lib error::tests --quiet 2>&1 | tail -30
```

Expected: both `test_useless_peer_not_fatal_*` assertions fail. The `test_protocol_breach_remains_fatal_post_handshake`, `test_incompatible_p2p_version_remains_fatal_post_handshake`, and `test_useless_peer_backoff_is_high` should pass (pre-existing behavior). If anything else fails, stop.

### Step 3: Update the existing `test_is_fatal_disconnect` at `error.rs:316-325`

The existing test hard-codes the assumption that `UselessPeer` is fatal. Replace it so it documents the behavior we keep (not the behavior we're fixing).

- [ ] Replace the body of `test_is_fatal_disconnect` (lines 316-325) with:

```rust
    #[test]
    fn test_is_fatal_disconnect() {
        // ProtocolBreach is the canonical example of a truly fatal disconnect:
        // the remote observed us violating the protocol and we have no way back.
        let err = PendingSessionHandshakeError::Eth(EthStreamError::P2PStreamError(
            P2PStreamError::HandshakeError(P2PHandshakeError::Disconnected(
                DisconnectReason::ProtocolBreach,
            )),
        ));

        assert!(err.is_fatal_protocol_error());
    }
```

### Step 4: Apply the production code change

- [ ] In `crates/net/network/src/error.rs`, locate the `impl SessionError for EthStreamError` `is_fatal_protocol_error` body (around line 136-180). Remove the two `DisconnectReason::UselessPeer |` lines — one in the `P2PHandshakeError::Disconnected(...)` arm (currently line 146) and one in the `P2PStreamError::Disconnected(...)` arm (currently line 155).

Before:

```rust
            Self::P2PStreamError(err) => {
                matches!(
                    err,
                    P2PStreamError::HandshakeError(
                        P2PHandshakeError::NoSharedCapabilities |
                            P2PHandshakeError::HelloNotInHandshake |
                            P2PHandshakeError::NonHelloMessageInHandshake |
                            P2PHandshakeError::Disconnected(
                                DisconnectReason::UselessPeer |
                                    DisconnectReason::IncompatibleP2PProtocolVersion |
                                    DisconnectReason::ProtocolBreach
                            )
                    ) | P2PStreamError::UnknownReservedMessageId(_) |
                        P2PStreamError::EmptyProtocolMessage |
                        P2PStreamError::ParseSharedCapability(_) |
                        P2PStreamError::CapabilityNotShared |
                        P2PStreamError::Disconnected(
                            DisconnectReason::UselessPeer |
                                DisconnectReason::IncompatibleP2PProtocolVersion |
                                DisconnectReason::ProtocolBreach
                        ) |
                        P2PStreamError::MismatchedProtocolVersion { .. }
                )
            }
```

After (two `DisconnectReason::UselessPeer |` lines removed):

```rust
            Self::P2PStreamError(err) => {
                matches!(
                    err,
                    P2PStreamError::HandshakeError(
                        P2PHandshakeError::NoSharedCapabilities |
                            P2PHandshakeError::HelloNotInHandshake |
                            P2PHandshakeError::NonHelloMessageInHandshake |
                            P2PHandshakeError::Disconnected(
                                DisconnectReason::IncompatibleP2PProtocolVersion |
                                    DisconnectReason::ProtocolBreach
                            )
                    ) | P2PStreamError::UnknownReservedMessageId(_) |
                        P2PStreamError::EmptyProtocolMessage |
                        P2PStreamError::ParseSharedCapability(_) |
                        P2PStreamError::CapabilityNotShared |
                        P2PStreamError::Disconnected(
                            DisconnectReason::IncompatibleP2PProtocolVersion |
                                DisconnectReason::ProtocolBreach
                        ) |
                        P2PStreamError::MismatchedProtocolVersion { .. }
                )
            }
```

No changes to the `EthHandshakeError` arm (around line 162-177). No changes to `should_backoff` (around line 182-236).

### Step 5: Re-run error.rs tests — must pass

- [ ] Run:

```bash
cargo test -p reth-network --lib error::tests --quiet
```

Expected: all tests pass.

### Step 6: Update four `peers.rs` tests whose intent is "any fatal reason triggers ban"

Four existing tests pass a `DisconnectReason::UselessPeer` through a fatal-only code path and assert that the peer gets banned. After Step 4, these tests expose a mismatch between the test's intent ("exercise the ban-on-fatal codepath") and its input (a now-non-fatal reason). The right fix is to switch the reason to `ProtocolBreach`, which remains fatal.

- [ ] **`test_ban_on_active_drop` at `crates/net/network/src/peers.rs:1609-1662`** — change the reason passed to `on_active_session_dropped` at line 1638. Before:

```rust
            &EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
                DisconnectReason::UselessPeer,
            )),
```

After:

```rust
            &EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
                DisconnectReason::ProtocolBreach,
            )),
```

- [ ] **`test_ban_on_pending_drop` at `crates/net/network/src/peers.rs:1720-~1775`** — change the reason at line 1749 from `UselessPeer` to `ProtocolBreach` (same `Before/After` shape as above, wrapped in `PendingSessionHandshakeError::Eth(EthStreamError::P2PStreamError(...))` depending on the surrounding context; preserve the outer wrapping and change only the reason variant).

- [ ] **`test_dropped_incoming` at `crates/net/network/src/peers.rs:1902-~1935`** — change the reason at line 1912 from `UselessPeer` to `ProtocolBreach` (same shape as above).

- [ ] **`test_reject_incoming_at_pending_capacity_trusted_peers` at `crates/net/network/src/peers.rs:1804-1888`** — **leave unchanged.** This test uses `UselessPeer` at line 1860 only as a generic pending-drop error; it asserts on capacity recovery (line 1887) and not on `ban_list` state. After Task 1 Step 4 it continues to pass regardless of the reason variant, because `on_incoming_pending_session_dropped` always calls `decr_pending_in` whether the error is fatal or not (`peers.rs:326-341`).

### Step 6.5: Add behavioral regression test for the fix

Complement the unit-level classification assertions from Step 1 with a behavioral test in the `peers.rs` tests module: construct a peer, drop its active session with `UselessPeer`, and assert that (a) no `PeerRemoved` action is emitted, (b) no `BanPeer` action is emitted, (c) the peer remains in the `peers` table, (d) `ban_list` stays clean. This is the positive inverse of `test_ban_on_active_drop`.

- [ ] Append to the `mod tests` block in `crates/net/network/src/peers.rs` (right after `test_ban_on_active_drop` at line ~1662):

```rust
    #[tokio::test]
    async fn test_useless_peer_does_not_ban_on_active_drop() {
        let peer = PeerId::random();
        let socket_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 1, 2)), 8008);
        let mut peers = PeersManager::default();
        peers.add_peer(peer, PeerAddr::from_tcp(socket_addr), None);

        // Consume the PeerAdded + Connect actions.
        match event!(peers) {
            PeerAction::PeerAdded(peer_id) => assert_eq!(peer_id, peer),
            _ => unreachable!(),
        }
        match event!(peers) {
            PeerAction::Connect { peer_id, .. } => assert_eq!(peer_id, peer),
            _ => unreachable!(),
        }

        poll_fn(|cx| {
            assert!(peers.poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        // Remote drops us with UselessPeer. After the fix this must NOT ban the peer.
        peers.on_active_session_dropped(
            &socket_addr,
            &peer,
            &EthStreamError::P2PStreamError(P2PStreamError::Disconnected(
                DisconnectReason::UselessPeer,
            )),
        );

        // No PeerRemoved or BanPeer action should surface. Poll once to confirm the
        // queue stays pending.
        poll_fn(|cx| {
            assert!(peers.poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;

        assert!(peers.peers.contains_key(&peer), "peer should remain in the table");
        assert!(!peers.ban_list.is_banned_peer(&peer), "peer should not be banned");
    }
```

### Step 7: Full `peers.rs` test suite must pass

- [ ] Run:

```bash
cargo test -p reth-network --lib peers::tests --quiet
```

Expected: all tests pass.

### Step 8: Full `reth-network` + `reth-eth-wire` suites must pass

- [ ] Run:

```bash
cargo test -p reth-network --lib --quiet
cargo test -p reth-eth-wire --lib --quiet
```

Expected: both exit 0.

### Step 9: Clippy clean on the touched crate

- [ ] Run:

```bash
cargo clippy -p reth-network --lib --tests -- -D warnings
```

Expected: exit 0.

### Step 10: Commit

- [ ] Stage and commit:

```bash
git add crates/net/network/src/error.rs crates/net/network/src/peers.rs
git commit -m "$(cat <<'EOF'
fix(net): do not treat DiscUselessPeer as fatal protocol error

DiscUselessPeer (0x03) is emitted by go-ethereum in several heterogeneous
cases: downloader stalling checks, random peer culling (eth/dropper.go),
no shared capability, etc. None of these indicate a permanently broken
connection, yet reth currently classifies it as a fatal protocol error
and bans the peer for 12 hours on a single occurrence. In a cross-region
BSC deployment under load, a single propagation-lag event is enough to
trigger the ban cascade and isolate a node from its entire peer set for
12 hours (see the debug trace in the design spec).

Remove DisconnectReason::UselessPeer from the two match arms inside
EthStreamError::is_fatal_protocol_error. The non-fatal branch of
on_connection_failure now handles it: Dropped(-4096) reputation,
BackoffKind::High outbound backoff, peer stays in the peers table.
A peer that repeatedly causes stalling disconnects still gets banned
after ~13 accumulated events via the reputation path.

ProtocolBreach and IncompatibleP2PProtocolVersion stay fatal - those
are real protocol-level incompatibilities where a 12 h ban is correct.

Four existing tests used UselessPeer as the input to exercise the
fatal code path. They're updated to use ProtocolBreach to preserve
their intent.

Ref: docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md
EOF
)"
```

---

## Task 2: Reputation change logging (spec Change 2)

**Files:**
- Modify: `crates/net/network/src/peers.rs:485-532` (`apply_reputation_change`)

### Step 1: Read the current function

- [ ] Read `crates/net/network/src/peers.rs:485-532`. Note the current `trace!` at line 486 and the `match outcome` block at lines 518-531.

### Step 2: Replace the existing trace with a level-selecting event at the end of the function

The existing `trace!` at line 486 logs the *intent* ("applying reputation change"). We want instead one log per *applied* change, so the level can depend on the `outcome`.

- [ ] In `crates/net/network/src/peers.rs`, make these two edits:

(a) Remove the existing trace call at line 486:

```rust
    pub(crate) fn apply_reputation_change(&mut self, peer_id: &PeerId, rep: ReputationChangeKind) {
        trace!(target: "net::peers", ?peer_id, reputation=?rep, "applying reputation change");   // <-- remove this line

        let outcome = if let Some(peer) = self.peers.get_mut(peer_id) {
```

(b) Before the `match outcome` block at line 518, capture the reputation for logging and emit a structured event. Replace lines 517-532 (from `};` after the if-let-else through the closing `}`) with:

```rust
        };

        // Visibility: log every successful reputation change. DEBUG for
        // None (no ban-state change); INFO when the change transitions
        // the peer's ban state.
        let new_reputation = self.peers.get(peer_id).map(|p| p.reputation).unwrap_or(0);
        let level = match outcome {
            ReputationChangeOutcome::None => tracing::Level::DEBUG,
            ReputationChangeOutcome::Ban
            | ReputationChangeOutcome::DisconnectAndBan
            | ReputationChangeOutcome::Unban => tracing::Level::INFO,
        };
        match level {
            tracing::Level::INFO => tracing::info!(
                target: "net::peers",
                ?peer_id,
                kind = ?rep,
                new_reputation,
                ?outcome,
                "reputation change applied",
            ),
            _ => tracing::debug!(
                target: "net::peers",
                ?peer_id,
                kind = ?rep,
                new_reputation,
                ?outcome,
                "reputation change applied",
            ),
        }

        match outcome {
            ReputationChangeOutcome::None => {}
            ReputationChangeOutcome::Ban => {
                self.ban_peer(*peer_id);
            }
            ReputationChangeOutcome::Unban => self.unban_peer(*peer_id),
            ReputationChangeOutcome::DisconnectAndBan => {
                self.queued_actions.push_back(PeerAction::Disconnect {
                    peer_id: *peer_id,
                    reason: Some(DisconnectReason::DisconnectRequested),
                });
                self.ban_peer(*peer_id);
            }
        }
    }
```

Rationale for the explicit `match level { ... }` rather than a dynamic `event!(level, ...)`: `tracing::event!` with a non-const level argument requires different syntax and fields must all be static-friendly. The match is more boring and equally correct.

### Step 3: Verify `peers.rs` tests still pass

- [ ] Run:

```bash
cargo test -p reth-network --lib peers::tests --quiet
```

Expected: all pass. The reputation logging change does not affect any test assertion.

### Step 4: Smoke-check the log output manually (optional but fast)

- [ ] Run a single reputation-exercising test with trace capture:

```bash
RUST_LOG=net::peers=debug cargo test -p reth-network --lib peers::tests::test_reputation_change_connected -- --nocapture 2>&1 | grep 'reputation change applied' | head
```

Expected: at least one line matching `reputation change applied ... kind=... new_reputation=... outcome=...`.

### Step 5: Clippy

- [ ] Run:

```bash
cargo clippy -p reth-network --lib --tests -- -D warnings
```

Expected: exit 0.

### Step 6: Commit

- [ ] Stage and commit:

```bash
git add crates/net/network/src/peers.rs
git commit -m "$(cat <<'EOF'
feat(net): log reputation changes at debug/info levels

apply_reputation_change previously emitted only a TRACE log at function
entry, invisible at the default INFO production level. Operators
troubleshooting peer drops could not see which peer had its reputation
slashed, by what kind, or what the resulting outcome was.

Replace the entry TRACE with a structured event at the end of the
function, after the outcome is known:

  - DEBUG for ReputationChangeOutcome::None (routine churn, e.g.
    AlreadySeenTransaction), only visible under net::peers=debug.
  - INFO for Ban / DisconnectAndBan / Unban, visible at default.

Fields: peer_id, kind (ReputationChangeKind), new_reputation, outcome.

Ref: docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md
EOF
)"
```

---

## Task 3: WARN logs on ban_peer, fatal branch, and inbound-reject paths (spec Change 3)

**Files:**
- Modify: `crates/net/network/src/peers.rs:410-423` (`ban_peer`)
- Modify: `crates/net/network/src/peers.rs:640-672` (`on_connection_failure` fatal branch)
- Modify: `crates/net/network/src/peers.rs:349-394` (`on_incoming_session_established` three reject paths)

### Step 1: `ban_peer` — add `warn!` on entry

- [ ] In `crates/net/network/src/peers.rs`, replace the body of `ban_peer` at lines 410-423 with:

```rust
    fn ban_peer(&mut self, peer_id: PeerId) {
        let peer_entry = self.peers.get(&peer_id);
        let trusted_or_static =
            peer_entry.is_some_and(|p| p.is_trusted() || p.is_static());

        let ban_duration = if trusted_or_static {
            // For misbehaving trusted or static peers, we provide a bit more leeway when
            // penalizing them.
            self.backoff_durations.low / 2
        } else {
            self.ban_duration
        };

        tracing::warn!(
            target: "net::peers",
            ?peer_id,
            duration = ?ban_duration,
            trusted = trusted_or_static,
            "banning peer",
        );

        self.ban_list.ban_peer_until(peer_id, std::time::Instant::now() + ban_duration);
        self.queued_actions.push_back(PeerAction::BanPeer { peer_id });
    }
```

Behavior difference from the original: the `if let Some(peer) = self.peers.get(&peer_id) && (peer.is_trusted() || peer.is_static())` construct is factored into two local bindings so the trusted/static state is reusable for logging. No functional change.

### Step 2: `on_connection_failure` fatal branch — add `warn!`

- [ ] In `crates/net/network/src/peers.rs`, inside `on_connection_failure` at lines 640-672, locate the fatal branch starting at `if err.is_fatal_protocol_error() {` (line 649). Add a `warn!` at the top of that branch, immediately after the existing `trace!("fatal connection error")`. The final shape should be:

```rust
        if err.is_fatal_protocol_error() {
            trace!(target: "net::peers", ?remote_addr, ?peer_id, %err, "fatal connection error");
            tracing::warn!(
                target: "net::peers",
                ?remote_addr,
                ?peer_id,
                err = %err,
                "removing and banning peer on fatal protocol error",
            );
            // remove the peer to which we can't establish a connection due to protocol related
            // issues.
            if let Entry::Occupied(mut entry) = self.peers.entry(*peer_id) {
                self.connection_info.decr_state(entry.get().state);
                // only remove if the peer is not trusted
                if entry.get().is_trusted() {
                    entry.get_mut().state = PeerConnectionState::Idle;
                } else {
                    entry.remove();
                    self.queued_actions.push_back(PeerAction::PeerRemoved(*peer_id));
                    // If the error is caused by a peer that should be banned from discovery
                    if err.merits_discovery_ban() {
                        self.queued_actions.push_back(PeerAction::DiscoveryBanPeerId {
                            peer_id: *peer_id,
                            ip_addr: remote_addr.ip(),
                        })
                    }
                }
            }

            // ban the peer
            self.ban_peer(*peer_id);
        } else {
```

Rationale for keeping the existing `trace!("fatal connection error")`: it carries slightly different phrasing than the new `warn!`, and trace-enabled developers may parse against it. Removing it would be a gratuitous behavior change for this commit.

### Step 3: `on_incoming_session_established` — three reject paths

Three separate early returns each queue a kick action with no log. Add a `warn!` immediately before each `return;`. Labels must match Table 3c of the spec so operators can tell the paths apart.

- [ ] At `peers.rs:354-356`, inside the `if self.ban_list.is_banned_peer(&peer_id)` branch, add a `warn!` before `return;`:

```rust
        if self.ban_list.is_banned_peer(&peer_id) {
            tracing::warn!(
                target: "net::peers",
                ?peer_id,
                ?addr,
                reason = "banned_by_list",
                "rejecting established inbound session",
            );
            self.queued_actions.push_back(PeerAction::DisconnectBannedIncoming { peer_id });
            return;
        }
```

- [ ] At `peers.rs:361-363`, inside the `if self.trusted_nodes_only && !is_trusted` branch, add a `warn!` before `return;`:

```rust
        if self.trusted_nodes_only && !is_trusted {
            tracing::warn!(
                target: "net::peers",
                ?peer_id,
                ?addr,
                reason = "trusted_nodes_only",
                "rejecting established inbound session",
            );
            self.queued_actions.push_back(PeerAction::DisconnectUntrustedIncoming { peer_id });
            return;
        }
```

- [ ] At `peers.rs:372-374`, inside the `if peer.is_banned()` branch, add a `warn!` before `return;`:

```rust
                if peer.is_banned() {
                    tracing::warn!(
                        target: "net::peers",
                        ?peer_id,
                        ?addr,
                        reason = "reputation_below_threshold",
                        "rejecting established inbound session",
                    );
                    self.queued_actions.push_back(PeerAction::DisconnectBannedIncoming { peer_id });
                    return;
                }
```

### Step 4: `peers.rs` tests must still pass

- [ ] Run:

```bash
cargo test -p reth-network --lib peers::tests --quiet
```

Expected: all pass. None of the new `warn!` lines change control flow; they only emit logs.

### Step 5: Smoke-check the new warn lines manually

- [ ] Run a ban-exercising test with captured output:

```bash
RUST_LOG=net::peers=warn cargo test -p reth-network --lib peers::tests::test_ban_on_active_drop -- --nocapture 2>&1 | grep -E '(banning peer|removing and banning)' | head
```

Expected: one `removing and banning peer on fatal protocol error` line and one `banning peer` line.

- [ ] And a banned-incoming test (if an equivalent exists; otherwise this check is deferred to Task 4):

```bash
RUST_LOG=net::peers=warn cargo test -p reth-network --lib peers::tests::test_on_active_inbound_ban_list -- --nocapture 2>&1 | grep 'rejecting established inbound session' | head
```

Expected: one `reason=banned_by_list` line.

### Step 6: Clippy

- [ ] Run:

```bash
cargo clippy -p reth-network --lib --tests -- -D warnings
```

Expected: exit 0.

### Step 7: Commit

- [ ] Stage and commit:

```bash
git add crates/net/network/src/peers.rs
git commit -m "$(cat <<'EOF'
feat(net): warn on peer ban and banned-incoming rejections

Three peer-disappearance paths in PeersManager emitted no log at the
default INFO level, making it impossible for an operator to see why
a peer was removed during an incident:

  * ban_peer (entry): no log at all.
  * on_connection_failure fatal branch: TRACE only.
  * on_incoming_session_established three reject paths: no log at all.

Add structured warn!() lines at each site. The inbound-reject log
carries a reason label ("banned_by_list", "trusted_nodes_only",
"reputation_below_threshold") so the three paths are distinguishable
in logs - important because the wire DISCONNECT carries no further
detail and the remote side's log only shows "disconnect requested".

Ref: docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md
EOF
)"
```

---

## Task 4: Integration validation

**Files:** none modified (this task runs existing and new tests end-to-end; no commit).

### Step 1: Full workspace tests for touched crates

- [ ] Run:

```bash
cd /Users/jiaqiwang/workspace/reth
cargo test -p reth-network --quiet     # includes tests/it/
cargo test -p reth-eth-wire --quiet
```

Expected: both exit 0. The integration suite at `crates/net/network/tests/it/` exercises real socket pairs and is the closest existing coverage for the useless-peer-does-not-ban contract; any regression there blocks this plan.

### Step 2: Clippy across touched crates

- [ ] Run:

```bash
cargo clippy -p reth-network --all-targets -- -D warnings
cargo clippy -p reth-eth-wire --all-targets -- -D warnings
```

Expected: exit 0.

### Step 3: `cargo fmt` is clean

- [ ] Run:

```bash
cargo +nightly fmt --check -p reth-network
cargo +nightly fmt --check -p reth-eth-wire
```

Expected: exit 0. If nightly is unavailable, `cargo fmt --check` is acceptable — reth uses nightly-only rustfmt settings but stable rustfmt still catches most issues.

No commit in this task.

---

## Task 5: Include `DisconnectReason` in `P2PStreamError::Disconnected` Display (spec Change 4)

**Files:**
- Modify: `crates/net/eth-wire/src/errors/p2p.rs:70-72` (`Disconnected` variant's `#[error("...")]` attribute)
- Create: `crates/net/eth-wire/src/errors/p2p.rs` test module (or extend if one is added later)

### Step 1: Add a failing unit test

The file has no test module today. Add one at the bottom of `crates/net/eth-wire/src/errors/p2p.rs` (after line 133, the closing brace of the `PingerError` enum):

- [ ] Append:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use reth_eth_wire_types::DisconnectReason;

    #[test]
    fn p2p_disconnected_display_includes_reason() {
        let err = P2PStreamError::Disconnected(DisconnectReason::UselessPeer);
        assert_eq!(format!("{err}"), "disconnected: useless peer");
    }

    #[test]
    fn p2p_handshake_disconnected_display_shape_unchanged() {
        // Regression: the handshake variant already includes the reason.
        let err = P2PHandshakeError::Disconnected(DisconnectReason::ProtocolBreach);
        assert_eq!(format!("{err}"), "disconnected by peer: protocol breach");
    }
}
```

The exact Display strings are from `crates/net/eth-wire-types/src/disconnect_reason.rs`: `DisconnectReason::UselessPeer` has `#[display("useless peer")]` at line 27, and `DisconnectReason::ProtocolBreach` has `#[display("protocol breach")]`. The assertions above match these.

### Step 2: Run the new test — it must fail today

- [ ] Run:

```bash
cargo test -p reth-eth-wire --lib errors::p2p::tests::p2p_disconnected_display --quiet 2>&1 | tail -20
```

Expected: `p2p_disconnected_display_includes_reason` fails with an assertion diff (`"disconnected"` vs `"disconnected: useless peer"`). The `p2p_handshake_disconnected_display_shape_unchanged` test should pass — it documents behavior that already existed.

### Step 3: Apply the production change

- [ ] In `crates/net/eth-wire/src/errors/p2p.rs`, change the `#[error(...)]` attribute on the `Disconnected` variant at line 71. Before:

```rust
    /// Disconnected error.
    #[error("disconnected")]
    Disconnected(DisconnectReason),
```

After:

```rust
    /// Disconnected error.
    #[error("disconnected: {0}")]
    Disconnected(DisconnectReason),
```

Do not touch the `P2PHandshakeError::Disconnected` variant at line 118 — it already has `#[error("disconnected by peer: {0}")]` and the regression test asserts it stays that way.

### Step 4: Verify new test passes

- [ ] Run:

```bash
cargo test -p reth-eth-wire --lib errors::p2p::tests --quiet
```

Expected: both tests pass.

### Step 5: Run the full `reth-eth-wire` and `reth-network` suites

- [ ] Run:

```bash
cargo test -p reth-eth-wire --quiet
cargo test -p reth-network --quiet
```

Expected: both exit 0. `reth-network` depends on `reth-eth-wire` so its session-layer tests may observe the Display change. If any log-matching assertion breaks there, the fix is to update the expected string to include `: <reason>` — no behavior change.

### Step 6: Clippy

- [ ] Run:

```bash
cargo clippy -p reth-eth-wire --all-targets -- -D warnings
```

Expected: exit 0.

### Step 7: Commit

- [ ] Stage and commit:

```bash
git add crates/net/eth-wire/src/errors/p2p.rs
git commit -m "$(cat <<'EOF'
chore(eth-wire): include DisconnectReason in P2PStreamError::Disconnected Display

Previously, P2PStreamError::Disconnected(reason) formatted as just
"disconnected", discarding the reason. Downstream log sites like
crates/net/network/src/session/active.rs:734 then emitted lines of
the form `err=disconnected` with no indication of what the remote
actually said.

Change the Display format from "disconnected" to "disconnected: {0}"
so log sites get the reason for free. The sibling variant
P2PHandshakeError::Disconnected already has this shape and is
unchanged.

Ref: docs/superpowers/specs/2026-04-23-useless-peer-not-fatal-design.md
EOF
)"
```

---

## Task 6: Final green check and review prep

**Files:** none modified.

### Step 1: Full test pass on branch

- [ ] Run:

```bash
cd /Users/jiaqiwang/workspace/reth
cargo test -p reth-network --quiet
cargo test -p reth-eth-wire --quiet
```

Expected: both green.

### Step 2: Commit graph sanity-check

- [ ] Run:

```bash
git log --oneline cross-region..HEAD
```

Expected: exactly four commits in this order:

```
<sha>  chore(eth-wire): include DisconnectReason in P2PStreamError::Disconnected Display
<sha>  feat(net): warn on peer ban and banned-incoming rejections
<sha>  feat(net): log reputation changes at debug/info levels
<sha>  fix(net): do not treat DiscUselessPeer as fatal protocol error
```

### Step 3: Diff stats per commit, for PR description

- [ ] Run:

```bash
git log --stat cross-region..HEAD
```

Expected: each commit touches exactly the files listed in its task. Ranges:

- Commit 1 (Task 1): `crates/net/network/src/error.rs` (~25 lines changed: 2 lines removed in `is_fatal_protocol_error`, ~5 lines for the `test_is_fatal_disconnect` refresh, ~25 lines of new unit tests), `crates/net/network/src/peers.rs` (~55 lines changed: 3 lines substituted in the 3 ban-asserting tests, ~50 new lines for `test_useless_peer_does_not_ban_on_active_drop`).
- Commit 2 (Task 2): `crates/net/network/src/peers.rs` (~30 lines changed, net additions only).
- Commit 3 (Task 3): `crates/net/network/src/peers.rs` (~40 lines changed, net additions only).
- Commit 4 (Task 5): `crates/net/eth-wire/src/errors/p2p.rs` (~20 lines changed: 1 production + ~18 tests).

If any commit bleeds into an unrelated file, `git commit --amend` with the misattributed file moved, or `git reset HEAD~N` and re-commit cleanly.

### Step 4: Optional manual reproduction against the QA cluster

Out of scope for this plan (the QA cluster is not available to the implementing agent). After merge and rev-bump in `reth-bsc`, the design spec's "Manual reproduction" section describes the load test that should show peer counts recovering within ~35 s instead of staying sticky for 12 h.

---

## Rollback / Partial Acceptance

Each commit is independently revertable:

- Reverting Commit 4 only removes the reason from `err=disconnected` log output; no behavior change.
- Reverting Commit 3 only removes logs; no behavior change.
- Reverting Commit 2 only removes logs; no behavior change.
- Reverting Commit 1 restores the 12 h ban on `DiscUselessPeer`; no other effect.

If a reviewer asks to drop Commit 2 or Commit 3 but keep Commit 1, `git rebase -i cross-region` and remove those commits. Commit 1's test changes in `peers.rs` are independent of the logging commits and will remain intact.

## Follow-up in reth-bsc (out of scope for this plan)

After the PR merges on `bnb-chain/reth`:

1. In `/Users/jiaqiwang/workspace/reth-bsc/Cargo.toml`, update every `rev = "ef46a482a182f195d7623a6ca24643c0ada6d893"` to the new merged commit. Every `reth-*` line must be bumped in lockstep or the build will hit duplicate-crate errors (see `CLAUDE.md`).
2. Run `cargo check --workspace` in `reth-bsc` to confirm the rev bump compiles.
3. Commit with `chore(deps): bump reth rev for non-fatal UselessPeer fix`.
4. Deploy to the QA cluster and execute the spec's manual reproduction procedure.
