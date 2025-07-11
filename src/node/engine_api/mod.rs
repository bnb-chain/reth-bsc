use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use jsonrpsee_core::server::RpcModule;
use jsonrpsee_types::error::ErrorObjectOwned;
use reth::rpc::api::IntoEngineApiRpcModule;
use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_payload_primitives::EngineApiMessageVersion;
use std::sync::Arc;

use reth_node_ethereum::engine::EthPayloadAttributes;

pub mod builder;
pub mod payload;
pub mod validator;

#[derive(Debug, Clone)]
pub struct BscEngineApi {
    /// Handle to the beacon consensus engine
    engine_handle:
        Arc<BeaconConsensusEngineHandle<crate::node::engine_api::payload::BscPayloadTypes>>,
}

impl BscEngineApi {
    /// Create a new BSC Engine API instance
    pub fn new(
        engine_handle: Arc<
            BeaconConsensusEngineHandle<crate::node::engine_api::payload::BscPayloadTypes>,
        >,
    ) -> Self {
        Self { engine_handle }
    }
}

impl IntoEngineApiRpcModule for BscEngineApi {
    fn into_rpc_module(self) -> RpcModule<()> {
        let mut module = RpcModule::new(());

        // Register the fork choice update v1 method
        module
            .register_async_method("engine_forkchoiceUpdatedV1", move |params, _, _| {
                let engine_handle = self.engine_handle.clone();

                async move {
                    // Parse the parameters - ForkchoiceState and optional PayloadAttributes
                    let (forkchoice_state, payload_attrs): (
                        ForkchoiceState,
                        Option<EthPayloadAttributes>,
                    ) = params.parse().map_err(|e| {
                        ErrorObjectOwned::owned(-32602, format!("Parse error: {}", e), None::<()>)
                    })?;

                    let engine = engine_handle.clone();
                    // Call the engine service
                    match engine
                        .fork_choice_updated(
                            forkchoice_state,
                            payload_attrs,
                            EngineApiMessageVersion::V1,
                        )
                        .await
                    {
                        Ok(response) => match response.payload_status.status {
                            PayloadStatusEnum::Valid => Ok(response),
                            PayloadStatusEnum::Invalid { validation_error } => {
                                Err(ErrorObjectOwned::owned(
                                    -32603,
                                    format!("Engine error: {}", validation_error),
                                    None::<()>,
                                ))
                            }
                            _ => Err(ErrorObjectOwned::owned(
                                -32603,
                                format!("Engine status error: {}", response.payload_status.status),
                                None::<()>,
                            )),
                        },
                        Err(err) => Err(ErrorObjectOwned::owned(
                            -32603,
                            format!("Engine fork_choice_updated error: {}", err),
                            None::<()>,
                        )),
                    }
                }
            })
            .expect("Failed to register engine_forkchoiceUpdatedV1");

        module
    }
}
