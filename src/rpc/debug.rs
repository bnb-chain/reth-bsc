//! Transport restrictions for tracing blocks that have not passed consensus validation.

use reth::rpc::builder::TransportRpcModules;

/// Keep raw-block and bad-block tracing on IPC until their execution resource bounds are fixed.
///
/// Call after registering all RPC extensions and before starting the servers. Restrict every
/// HTTP/WS binding, including loopback, because a reverse proxy can expose a loopback listener.
pub fn restrict_block_tracing(modules: &mut TransportRpcModules) {
    for method in ["debug_traceBlock", "debug_traceBadBlock"] {
        modules.remove_http_method(method);
        modules.remove_ws_method(method);
    }
}

#[cfg(test)]
mod tests;
