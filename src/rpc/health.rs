//! Completed-request latency sampling for `eth_health`.

use jsonrpsee::core::{
    server::{MethodCallback, Methods},
    RegisterMethodError,
};
use parking_lot::Mutex;
use rand::{distr::Open01, Rng};
use reth::rpc::builder::TransportRpcModules;
use std::{
    cmp::{Ordering, Reverse},
    collections::BinaryHeap,
    sync::Arc,
    time::Duration,
};
use tokio::time::Instant;

/// Shared health state for the node's ordinary RPC methods on all public transports.
///
/// Matches Go-BSC's completed-request p75 rule, including failures and health probes.
/// Sampling is installed only when metrics are enabled at startup. Pending calls and
/// subscription lifetimes are not samples. An empty sample is healthy; this is not a sync
/// or readiness check.
#[derive(Clone, Debug, Default)]
pub struct RpcHealth {
    sample: Arc<Mutex<LatencySample>>,
}

impl RpcHealth {
    /// Whether the sampled 75th percentile is strictly below five seconds.
    /// Returns true with an empty sample, including when metrics are disabled.
    pub fn is_healthy(&self) -> bool {
        let mut values: Vec<_> = self.sample.lock().values.iter().map(|v| v.0.elapsed).collect();
        values.sort_unstable();
        // Go's percentile interpolation uses p * (n + 1), with one-based positions.
        let n = values.len();
        if n == 0 {
            return true;
        }
        let pos = 0.75 * (n + 1) as f64;
        let p75 = if pos >= n as f64 {
            values[n - 1].as_secs_f64()
        } else {
            let lower = values[pos as usize - 1].as_secs_f64();
            let upper = values[pos as usize].as_secs_f64();
            lower + pos.fract() * (upper - lower)
        };
        p75 < 5.0
    }

    fn record(&self, elapsed: Duration) {
        let uniform = rand::rng().sample(Open01);
        self.sample.lock().record(Instant::now(), elapsed, uniform);
    }

    /// Instrument each transport separately when metrics are enabled at startup.
    /// Call after registering all extensions and before starting the servers.
    ///
    /// With metrics disabled, the original callbacks are retained without timing wrappers.
    /// Wrapping callbacks also times individual batch entries and IPC requests, which do not
    /// all pass through the same server middleware hook. Subscriptions are left untouched.
    pub fn instrument(
        &self,
        modules: &mut TransportRpcModules,
        metrics_enabled: bool,
    ) -> Result<(), RegisterMethodError> {
        if !metrics_enabled {
            return Ok(());
        }
        if let Some(methods) = modules.http_methods(|_| true) {
            modules.replace_http(self.instrument_methods(&methods)?)?;
        }
        if let Some(methods) = modules.ws_methods(|_| true) {
            modules.replace_ws(self.instrument_methods(&methods)?)?;
        }
        if let Some(methods) = modules.ipc_methods(|_| true) {
            modules.replace_ipc(self.instrument_methods(&methods)?)?;
        }
        Ok(())
    }

    fn instrument_methods(&self, methods: &Methods) -> Result<Methods, RegisterMethodError> {
        let mut wrapped = Methods::new();
        for name in methods.method_names() {
            let callback =
                methods.method(name).expect("method name came from this registry").clone();
            let health = self.clone();
            let callback = match callback {
                MethodCallback::Sync(inner) => {
                    MethodCallback::Sync(Arc::new(move |id, params, limit, extensions| {
                        let start = Instant::now();
                        let response = inner(id, params, limit, extensions);
                        health.record(start.elapsed());
                        response
                    }))
                }
                MethodCallback::Async(inner) => {
                    MethodCallback::Async(Arc::new(move |id, params, conn, limit, extensions| {
                        let start = Instant::now();
                        let response = inner(id, params, conn, limit, extensions);
                        let health = health.clone();
                        Box::pin(async move {
                            let response = response.await;
                            health.record(start.elapsed());
                            response
                        })
                    }))
                }
                // Go-BSC's common method timer excludes subscribe/unsubscribe handling too.
                callback => callback,
            };
            wrapped.verify_and_insert(name, callback)?;
        }
        Ok(wrapped)
    }
}

// Go-BSC's timer uses a 1,028-entry exponentially decaying reservoir with alpha 0.015.
const SAMPLE_SIZE: usize = 1028;
const ALPHA: f64 = 0.015;

#[derive(Debug)]
struct LatencySample {
    origin: Instant,
    values: BinaryHeap<Reverse<Observation>>,
}

impl Default for LatencySample {
    fn default() -> Self {
        Self { origin: Instant::now(), values: BinaryHeap::with_capacity(SAMPLE_SIZE) }
    }
}

impl LatencySample {
    fn record(&mut self, now: Instant, elapsed: Duration, uniform: f64) {
        // Log priorities preserve exp(alpha * age) / U ordering without overflow or rescaling.
        let priority = ALPHA * now.duration_since(self.origin).as_secs_f64() - uniform.ln();
        // Match Go's reservoir: evict the lowest priority before inserting a new observation.
        if self.values.len() == SAMPLE_SIZE {
            self.values.pop();
        }
        self.values.push(Reverse(Observation { priority, elapsed }));
    }
}

#[derive(Debug)]
struct Observation {
    priority: f64,
    elapsed: Duration,
}

impl PartialEq for Observation {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other).is_eq()
    }
}

impl Eq for Observation {}

impl PartialOrd for Observation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Observation {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority.total_cmp(&other.priority)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonrpsee::{rpc_params, types::ErrorObjectOwned, RpcModule};

    #[test]
    fn threshold_and_go_percentile_interpolation() {
        assert!(RpcHealth::default().is_healthy());
        for (elapsed, expected) in [
            (Duration::from_secs(5) - Duration::from_nanos(1), true),
            (Duration::from_secs(5), false),
            (Duration::from_secs(5) + Duration::from_nanos(1), false),
        ] {
            let health = RpcHealth::default();
            health.record(elapsed);
            assert_eq!(health.is_healthy(), expected);
        }
        for (last, expected) in [(5, true), (6, false)] {
            let health = RpcHealth::default();
            for seconds in [1, 2, 3, last] {
                health.record(Duration::from_secs(seconds));
            }
            // p75 is 4.5 or 5.25, respectively, rather than a nearest-rank percentile.
            assert_eq!(health.is_healthy(), expected);
        }
        let health = RpcHealth::default();
        for _ in 0..100 {
            health.record(Duration::from_millis(1));
        }
        health.record(Duration::from_secs(20));
        assert!(health.is_healthy(), "one outlier must not act like a maximum-latency check");
    }

    #[tokio::test(start_paused = true)]
    async fn bounded_reservoir_recovers_with_new_samples_not_idle_time() {
        let health = RpcHealth::default();
        for _ in 0..SAMPLE_SIZE {
            health.sample.lock().record(Instant::now(), Duration::from_secs(6), 0.5);
        }
        assert!(!health.is_healthy());
        tokio::time::advance(Duration::from_secs(10_000)).await;
        assert!(!health.is_healthy(), "idle time alone does not remove Go's old observations");
        for _ in 0..SAMPLE_SIZE {
            health.sample.lock().record(Instant::now(), Duration::from_millis(1), 0.5);
        }
        assert!(health.is_healthy());
        assert_eq!(health.sample.lock().values.len(), SAMPLE_SIZE);
        assert!(health.sample.lock().values.iter().all(|v| v.0.priority.is_finite()));
    }

    #[tokio::test(start_paused = true)]
    async fn pending_calls_errors_health_probes_and_recovery() {
        let health = RpcHealth::default();
        let mut module = RpcModule::new(health.clone());
        module.register_method("eth_health", |_, health, _| health.is_healthy()).unwrap();
        let gate = Arc::new(tokio::sync::Notify::new());
        module
            .register_async_method("slow", {
                let gate = gate.clone();
                move |_, _, _| {
                    let gate = gate.clone();
                    async move {
                        gate.notified().await;
                        Err::<bool, _>(ErrorObjectOwned::owned(-32003, "out of gas", None::<()>))
                    }
                }
            })
            .unwrap();
        let mut wrapped = RpcModule::new(());
        wrapped.merge(health.instrument_methods(&module).unwrap()).unwrap();
        let wrapped = Arc::new(wrapped);
        let slow = tokio::spawn({
            let wrapped = wrapped.clone();
            async move { wrapped.call::<_, bool>("slow", rpc_params![]).await }
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(!slow.is_finished());
        assert!(wrapped.call::<_, bool>("eth_health", rpc_params![]).await.unwrap());
        assert_eq!(health.sample.lock().values.len(), 1, "pending call is not sampled");
        gate.notify_one();
        assert!(slow.await.unwrap().is_err());
        assert!(!health.is_healthy(), "completed errors contribute their duration");
        assert!(!wrapped.call::<_, bool>("eth_health", rpc_params![]).await.unwrap());
        assert_eq!(
            health.sample.lock().values.len(),
            3,
            "health probe is recorded after responding"
        );
        for _ in 0..10 {
            let _: bool = wrapped.call("eth_health", rpc_params![]).await.unwrap();
        }
        assert!(health.is_healthy(), "fast probes participate in recovery, as in Go-BSC");
    }

    #[tokio::test]
    async fn transport_registration_preserves_methods_and_samples_once() {
        let health = RpcHealth::default();
        let mut http = RpcModule::new(());
        http.register_method("same_name", |_, _, _| 1u64).unwrap();
        let mut ipc = RpcModule::new(());
        ipc.register_method("same_name", |_, _, _| 2u64).unwrap();
        ipc.register_method("admin_private", |_, _, _| true).unwrap();
        let mut ws = RpcModule::new(());
        ws.register_method("same_name", |_, _, _| 3u64).unwrap();
        let mut modules = TransportRpcModules::default().with_http(http).with_ipc(ipc).with_ws(ws);
        health.instrument(&mut modules, true).unwrap();
        let http = modules.http_methods(|_| true).unwrap();
        let ipc = modules.ipc_methods(|_| true).unwrap();
        let ws = modules.ws_methods(|_| true).unwrap();
        assert!(http.method("admin_private").is_none());
        assert!(ws.method("admin_private").is_none());
        assert_eq!(http.call::<_, u64>("same_name", rpc_params![]).await.unwrap(), 1);
        assert_eq!(ipc.call::<_, u64>("same_name", rpc_params![]).await.unwrap(), 2);
        assert_eq!(ws.call::<_, u64>("same_name", rpc_params![]).await.unwrap(), 3);
        assert_eq!(health.sample.lock().values.len(), 3);
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_metrics_retains_callbacks_and_does_not_sample_slow_calls() {
        let health = RpcHealth::default();
        let mut module = RpcModule::new(());
        module.register_method("sync", |_, _, _| 7u64).unwrap();
        module
            .register_async_method("slow", |_, _, _| async {
                tokio::time::sleep(Duration::from_secs(6)).await;
                Err::<u64, _>(ErrorObjectOwned::owned(-32003, "out of gas", None::<()>))
            })
            .unwrap();
        let mut modules = TransportRpcModules::default()
            .with_http(module.clone())
            .with_ws(module.clone())
            .with_ipc(module.clone());
        health.instrument(&mut modules, false).unwrap();
        for methods in [
            modules.http_methods(|_| true).unwrap(),
            modules.ws_methods(|_| true).unwrap(),
            modules.ipc_methods(|_| true).unwrap(),
        ] {
            // No wrapper or per-request enablement check should be added in disabled mode.
            match (module.method("sync").unwrap(), methods.method("sync").unwrap()) {
                (MethodCallback::Sync(original), MethodCallback::Sync(actual)) => {
                    assert!(Arc::ptr_eq(original, actual));
                }
                _ => panic!("synchronous callback changed kind"),
            }
            match (module.method("slow").unwrap(), methods.method("slow").unwrap()) {
                (MethodCallback::Async(original), MethodCallback::Async(actual)) => {
                    assert!(Arc::ptr_eq(original, actual));
                }
                _ => panic!("asynchronous callback changed kind"),
            }
            assert_eq!(methods.call::<_, u64>("sync", rpc_params![]).await.unwrap(), 7);
            assert!(methods.call::<_, u64>("slow", rpc_params![]).await.is_err());
        }
        assert!(health.sample.lock().values.is_empty());
        assert!(health.is_healthy());
    }

    #[tokio::test]
    async fn http_ws_ipc_and_batch_share_completed_call_samples() {
        use jsonrpsee::core::{client::ClientT, params::BatchRequestBuilder};
        use reth::rpc::builder::RpcServerConfig;

        let health = RpcHealth::default();
        let mut module = RpcModule::new(health.clone());
        module.register_method("eth_health", |_, health, _| health.is_healthy()).unwrap();
        module.register_method("sync", |_, _, _| 7u64).unwrap();
        module.register_async_method("async", |_, _, _| async { 9u64 }).unwrap();
        module
            .register_method("failure", |_, _, _| {
                Err::<u64, _>(ErrorObjectOwned::owned(-32003, "out of gas", None::<()>))
            })
            .unwrap();
        let module = module.remove_context();
        let mut modules = TransportRpcModules::default()
            .with_http(module.clone())
            .with_ws(module.clone())
            .with_ipc(module);
        health.instrument(&mut modules, true).unwrap();
        let path = std::env::temp_dir().join(format!("rpc-health-{}.ipc", uuid::Uuid::new_v4()));
        let endpoint = path.to_str().unwrap().to_owned();
        let server = RpcServerConfig::http(Default::default())
            .with_http_address("127.0.0.1:0".parse().unwrap())
            .with_ws(Default::default())
            // Exercise HTTP and WS sharing a listener, as well as the independent IPC path.
            .with_ws_address("127.0.0.1:0".parse().unwrap())
            .with_ipc(Default::default())
            .with_ipc_endpoint(endpoint.clone())
            .start(&modules)
            .await
            .unwrap();
        let http = server.http_client().unwrap();
        let ws = server.ws_client().await.unwrap();
        let ipc = reth_ipc::client::IpcClientBuilder::default().build(&endpoint).await.unwrap();
        assert_eq!(http.request::<u64, _>("sync", rpc_params![]).await.unwrap(), 7);
        assert_eq!(ws.request::<u64, _>("async", rpc_params![]).await.unwrap(), 9);
        assert!(ipc.request::<u64, _>("failure", rpc_params![]).await.is_err());
        assert_eq!(health.sample.lock().values.len(), 3);

        let mut batch = BatchRequestBuilder::new();
        batch.insert("sync", rpc_params![]).unwrap();
        batch.insert("async", rpc_params![]).unwrap();
        batch.insert("failure", rpc_params![]).unwrap();
        let response = http.batch_request::<u64>(batch).await.unwrap();
        assert_eq!(response.num_successful_calls(), 2);
        assert_eq!(response.num_failed_calls(), 1);
        assert_eq!(health.sample.lock().values.len(), 6, "each batch entry is sampled once");

        for _ in 0..20 {
            health.record(Duration::from_secs(6));
        }
        assert!(!http.request::<bool, _>("eth_health", rpc_params![]).await.unwrap());
        assert!(!ws.request::<bool, _>("eth_health", rpc_params![]).await.unwrap());
        assert!(!ipc.request::<bool, _>("eth_health", rpc_params![]).await.unwrap());
        server.stop().unwrap();
        let _ = std::fs::remove_file(path);
    }
}
