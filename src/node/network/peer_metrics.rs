//! Periodic aggregated peer-lifecycle metrics.
//!
//! Subscribes to the reth network's `PeerEvent` stream and emits **one
//! `INFO`-level summary line per window** (default 60 s) with counts of
//! sessions established, sessions closed (broken out by
//! `DisconnectReason`), and peer add/remove deltas.
//!
//! Purpose — diagnosing the `peers-drop-to-zero-after-sync` drain
//! (reth-bsc #320). Under the old behaviour we had no visibility in
//! production into WHY peers were disconnecting; rep-penalty code paths,
//! protocol-breach disconnects, and ordinary ping timeouts all looked the
//! same from the outside. This collector turns it into a single
//! low-frequency line that tells you the shape of the drain:
//!
//!   INFO bsc::peer_metrics window_secs=60 opened=3 closed=15
//!        closed_by=disconnect_requested:8,too_many_peers:4,ping_timeout:3
//!        "peer lifecycle summary"
//!
//! which is digestible to a human AND easy to grep / build Grafana
//! panels off later.
//!
//! Design choices:
//!   * aggregation window, not per-event logging — per-peer churn can be
//!     many per second in a healthy network; logging every one would
//!     drown the rest of the node output.
//!   * INFO level, not DEBUG — you want this visible by default in a
//!     running prod node, without having to bump filters.
//!   * per-`DisconnectReason` bucketing — drain-to-zero looks the same as
//!     healthy churn if you only count events; the reason distribution
//!     is what tells you whether something's wrong.
//!   * idle windows are silent — if no peer events happened, no log line
//!     is emitted. Keeps the feed clean during steady-state operation.

use futures::StreamExt;
use reth_eth_wire::DisconnectReason;
use reth_network_api::{
    events::{NetworkEvent, PeerEvent},
    NetworkEventListenerProvider,
};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

/// How often we emit a summary. 60 s is a reasonable default — long
/// enough that healthy churn isn't noisy, short enough that a drain
/// becomes visible within a couple of window prints.
const WINDOW: Duration = Duration::from_secs(60);

/// Per-window accumulator. Reset after each summary.
#[derive(Default)]
struct Window {
    opened: u64,
    closed: u64,
    added: u64,
    removed: u64,
    close_reasons: HashMap<&'static str, u64>,
}

impl Window {
    fn record(&mut self, event: PeerEvent) {
        match event {
            PeerEvent::SessionEstablished(_) => self.opened += 1,
            PeerEvent::SessionClosed { reason, .. } => {
                self.closed += 1;
                let label = reason.map(disconnect_label).unwrap_or("unknown");
                *self.close_reasons.entry(label).or_default() += 1;
            }
            PeerEvent::PeerAdded(_) => self.added += 1,
            PeerEvent::PeerRemoved(_) => self.removed += 1,
        }
    }

    fn any_activity(&self) -> bool {
        self.opened > 0 || self.closed > 0 || self.added > 0 || self.removed > 0
    }
}

/// Turn a `DisconnectReason` into a stable short snake-case label that
/// humans can read and scripts can grep for without worrying about the
/// enum's `Display` format changing across reth versions.
fn disconnect_label(reason: DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::DisconnectRequested => "disconnect_requested",
        DisconnectReason::TcpSubsystemError => "tcp_error",
        DisconnectReason::ProtocolBreach => "protocol_breach",
        DisconnectReason::UselessPeer => "useless_peer",
        DisconnectReason::TooManyPeers => "too_many_peers",
        DisconnectReason::AlreadyConnected => "already_connected",
        DisconnectReason::IncompatibleP2PProtocolVersion => "incompatible_p2p_version",
        DisconnectReason::NullNodeIdentity => "null_identity",
        DisconnectReason::ClientQuitting => "client_quitting",
        DisconnectReason::UnexpectedHandshakeIdentity => "unexpected_handshake_identity",
        DisconnectReason::ConnectedToSelf => "connected_to_self",
        DisconnectReason::PingTimeout => "ping_timeout",
        DisconnectReason::SubprotocolSpecific => "subprotocol_specific",
    }
}

/// Render the per-reason counts in a stable `k:v,k:v,...` format sorted by
/// reason label (so the line is diffable across windows).
fn format_reasons(reasons: &HashMap<&'static str, u64>) -> String {
    let mut pairs: Vec<(&&str, &u64)> = reasons.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs.into_iter().map(|(k, v)| format!("{k}:{v}")).collect::<Vec<_>>().join(",")
}

/// Subscribe to the supplied network handle's event stream and spawn a
/// background task that emits one INFO-level summary line per [`WINDOW`].
/// Call this ONCE, after the network is up (e.g. post-launch in `main.rs`).
pub fn spawn<N>(network: &N)
where
    N: NetworkEventListenerProvider + 'static,
{
    let stream = network.event_listener();
    tokio::spawn(run(stream));
}

/// Generic over the concrete `EventStream<NetworkEvent<_>>` so we don't
/// have to name `reth-tokio-util::EventStream` directly (it's not in our
/// dep list — we let the caller hand us whatever the trait returns and
/// constrain it via `Stream`).
async fn run<S, R>(mut stream: S)
where
    S: futures::Stream<Item = NetworkEvent<R>> + Unpin,
{
    let mut window = Window::default();
    let mut ticker = tokio::time::interval(WINDOW);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick fires immediately; burn it so our first real summary
    // covers a full window.
    ticker.tick().await;

    let mut started = Instant::now();

    loop {
        tokio::select! {
            maybe_event = stream.next() => {
                let Some(event) = maybe_event else {
                    tracing::debug!(target: "bsc::peer_metrics", "network event stream closed");
                    return;
                };
                // Convert to PeerEvent via the From impl — ActivePeerSession
                // collapses to SessionEstablished.
                window.record(event.into());
            }
            _ = ticker.tick() => {
                if window.any_activity() {
                    tracing::info!(
                        target: "bsc::peer_metrics",
                        window_secs = WINDOW.as_secs(),
                        window_started_ms_ago = started.elapsed().as_millis() as u64,
                        opened = window.opened,
                        closed = window.closed,
                        added = window.added,
                        removed = window.removed,
                        closed_by = %format_reasons(&window.close_reasons),
                        "peer lifecycle summary",
                    );
                }
                window = Window::default();
                started = Instant::now();
            }
        }
    }
}
