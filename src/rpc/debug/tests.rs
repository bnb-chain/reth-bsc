use super::*;
use jsonrpsee::{
    core::{client::ClientT, params::BatchRequestBuilder, ClientError},
    rpc_params, RpcModule,
};
use reth::rpc::builder::RpcServerConfig;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

fn debug_module(calls: Arc<AtomicUsize>) -> RpcModule<()> {
    let mut module = RpcModule::new(calls);
    for method in ["debug_traceBlock", "debug_traceBadBlock"] {
        module
            .register_method(method, |_, calls, _| {
                calls.fetch_add(1, Ordering::SeqCst);
                true
            })
            .unwrap();
    }
    for method in [
        "debug_traceBlockByHash",
        "debug_traceBlockByNumber",
        "debug_traceTransaction",
        "debug_traceCall",
        "debug_traceCallMany",
        "debug_getRawBlock",
        "eth_blockNumber",
    ] {
        module.register_method(method, |_, _, _| true).unwrap();
    }
    module.remove_context()
}

#[test]
fn restrictions_preserve_transport_configuration_and_unrelated_methods() {
    for mask in 0..8 {
        let module = debug_module(Arc::new(AtomicUsize::new(0)));
        let mut modules = TransportRpcModules::default();
        if mask & 1 != 0 {
            modules = modules.with_http(module.clone());
        }
        if mask & 2 != 0 {
            modules = modules.with_ws(module.clone());
        }
        if mask & 4 != 0 {
            modules = modules.with_ipc(module);
        }
        restrict_block_tracing(&mut modules);
        // Applying the restriction twice must also work when Debug or a transport is absent.
        restrict_block_tracing(&mut modules);
        for (bit, methods) in [
            (1, modules.http_methods(|_| true)),
            (2, modules.ws_methods(|_| true)),
            (4, modules.ipc_methods(|_| true)),
        ] {
            assert_eq!(methods.is_some(), mask & bit != 0);
            let Some(methods) = methods else { continue };
            for name in ["debug_traceBlock", "debug_traceBadBlock"] {
                assert_eq!(methods.method(name).is_some(), bit == 4, "mask={mask}, {name}");
            }
            for name in [
                "debug_traceBlockByHash",
                "debug_traceBlockByNumber",
                "debug_traceTransaction",
                "debug_traceCall",
                "debug_traceCallMany",
                "debug_getRawBlock",
                "eth_blockNumber",
            ] {
                assert!(methods.method(name).is_some(), "mask={mask}, {name}");
            }
        }
    }
}

fn assert_method_not_found(error: ClientError) {
    match error {
        ClientError::Call(error) => assert_eq!(error.code(), -32601),
        other => panic!("expected method not found, got {other}"),
    }
}

#[tokio::test]
async fn http_ws_and_batches_cannot_dispatch_restricted_traces_but_ipc_can() {
    let calls = Arc::new(AtomicUsize::new(0));
    let module = debug_module(calls.clone());
    let mut modules = TransportRpcModules::default()
        .with_http(module.clone())
        .with_ws(module.clone())
        .with_ipc(module);
    restrict_block_tracing(&mut modules);
    // Health instrumentation runs after restrictions in the node startup hook.
    crate::rpc::health::RpcHealth::default().instrument(&mut modules, true).unwrap();
    // macOS's temporary directory already consumes much of the Unix socket path limit.
    let path = std::env::temp_dir().join(format!("trace-{:016x}.ipc", rand::random::<u64>()));
    let endpoint = path.to_str().unwrap().to_owned();
    let server = RpcServerConfig::http(Default::default())
        .with_http_address("127.0.0.1:0".parse().unwrap())
        .with_ws(Default::default())
        // Exercise HTTP/WS sharing a listener in addition to independent IPC.
        .with_ws_address("127.0.0.1:0".parse().unwrap())
        .with_ipc(Default::default())
        .with_ipc_endpoint(endpoint.clone())
        .start(&modules)
        .await
        .unwrap();
    let http = server.http_client().unwrap();
    let ws = server.ws_client().await.unwrap();
    let ipc = reth_ipc::client::IpcClientBuilder::default().build(&endpoint).await.unwrap();
    for name in ["debug_traceBlock", "debug_traceBadBlock"] {
        assert_method_not_found(http.request::<bool, _>(name, rpc_params![]).await.unwrap_err());
        assert_method_not_found(ws.request::<bool, _>(name, rpc_params![]).await.unwrap_err());
    }
    for name in ["debug_traceBlockByHash", "debug_traceBlockByNumber", "debug_traceCall"] {
        assert!(http.request::<bool, _>(name, rpc_params![]).await.unwrap());
        assert!(ws.request::<bool, _>(name, rpc_params![]).await.unwrap());
    }
    let mut batch = BatchRequestBuilder::new();
    batch.insert("debug_traceBlock", rpc_params![]).unwrap();
    batch.insert("debug_traceBadBlock", rpc_params![]).unwrap();
    batch.insert("eth_blockNumber", rpc_params![]).unwrap();
    let http_results = http.batch_request::<bool>(batch.clone()).await.unwrap();
    let ws_results = ws.batch_request::<bool>(batch).await.unwrap();
    for results in [http_results, ws_results] {
        assert_eq!(results.num_successful_calls(), 1);
        assert_eq!(results.num_failed_calls(), 2);
        for result in results.into_iter().take(2) {
            assert_eq!(result.unwrap_err().code(), -32601);
        }
    }
    assert_eq!(calls.load(Ordering::SeqCst), 0, "network requests reached a restricted handler");
    for name in ["debug_traceBlock", "debug_traceBadBlock"] {
        assert!(ipc.request::<bool, _>(name, rpc_params![]).await.unwrap());
    }
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    server.stop().unwrap();
    let _ = std::fs::remove_file(path);
}
