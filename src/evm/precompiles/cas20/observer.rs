//! Observation of CAS20 calls: an observer is told about each call once it has
//! finished, and about the work it did, without ever touching gas, state or
//! control flow. The default observer does nothing and costs nothing; the
//! metrics observer exports the calls to Prometheus under `bsc.cas20`.

use super::{Kind, VARIANT_ASSET};
use metrics::{counter, histogram, Counter, Histogram};
use std::{
    collections::HashMap,
    hash::Hash,
    sync::{LazyLock, RwLock},
    time::Duration,
};

/// How a call ended, as the EVM sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallStatus {
    Return,
    Revert,
    OutOfGas,
    Fatal,
}

impl CallStatus {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Return => "return",
            Self::Revert => "revert",
            Self::OutOfGas => "out_of_gas",
            Self::Fatal => "fatal",
        }
    }
}

/// What one call did. Counted during execution, reported once at the end.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CallStats {
    pub sloads: u32,
    pub sstores: u32,
    /// Hashes paid for, not computed: a permit prepays several before hashing.
    pub keccaks: u32,
    /// Entries of announce bundles and initCalls arrays dispatched.
    pub internal_calls: u32,
    pub internal_call_bytes: u64,
    /// The variant of a token this call created, if it created one.
    pub created: Option<u8>,
}

/// One finished call.
#[derive(Clone, Copy, Debug)]
pub struct CallRecord {
    pub kind: Kind,
    /// The function's name, `"unknown"` for a selector no table has, `"short"`
    /// for calldata under four bytes.
    pub selector: &'static str,
    pub status: CallStatus,
    pub gas_used: u64,
    /// Present only when the observer asked for timing.
    pub elapsed: Option<Duration>,
    pub stats: CallStats,
}

/// Receives finished CAS20 calls. Implementations must be cheap and must not
/// panic: they run inside block execution.
pub trait Cas20Observer: Clone + Send + Sync + 'static {
    /// False for an observer that ignores everything, so the entry point skips
    /// the clock and the labelling.
    const ENABLED: bool = true;

    fn record_call(&self, _call: &CallRecord) {}
}

/// The observer that records nothing.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopObserver;

impl Cas20Observer for NoopObserver {
    const ENABLED: bool = false;
}

/// Exports every call to Prometheus. The label space is bounded (five kinds, the
/// selector table, four statuses), so every handle is registered once and kept:
/// the recorder's per-call registry lookup and label allocation are paid only
/// the first time a combination is seen.
#[derive(Clone, Copy, Debug, Default)]
pub struct MetricsObserver;

type Labels = (&'static str, &'static str, &'static str);
type GasHandles = (Histogram, Histogram);

struct Handles {
    calls: RwLock<HashMap<Labels, Counter>>,
    gas: RwLock<HashMap<(&'static str, &'static str), GasHandles>>,
    internal_calls: RwLock<HashMap<&'static str, (Counter, Counter)>>,
    sloads: Counter,
    sstores: Counter,
    keccaks: Counter,
    created_asset: Counter,
    created_stablecoin: Counter,
}

static HANDLES: LazyLock<Handles> = LazyLock::new(|| Handles {
    calls: RwLock::default(),
    gas: RwLock::default(),
    internal_calls: RwLock::default(),
    sloads: counter!("bsc.cas20.storage_ops_total", "op" => "sload"),
    sstores: counter!("bsc.cas20.storage_ops_total", "op" => "sstore"),
    keccaks: counter!("bsc.cas20.storage_ops_total", "op" => "keccak"),
    created_asset: counter!("bsc.cas20.tokens_created_total", "variant" => "asset"),
    created_stablecoin: counter!("bsc.cas20.tokens_created_total", "variant" => "stablecoin"),
});

/// Looks a handle up under the read lock, registering it under the write lock the
/// first time only.
fn cached<K: Copy + Eq + Hash, V: Clone>(
    map: &RwLock<HashMap<K, V>>,
    key: K,
    make: impl FnOnce() -> V,
) -> V {
    if let Some(v) = map.read().unwrap().get(&key) {
        return v.clone();
    }
    map.write().unwrap().entry(key).or_insert_with(make).clone()
}

impl Cas20Observer for MetricsObserver {
    fn record_call(&self, call: &CallRecord) {
        let (kind, selector) = (call.kind.name(), call.selector);
        let h = &*HANDLES;
        cached(&h.calls, (kind, selector, call.status.label()), || {
            counter!("bsc.cas20.calls_total", "kind" => kind, "selector" => selector, "status" => call.status.label())
        })
        .increment(1);
        let (gas, duration) = cached(&h.gas, (kind, selector), || {
            (
                histogram!("bsc.cas20.call_gas_used", "kind" => kind, "selector" => selector),
                histogram!("bsc.cas20.call_duration_seconds", "kind" => kind, "selector" => selector),
            )
        });
        gas.record(call.gas_used as f64);
        if let Some(elapsed) = call.elapsed {
            duration.record(elapsed.as_secs_f64());
        }
        let s = call.stats;
        h.sloads.increment(s.sloads as u64);
        h.sstores.increment(s.sstores as u64);
        h.keccaks.increment(s.keccaks as u64);
        if s.internal_calls > 0 {
            let (calls, bytes) = cached(&h.internal_calls, kind, || {
                (
                    counter!("bsc.cas20.internal_calls_total", "kind" => kind),
                    counter!("bsc.cas20.internal_call_bytes_total", "kind" => kind),
                )
            });
            calls.increment(s.internal_calls as u64);
            bytes.increment(s.internal_call_bytes);
        }
        if let Some(variant) = s.created {
            if variant == VARIANT_ASSET { &h.created_asset } else { &h.created_stablecoin }
                .increment(1);
        }
    }
}
