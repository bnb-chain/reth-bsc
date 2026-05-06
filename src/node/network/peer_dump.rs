//! Periodic p2p diagnostics. Each tick logs to `bsc::peers` (INFO):
//!   * connected peers (reputation, fork id, td, kind, age)
//!   * ban list + backoff list
//!   * cumulative reputation-change kinds + dial-failure classes since boot
//!   * events that arrived since the previous tick: disconnects, reputation
//!     changes, dial failures
//!
//! Spawned from [`spawn_peer_diagnostic_task`]; period configurable via
//! `BSC_PEER_DIAG_PERIOD_SECS` (default 60s, 0 = disabled).
//!
//! Default reth logging only shows ban *transitions* at INFO; cumulative
//! reputation drift is at DEBUG and dial failures are at TRACE. So a node
//! whose peer count slowly drains away has no actionable signal at INFO.
//! This task surfaces both.

use crate::node::network::BscNetworkPrimitives;
use futures::StreamExt;
use parking_lot::Mutex;
use reth_eth_wire::DisconnectReason;
use reth_network::NetworkHandle;
use reth_network_api::{
    events::{NetworkEvent, NetworkEventListenerProvider, PeerEvent},
    BanSnapshot, PeerInfo, Peers, PeersInfo, ReputationChangeKind, ReputationChangeOutcome,
};
use reth_network_peers::PeerId;
use std::{
    collections::{HashMap, VecDeque},
    fmt::Display,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, Instant},
};
use tracing::{info, warn};

/// Hard cap on each per-tick ring buffer; bounds memory across ticks even
/// under event flood. Each tick drains the rings, so this only limits
/// in-flight burst size.
const RING_CAPACITY: usize = 256;

/// Per-tick cap on rendered entries from each ring (extras are dropped with
/// a `(N more dropped)` log line so the operator knows to lower the period).
const PER_TICK_RENDER_LIMIT: usize = 32;

/// Hard cap on cumulative dial-class bucket cardinality. Beyond this,
/// previously-unseen classes are bucketed as `_other_`. Bounds map size
/// against malicious peers spraying unique handshake error strings.
const MAX_DIAL_CLASS_BUCKETS: usize = 64;

#[derive(Debug, Clone)]
struct DisconnectRecord {
    at: Instant,
    peer_id: PeerId,
    reason: Option<DisconnectReason>,
}

#[derive(Debug, Clone)]
struct ReputationRecord {
    at: Instant,
    peer_id: PeerId,
    kind: ReputationChangeKind,
    new_reputation: i32,
    outcome: ReputationChangeOutcome,
}

#[derive(Debug, Clone)]
struct DialFailRecord {
    at: Instant,
    peer_id: PeerId,
    remote_addr: SocketAddr,
    error_class: Arc<str>,
    fatal: bool,
}

#[derive(Default)]
struct DiagState {
    disconnects: VecDeque<DisconnectRecord>,
    reputations: VecDeque<ReputationRecord>,
    dial_fails: VecDeque<DialFailRecord>,
    /// Cumulative counts since process start. `cum_kinds` is bounded by the
    /// number of `ReputationChangeKind` variants; `cum_dial_classes` is
    /// bounded by `MAX_DIAL_CLASS_BUCKETS` via [`Self::push_dial_fail`].
    cum_kinds: HashMap<&'static str, u64>,
    cum_dial_classes: HashMap<String, u64>,
}

impl DiagState {
    fn push_disconnect(&mut self, rec: DisconnectRecord) {
        push_capped(&mut self.disconnects, rec);
    }

    fn push_reputation(&mut self, rec: ReputationRecord) {
        *self.cum_kinds.entry(kind_label(&rec.kind)).or_insert(0) += 1;
        push_capped(&mut self.reputations, rec);
    }

    fn push_dial_fail(&mut self, rec: DialFailRecord) {
        let bucket = dial_class_bucket(&rec.error_class);
        let key = if self.cum_dial_classes.contains_key(bucket)
            || self.cum_dial_classes.len() < MAX_DIAL_CLASS_BUCKETS
        {
            bucket.to_string()
        } else {
            "_other_".to_string()
        };
        *self.cum_dial_classes.entry(key).or_insert(0) += 1;
        push_capped(&mut self.dial_fails, rec);
    }
}

fn push_capped<T>(buf: &mut VecDeque<T>, item: T) {
    if buf.len() == RING_CAPACITY {
        buf.pop_front();
    }
    buf.push_back(item);
}

/// Spawn the periodic diagnostic task.
pub fn spawn_peer_diagnostic_task(net: NetworkHandle<BscNetworkPrimitives>, period: Duration) {
    let state: Arc<Mutex<DiagState>> = Arc::new(Mutex::new(DiagState::default()));

    let state_for_events = state.clone();
    let net_for_events = net.clone();
    tokio::spawn(async move {
        let mut events = net_for_events.event_listener();
        while let Some(ev) = events.next().await {
            let NetworkEvent::Peer(peer_ev) = ev else { continue };
            let mut s = state_for_events.lock();
            match peer_ev {
                PeerEvent::SessionClosed { peer_id, reason } => {
                    s.push_disconnect(DisconnectRecord { at: Instant::now(), peer_id, reason });
                }
                PeerEvent::ReputationChanged { peer_id, kind, new_reputation, outcome } => {
                    s.push_reputation(ReputationRecord {
                        at: Instant::now(),
                        peer_id,
                        kind,
                        new_reputation,
                        outcome,
                    });
                }
                PeerEvent::DialFailed { peer_id, remote_addr, error_class, fatal } => {
                    s.push_dial_fail(DialFailRecord {
                        at: Instant::now(),
                        peer_id,
                        remote_addr,
                        error_class,
                        fatal,
                    });
                }
                // SessionEstablished / PeerAdded / PeerRemoved are observable
                // through the connected peer table, not buffered here.
                _ => {}
            }
        }
        warn!(target: "bsc::peers", "network event stream ended; diagnostic capture stopped");
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(period);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // tokio::time::interval fires immediately at t=0; advance once so the
        // first dump shows real activity.
        interval.tick().await;
        loop {
            interval.tick().await;
            dump_once(&net, &state).await;
        }
    });
}

async fn dump_once(net: &NetworkHandle<BscNetworkPrimitives>, state: &Arc<Mutex<DiagState>>) {
    let connected = net.num_connected_peers();
    let peers = match net.get_all_peers().await {
        Ok(v) => v,
        Err(err) => {
            warn!(target: "bsc::peers", %err, "get_all_peers failed");
            return;
        }
    };
    let ban_snapshot = match net.get_ban_snapshot().await {
        Ok(s) => s,
        Err(err) => {
            warn!(target: "bsc::peers", %err, "get_ban_snapshot failed");
            BanSnapshot::default()
        }
    };

    // Drain rings under the lock so the next tick sees a clean slate. This
    // is the single source of truth for "what arrived since last tick" —
    // a cursor pattern would silently lose events once the ring fills.
    let (disconnects, reputations, dial_fails, cum_kinds, cum_dial_classes) = {
        let mut s = state.lock();
        (
            std::mem::take(&mut s.disconnects),
            std::mem::take(&mut s.reputations),
            std::mem::take(&mut s.dial_fails),
            s.cum_kinds.clone(),
            s.cum_dial_classes.clone(),
        )
    };

    info!(
        target: "bsc::peers",
        connected = connected,
        reported = peers.len(),
        banned_peers = ban_snapshot.banned_peers.len(),
        banned_ips = ban_snapshot.banned_ips.len(),
        backed_off = ban_snapshot.backed_off.len(),
        rep_changes_total = cum_kinds.values().sum::<u64>(),
        dial_fails_total = cum_dial_classes.values().sum::<u64>(),
        "peer diagnostics summary"
    );

    if !cum_kinds.is_empty() {
        info!(target: "bsc::peers", "rep_change_totals {}", render_totals(&cum_kinds));
    }
    if !cum_dial_classes.is_empty() {
        info!(target: "bsc::peers", "dial_fail_totals {}", render_totals(&cum_dial_classes));
    }

    let now = Instant::now();
    for p in &peers {
        let line = render_peer(p, &fetch_reputation(net, p.remote_id).await, now);
        info!(target: "bsc::peers", "{line}");
    }

    for bp in &ban_snapshot.banned_peers {
        info!(
            target: "bsc::peers",
            "banned_peer {} until={}",
            short_peer(&bp.peer_id),
            render_until(bp.until, now)
        );
    }
    for bi in &ban_snapshot.banned_ips {
        info!(
            target: "bsc::peers",
            "banned_ip {} until={}",
            bi.ip,
            render_until(bi.until, now)
        );
    }
    for bo in &ban_snapshot.backed_off {
        info!(
            target: "bsc::peers",
            "backoff {} until={}",
            short_peer(&bo.peer_id),
            render_until(Some(bo.until), now)
        );
    }

    render_recent(&disconnects, "recent_disconnect", now, |rec, n| {
        format!(
            "ago={}s peer={} reason={}",
            n.saturating_duration_since(rec.at).as_secs(),
            short_peer(&rec.peer_id),
            rec.reason.map(|r| format!("{r:?}")).unwrap_or_else(|| "<none>".into())
        )
    });
    render_recent(&reputations, "recent_rep_change", now, |rec, n| {
        format!(
            "ago={}s peer={} kind={:?} new_rep={} outcome={:?}",
            n.saturating_duration_since(rec.at).as_secs(),
            short_peer(&rec.peer_id),
            rec.kind,
            rec.new_reputation,
            rec.outcome
        )
    });
    render_recent(&dial_fails, "recent_dial_fail", now, |rec, n| {
        format!(
            "ago={}s peer={} addr={} class={} fatal={}",
            n.saturating_duration_since(rec.at).as_secs(),
            short_peer(&rec.peer_id),
            rec.remote_addr,
            rec.error_class,
            rec.fatal
        )
    });
}

/// Print up to `PER_TICK_RENDER_LIMIT` of the *most recent* events from
/// `recs`; if more were drained, log a `(N more dropped)` line so the
/// operator knows the period needs to be lower.
fn render_recent<T, F>(recs: &VecDeque<T>, label: &str, now: Instant, render: F)
where
    F: Fn(&T, Instant) -> String,
{
    let total = recs.len();
    let skip = total.saturating_sub(PER_TICK_RENDER_LIMIT);
    for rec in recs.iter().skip(skip) {
        info!(target: "bsc::peers", "{label} {}", render(rec, now));
    }
    if skip > 0 {
        info!(target: "bsc::peers", "{label} ({skip} older entries dropped)");
    }
}

fn render_totals<K: Display + Ord>(map: &HashMap<K, u64>) -> String {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",")
}

async fn fetch_reputation(
    net: &NetworkHandle<BscNetworkPrimitives>,
    peer_id: PeerId,
) -> Option<i32> {
    net.reputation_by_id(peer_id).await.ok().flatten()
}

fn render_peer(p: &PeerInfo, reputation: &Option<i32>, now: Instant) -> String {
    let dir = if p.direction.is_incoming() { "in" } else { "out" };
    let age = now.saturating_duration_since(p.session_established);
    let fork = format!("{:08x}", u32::from_be_bytes(p.status.forkid.hash.0));
    let best = p.best_number.map(|n| n.to_string()).unwrap_or_else(|| "?".into());
    let td = p.best_td.map(|td| format!("{:#x}", td)).unwrap_or_else(|| "?".into());
    let rep = reputation.map(|r| r.to_string()).unwrap_or_else(|| "?".into());
    let client = trim_client(&p.client_version);
    format!(
        "peer {peer} addr={addr} dir={dir} kind={kind:?} eth={eth:?} fork={fork} best#={best} td={td} reput={rep} age={age}s client=\"{client}\"",
        peer = short_peer(&p.remote_id),
        addr = p.remote_addr,
        kind = p.kind,
        eth = p.eth_version,
        age = age.as_secs(),
    )
}

fn render_until(until: Option<Instant>, now: Instant) -> String {
    match until {
        None => "indef".to_string(),
        Some(t) if t > now => format!("in {}s", (t - now).as_secs()),
        Some(_) => "expired".to_string(),
    }
}

fn short_peer(peer: &PeerId) -> String {
    let bytes = peer.as_slice();
    let mut s = String::with_capacity(2 + 16 + 2);
    s.push_str("0x");
    for byte in &bytes[..bytes.len().min(8)] {
        use std::fmt::Write;
        let _ = write!(s, "{byte:02x}");
    }
    s.push_str("..");
    s
}

fn trim_client(v: &str) -> String {
    if v.len() <= 64 {
        v.to_string()
    } else {
        format!("{}…", &v[..63])
    }
}

/// Stable bucket label so cumulative counters don't fragment on
/// `Other(reputation)`, which embeds a numeric payload.
fn kind_label(kind: &ReputationChangeKind) -> &'static str {
    match kind {
        ReputationChangeKind::BadMessage => "BadMessage",
        ReputationChangeKind::BadBlock => "BadBlock",
        ReputationChangeKind::BadTransactions => "BadTransactions",
        ReputationChangeKind::AlreadySeenTransaction => "AlreadySeenTransaction",
        ReputationChangeKind::BadAnnouncement => "BadAnnouncement",
        ReputationChangeKind::Timeout => "Timeout",
        ReputationChangeKind::FailedToConnect => "FailedToConnect",
        ReputationChangeKind::Dropped => "Dropped",
        ReputationChangeKind::BadProtocol => "BadProtocol",
        ReputationChangeKind::Reset => "Reset",
        ReputationChangeKind::Other(_) => "Other",
    }
}

/// Coarse bucket key for `error_class` strings produced by reth's
/// `classify_*` helpers. The full string carries `<prefix>:<detail>`;
/// we bucket on the prefix so cardinality stays bounded.
fn dial_class_bucket(error_class: &str) -> &str {
    error_class.split_once(':').map(|(p, _)| p).unwrap_or(error_class)
}
