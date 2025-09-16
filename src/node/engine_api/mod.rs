use jsonrpsee::RpcModule;
use reth::rpc::api::IntoEngineApiRpcModule;
use reth_engine_primitives::ConsensusEngineHandle;
use std::sync::Arc;


pub mod builder;
pub mod payload;
pub mod validator;

#[derive(Debug, Clone)]
pub struct BscEngineApi {
    /// Handle to the beacon consensus engine
    #[allow(dead_code)]
    engine_handle:
        Arc<ConsensusEngineHandle<crate::node::engine_api::payload::BscPayloadTypes>>,
}

impl BscEngineApi {
    /// Create a new BSC Engine API instance
    pub fn new(
        engine_handle: Arc<
            ConsensusEngineHandle<crate::node::engine_api::payload::BscPayloadTypes>,
        >,
    ) -> Self {
        Self { engine_handle }
    }
}

impl IntoEngineApiRpcModule for BscEngineApi {
    fn into_rpc_module(self) -> RpcModule<()> {
        // Create RPC module compatible with the expected return type
        let module = RpcModule::new(());
        
        // Note: bench-test feature support has been simplified for compatibility
        #[cfg(feature = "bench-test")]
        {
            // TODO: Re-implement engine_forkchoiceUpdatedV1 when jsonrpsee compatibility is resolved
            tracing::warn!("bench-test feature temporarily disabled due to jsonrpsee compatibility issues");
        }

        module
    }
}
