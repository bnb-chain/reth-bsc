use jsonrpsee::RpcModule;
use reth::rpc::api::IntoEngineApiRpcModule;
use reth_engine_primitives::ConsensusEngineHandle;
use std::sync::Arc;

#[cfg(feature = "bench-test")]
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
#[cfg(feature = "bench-test")]
use jsonrpsee_types::ErrorObjectOwned;
#[cfg(feature = "bench-test")]
use reth_payload_primitives::EngineApiMessageVersion;
#[cfg(feature = "bench-test")]
use reth_node_ethereum::engine::EthPayloadAttributes;
#[cfg(feature = "bench-test")]
use alloy_primitives::hex;
#[cfg(feature = "bench-test")]
use alloy_rlp::Decodable;
#[cfg(feature = "bench-test")]
use reth_ethereum_primitives::Block as EthBlock;

#[cfg(feature = "bench-test")]
use crate::node::{
    engine_api::validator::BscExecutionData,
    primitives::{BscBlock, BscBlockBody},
};


pub mod builder;
pub mod payload;
pub mod validator;

#[cfg(test)]
mod validator_tests;

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
        #[cfg(feature = "bench-test")]
        let mut module = RpcModule::new(());
        #[cfg(not(feature = "bench-test"))]
        let module = RpcModule::new(());

        // BSC has no production engine API, so this module is empty in normal
        // builds. These two methods exist only so `reth-bench-bsc` can drive the
        // node, and only under bench-test.
        #[cfg(feature = "bench-test")]
        {
            let fcu_handle = self.engine_handle.clone();
            let payload_handle = self.engine_handle.clone();

            module
                .register_async_method("engine_forkchoiceUpdatedV1", move |params, _, _| {
                    let engine_handle = fcu_handle.clone();

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

            // Accepts a whole block as 0x-prefixed RLP so the driver can push
            // blocks in, instead of the node having to fetch them over p2p.
            //
            // Deliberately NOT named `engine_newPayloadV1`: the parameter is a
            // consensus block, not a spec `ExecutionPayloadV1`, and it should not
            // be mistaken for the real thing. `bin/reth-bench` is a separate
            // workspace on a different reth rev and cannot depend on reth-bsc
            // types, so it sends an Ethereum-shaped block and the BSC wrapper is
            // attached on this side.
            //
            // Returns the `PayloadStatus` verbatim for every status, including
            // SYNCING and INVALID - the caller decides what counts as failure.
            // Only parse and engine-transport failures surface as RPC errors.
            module
                .register_async_method("engine_newPayloadBscV1", move |params, _, _| {
                    let engine_handle = payload_handle.clone();

                    async move {
                        let (block_rlp,): (String,) = params.parse().map_err(|e| {
                            ErrorObjectOwned::owned(
                                -32602,
                                format!("Parse error: {}", e),
                                None::<()>,
                            )
                        })?;

                        let raw = hex::decode(block_rlp.strip_prefix("0x").unwrap_or(&block_rlp))
                            .map_err(|e| {
                                ErrorObjectOwned::owned(
                                    -32602,
                                    format!("Invalid block hex: {}", e),
                                    None::<()>,
                                )
                            })?;

                        let eth_block = EthBlock::decode(&mut raw.as_slice()).map_err(|e| {
                            ErrorObjectOwned::owned(
                                -32602,
                                format!("Invalid block RLP: {}", e),
                                None::<()>,
                            )
                        })?;

                        // Sidecars are a data-availability concern and expire from
                        // the network - the p2p path already executes these blocks
                        // without them. Execution needs only the versioned hashes,
                        // which the blob transactions themselves carry.
                        let block = BscBlock {
                            header: eth_block.header,
                            body: BscBlockBody { inner: eth_block.body, sidecars: None },
                        };

                        engine_handle.new_payload(BscExecutionData::new(block)).await.map_err(
                            |err| {
                                ErrorObjectOwned::owned(
                                    -32603,
                                    format!("Engine new_payload error: {}", err),
                                    None::<()>,
                                )
                            },
                        )
                    }
                })
                .expect("Failed to register engine_newPayloadBscV1");
        }

        module
    }
}
