//! Completed-call metrics under `bsc.cas20`, including simulations and calls
//! later reverted by an outer frame. These are execution counts, not chain totals.

use super::{sigs::SELECTOR_NAMES, Kind, VARIANT_ASSET};
use metrics::{counter, histogram, Counter, Histogram};
use std::{
    sync::{Arc, OnceLock},
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
    /// Fixed index into `sigs::SELECTOR_NAMES`, including unknown and short calldata.
    pub selector: usize,
    pub status: CallStatus,
    pub gas_used: u64,
    pub elapsed: Duration,
    pub stats: CallStats,
}

/// Receives finished CAS20 calls. Implementations must be cheap and must not
/// panic: they run inside block execution.
pub trait Cas20Observer: Clone + Send + Sync + 'static {
    fn enabled(&self) -> bool {
        true
    }
    fn record_call(&self, call: &CallRecord);
}

/// Handles are registered before execution; the hot path only indexes arrays.
#[derive(Clone, Debug)]
pub(crate) struct MetricsObserver(Arc<Handles>);

#[derive(Debug)]
struct SelectorHandles {
    calls: [Counter; 4],
    gas: Histogram,
    duration: Histogram,
}

#[derive(Debug)]
struct KindHandles {
    selectors: Vec<SelectorHandles>,
    internal_calls: Counter,
    internal_bytes: Counter,
}

#[derive(Debug)]
struct Handles {
    kinds: [KindHandles; 5],
    sloads: Counter,
    sstores: Counter,
    keccaks: Counter,
    created_asset: Counter,
    created_stablecoin: Counter,
}

static METRICS: OnceLock<MetricsObserver> = OnceLock::new();

/// Called after the node's recorder is installed, when metrics export is enabled.
pub fn enable_metrics() {
    METRICS.get_or_init(MetricsObserver::register);
}

#[derive(Clone, Copy)]
pub(crate) struct NodeObserver;

impl Cas20Observer for NodeObserver {
    fn enabled(&self) -> bool {
        METRICS.get().is_some()
    }
    fn record_call(&self, call: &CallRecord) {
        if let Some(observer) = METRICS.get() {
            observer.record_call(call);
        }
    }
}

impl MetricsObserver {
    pub(crate) fn register() -> Self {
        Self(Arc::new(Handles {
            kinds: Kind::ALL.map(|kind| {
                let kind = kind.name();
                KindHandles {
                    selectors: SELECTOR_NAMES.iter().map(|&selector| SelectorHandles {
                        calls: [CallStatus::Return, CallStatus::Revert, CallStatus::OutOfGas, CallStatus::Fatal].map(|status|
                            counter!("bsc.cas20.calls_total", "kind" => kind, "selector" => selector, "status" => status.label())
                        ),
                        gas: histogram!("bsc.cas20.call_gas_used", "kind" => kind, "selector" => selector),
                        duration: histogram!("bsc.cas20.call_duration_seconds", "kind" => kind, "selector" => selector),
                    }).collect(),
                    internal_calls: counter!("bsc.cas20.internal_calls_total", "kind" => kind),
                    internal_bytes: counter!("bsc.cas20.internal_call_bytes_total", "kind" => kind),
                }
            }),
            sloads: counter!("bsc.cas20.storage_ops_total", "op" => "sload"),
            sstores: counter!("bsc.cas20.storage_ops_total", "op" => "sstore"),
            keccaks: counter!("bsc.cas20.storage_ops_total", "op" => "keccak"),
            created_asset: counter!("bsc.cas20.tokens_created_total", "variant" => "asset"),
            created_stablecoin: counter!("bsc.cas20.tokens_created_total", "variant" => "stablecoin"),
        }))
    }
}

impl Cas20Observer for MetricsObserver {
    fn record_call(&self, call: &CallRecord) {
        let h = &self.0;
        let kind = &h.kinds[call.kind as usize];
        let selector = &kind.selectors[call.selector];
        selector.calls[call.status as usize].increment(1);
        selector.gas.record(call.gas_used as f64);
        selector.duration.record(call.elapsed.as_secs_f64());
        let s = call.stats;
        h.sloads.increment(s.sloads as u64);
        h.sstores.increment(s.sstores as u64);
        h.keccaks.increment(s.keccaks as u64);
        if s.internal_calls > 0 {
            kind.internal_calls.increment(s.internal_calls as u64);
            kind.internal_bytes.increment(s.internal_call_bytes);
        }
        if let Some(variant) = s.created {
            if variant == VARIANT_ASSET { &h.created_asset } else { &h.created_stablecoin }
                .increment(1);
        }
    }
}

#[cfg(any(test, feature = "bench-test"))]
impl Cas20Observer for Option<MetricsObserver> {
    fn enabled(&self) -> bool {
        self.is_some()
    }
    fn record_call(&self, call: &CallRecord) {
        if let Some(observer) = self {
            observer.record_call(call);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metrics_exporter_prometheus::PrometheusBuilder;

    #[test]
    fn registered_handles_record_bounded_labels_and_internal_work() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let observer = metrics::with_local_recorder(&recorder, MetricsObserver::register);
        let mut call = CallRecord {
            kind: Kind::Asset,
            selector: super::super::sigs::selector_index(&super::super::sigs::SEL_TRANSFER),
            status: CallStatus::Return,
            gas_used: 123,
            elapsed: Duration::from_millis(1),
            stats: CallStats {
                sloads: 2,
                sstores: 1,
                internal_calls: 2,
                internal_call_bytes: 64,
                created: Some(VARIANT_ASSET),
                ..Default::default()
            },
        };
        observer.record_call(&call);
        call.selector = super::super::sigs::selector_index(&[0xde, 0xad, 0xbe, 0xef]);
        call.status = CallStatus::Revert;
        observer.record_call(&call);
        let output = recorder.handle().render();
        assert!(output.contains(
            "bsc_cas20_calls_total{kind=\"CAS20Asset\",selector=\"transfer\",status=\"return\"} 1"
        ));
        assert!(output.contains(
            "bsc_cas20_calls_total{kind=\"CAS20Asset\",selector=\"unknown\",status=\"revert\"} 1"
        ));
        assert!(output.contains("bsc_cas20_internal_calls_total{kind=\"CAS20Asset\"} 4"));
        assert!(output.contains("bsc_cas20_storage_ops_total{op=\"sload\"} 4"));
    }
}
