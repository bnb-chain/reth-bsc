//! CAS20 prestate compatibility. Reth already observes native journal accesses;
//! only its empty-account classification needs adjustment. Delegate execution to
//! DebugApi and normalize its output without changing journal flags or state.

use crate::evm::precompiles::cas20::is_cas20_precompile;
use alloy_eips::{BlockId, BlockNumberOrTag};
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types::{
    eth::{Bundle, StateContext, TransactionRequest},
    trace::geth::{
        mux::MuxConfig, AccountState, GethDebugBuiltInTracerType, GethDebugTracerConfig,
        GethDebugTracerType, GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace,
        PreStateFrame, TraceResult,
    },
};
use jsonrpsee::{core::RpcResult, proc_macros::rpc};
use reth::rpc::api::DebugApiServer;

pub const METHODS: &[&str] = &[
    "debug_traceBlock",
    "debug_traceBlockByHash",
    "debug_traceBlockByNumber",
    "debug_traceTransaction",
    "debug_traceCall",
    "debug_traceCallMany",
    "debug_traceBadBlock",
];

#[rpc(server, namespace = "debug")]
pub trait BscPrestateApi {
    #[method(name = "traceBlock")]
    async fn debug_trace_block(
        &self,
        rlp_block: Bytes,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;
    #[method(name = "traceBlockByHash")]
    async fn debug_trace_block_by_hash(
        &self,
        block: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;
    #[method(name = "traceBlockByNumber")]
    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>>;
    #[method(name = "traceTransaction")]
    async fn debug_trace_transaction(
        &self,
        tx_hash: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace>;
    #[method(name = "traceCall")]
    async fn debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace>;
    #[method(name = "traceCallMany")]
    async fn debug_trace_call_many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<Vec<GethTrace>>>;
    #[method(name = "traceBadBlock")]
    async fn debug_trace_bad_block(
        &self,
        block_hash: B256,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<TraceResult>>;
}

pub struct BscPrestateApiImpl<Debug>(pub Debug);

#[async_trait::async_trait]
impl<Debug: DebugApiServer<TransactionRequest> + 'static> BscPrestateApiServer
    for BscPrestateApiImpl<Debug>
{
    async fn debug_trace_block(
        &self,
        rlp_block: Bytes,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts);
        let mut result = self.0.debug_trace_block(rlp_block, Some(opts)).await?;
        for trace in &mut result {
            if let TraceResult::Success { result, .. } = trace {
                plan.apply(result);
            }
        }
        Ok(result)
    }
    async fn debug_trace_block_by_hash(
        &self,
        block: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts);
        let mut result = self.0.debug_trace_block_by_hash(block, Some(opts)).await?;
        for trace in &mut result {
            if let TraceResult::Success { result, .. } = trace {
                plan.apply(result);
            }
        }
        Ok(result)
    }
    async fn debug_trace_block_by_number(
        &self,
        block: BlockNumberOrTag,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts);
        let mut result = self.0.debug_trace_block_by_number(block, Some(opts)).await?;
        for trace in &mut result {
            if let TraceResult::Success { result, .. } = trace {
                plan.apply(result);
            }
        }
        Ok(result)
    }
    async fn debug_trace_transaction(
        &self,
        tx_hash: B256,
        opts: Option<GethDebugTracingOptions>,
    ) -> RpcResult<GethTrace> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts);
        let mut result = self.0.debug_trace_transaction(tx_hash, Some(opts)).await?;
        plan.apply(&mut result);
        Ok(result)
    }
    async fn debug_trace_call(
        &self,
        request: TransactionRequest,
        block_id: Option<BlockId>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<GethTrace> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts.tracing_options);
        let mut result = self.0.debug_trace_call(request, block_id, Some(opts)).await?;
        plan.apply(&mut result);
        Ok(result)
    }
    async fn debug_trace_call_many(
        &self,
        bundles: Vec<Bundle<TransactionRequest>>,
        state_context: Option<StateContext>,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<Vec<GethTrace>>> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts.tracing_options);
        let mut result = self.0.debug_trace_call_many(bundles, state_context, Some(opts)).await?;
        for trace in result.iter_mut().flatten() {
            plan.apply(trace);
        }
        Ok(result)
    }
    async fn debug_trace_bad_block(
        &self,
        block_hash: B256,
        opts: Option<GethDebugTracingCallOptions>,
    ) -> RpcResult<Vec<TraceResult>> {
        let mut opts = opts.unwrap_or_default();
        let plan = Plan::prepare(&mut opts.tracing_options);
        let mut result = self.0.debug_trace_bad_block(block_hash, Some(opts)).await?;
        for trace in &mut result {
            if let TraceResult::Success { result, .. } = trace {
                plan.apply(result);
            }
        }
        Ok(result)
    }
}

#[derive(Default)]
enum Plan {
    #[default]
    None,
    Prestate {
        hide_code: bool,
        include_empty: bool,
    },
    Mux(Vec<(GethDebugTracerType, Plan)>),
}

impl Plan {
    fn prepare(opts: &mut GethDebugTracingOptions) -> Self {
        opts.tracer
            .as_ref()
            .map(|tracer| Self::prepare_config(tracer, &mut opts.tracer_config))
            .unwrap_or_default()
    }

    fn prepare_config(tracer: &GethDebugTracerType, config: &mut GethDebugTracerConfig) -> Self {
        match tracer {
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::PreStateTracer) => {
                if config.0.is_null() {
                    return Self::Prestate { hide_code: false, include_empty: false };
                }
                let Some(options) = config.0.as_object_mut() else { return Self::None };
                let mut include_empty = false;
                let mut hide_code = false;
                for (key, value) in options.iter() {
                    if ["includeEmpty", "disableCode", "disableStorage", "diffMode"]
                        .iter()
                        .any(|field| key.eq_ignore_ascii_case(field)) &&
                        !value.is_boolean() &&
                        !value.is_null()
                    {
                        return Self::None
                    }
                    if let Some(value) = value.as_bool() {
                        if key.eq_ignore_ascii_case("includeEmpty") {
                            include_empty = value;
                        }
                        if key.eq_ignore_ascii_case("disableCode") {
                            hide_code = value;
                        }
                    }
                }
                // Go checks account emptiness before applying disableCode. Preserve the original
                // field spelling for Reth's validation, and fetch code before filtering empties.
                if hide_code {
                    for (key, value) in options.iter_mut() {
                        if key.eq_ignore_ascii_case("disableCode") && value.is_boolean() {
                            *value = false.into();
                        }
                    }
                }
                Self::Prestate { hide_code, include_empty }
            }
            GethDebugTracerType::BuiltInTracer(GethDebugBuiltInTracerType::MuxTracer) => {
                let Ok(mut mux) = serde_json::from_value::<MuxConfig>(config.0.clone()) else {
                    return Self::None;
                };
                let plans = mux
                    .0
                    .iter_mut()
                    .map(|(tracer, config)| {
                        let plan = Self::prepare_config(
                            tracer,
                            config.get_or_insert_with(Default::default),
                        );
                        (tracer.clone(), plan)
                    })
                    .collect();
                config.0 = serde_json::to_value(mux).expect("serializable tracer configuration");
                Self::Mux(plans)
            }
            _ => Self::None,
        }
    }

    fn apply(&self, trace: &mut GethTrace) {
        match (self, trace) {
            (Self::Prestate { hide_code, include_empty }, GethTrace::PreStateTracer(frame)) => {
                let pre = match frame {
                    PreStateFrame::Default(pre) => &mut pre.0,
                    PreStateFrame::Diff(diff) => &mut diff.pre,
                };
                if !include_empty {
                    pre.retain(|address, state| {
                        !is_cas20_precompile(*address) || !empty_account(state)
                    });
                }
                if *hide_code {
                    for account in pre.values_mut() {
                        account.code = None;
                    }
                    if let PreStateFrame::Diff(diff) = frame {
                        for account in diff.post.values_mut() {
                            account.code = None;
                        }
                    }
                }
            }
            (Self::Mux(plans), GethTrace::MuxTracer(frame)) => {
                for (tracer, plan) in plans {
                    if let Some(trace) = frame.0.get_mut(tracer) {
                        plan.apply(trace);
                    }
                }
            }
            _ => {}
        }
    }
}

fn empty_account(state: &AccountState) -> bool {
    state.balance.is_none_or(|balance| balance.is_zero()) &&
        state.nonce.unwrap_or_default() == 0 &&
        state.code.as_ref().is_none_or(|code| code.is_empty())
}

#[cfg(test)]
mod tests;
