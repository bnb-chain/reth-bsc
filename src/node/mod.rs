use crate::{
    BscBlock, BscBlockBody, chainspec::BscChainSpec, node::{
        engine_api::{
            builder::BscEngineApiBuilder,
            payload::BscPayloadTypes,
            validator::{BscEngineValidatorBuilder, BscPayloadValidatorBuilder},
        }, pool::BscPoolBuilder, primitives::BscPrimitives, storage::BscStorage
    }
};
use consensus::BscConsensusBuilder;
use engine::BscPayloadServiceBuilder;
use evm::BscExecutorBuilder;
use network::BscNetworkBuilder;
use reth::{
    api::{FullNodeComponents, FullNodeTypes, HeaderTy, NodeTypes, PrimitivesTy},
    builder::rpc::EthApiCtx,
    builder::{
        components::ComponentsBuilder,
        rpc::{EthApiBuilder, RpcAddOns, RpcContext},
        DebugNode, Node, NodeAdapter,
    },
    rpc::eth::core::{EthApiFor, EthRpcConverterFor},
};
use reth_chainspec::{EthereumHardforks, Hardforks};
use reth_evm::ConfigureEvm;
use reth::rpc::server_types::eth::EthApiError;
use reth_rpc_eth_api::{helpers::pending_block::BuildPendingEnv, RpcConvert, FromEvmError};
use reth_engine_local::LocalPayloadAttributesBuilder;
use reth_engine_primitives::ConsensusEngineHandle;

use reth_payload_primitives::{PayloadAttributesBuilder, PayloadTypes};
use reth_primitives::BlockBody;
use reth_rpc_eth_api::helpers::config::{EthConfigApiServer, EthConfigHandler};
use reth_rpc_server_types::RethRpcModule;
use std::sync::Arc;
use tokio::sync::{oneshot, Mutex};
use tracing::trace;

pub mod consensus;
pub mod engine;
pub mod engine_api;
pub mod evm;
pub mod pool;
pub mod miner;
pub mod network;
pub mod primitives;
pub mod storage;
pub mod vote_producer;
pub mod vote_journal;

/// Bsc addons configuring RPC types
pub type BscNodeAddOns<N> = RpcAddOns<
    N,
    BscEthApiBuilder,
    BscPayloadValidatorBuilder,
    BscEngineApiBuilder,
    BscEngineValidatorBuilder,
>;

#[derive(Debug, Default)]
pub struct BscEthApiBuilder;

impl<N> EthApiBuilder<N> for BscEthApiBuilder
where
    N: FullNodeComponents<
        Types: NodeTypes<ChainSpec: Hardforks + EthereumHardforks>,
        Evm: ConfigureEvm<NextBlockEnvCtx: BuildPendingEnv<HeaderTy<N::Types>>>,
    >,
    EthRpcConverterFor<N>: RpcConvert<
        Primitives = PrimitivesTy<N::Types>,
        Error = EthApiError,
        Evm = N::Evm,
    >,
    EthApiError: FromEvmError<N::Evm>,
{
    type EthApi = EthApiFor<N>;

    async fn build_eth_api(self, ctx: EthApiCtx<'_, N>) -> eyre::Result<Self::EthApi> {
        let eth_api = ctx
            .eth_api_builder()
            .with_current_validators_len(move || {
                let count = (|| {
                    let best_block = crate::shared::get_best_canonical_block_number()?;
                    let header = crate::shared::get_canonical_header_by_number(best_block)?;
                    let snapshot_provider = crate::shared::get_snapshot_provider()?;
                    Some(
                        snapshot_provider
                            .snapshot_by_hash(&header.hash_slow())?
                            .validators
                            .len(),
                    )
                })();

                if count.is_none() {
                    trace!(target: "rpc::eth", "validator count unavailable for finalized-header callbacks");
                }

                count
            })
            .map_converter(|r| r.with_network())
            .build();

        Ok(eth_api)
    }
}

/// Type configuration for a regular BSC node.
#[derive(Debug, Clone)]
pub struct BscNode {
    engine_handle_rx:
        Arc<Mutex<Option<oneshot::Receiver<ConsensusEngineHandle<BscPayloadTypes>>>>>,
}

impl BscNode {
    pub fn new() -> (Self, oneshot::Sender<ConsensusEngineHandle<BscPayloadTypes>>) {
        let (tx, rx) = oneshot::channel();
        (Self { engine_handle_rx: Arc::new(Mutex::new(Some(rx))) }, tx)
    }
}

impl Default for BscNode {
    fn default() -> Self {
        let (node, _tx) = Self::new();
        node
    }
}

impl BscNode {
    /// Returns a [`ComponentsBuilder`] configured for a regular BSC node.
    pub fn components<Node>(
        &self,
    ) -> ComponentsBuilder<
        Node,
        BscPoolBuilder,
        BscPayloadServiceBuilder,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >
    where
        Node: FullNodeTypes<Types = Self>,
    {
        ComponentsBuilder::default()
            .node_types::<Node>()
            .pool(BscPoolBuilder::default())
            .executor(BscExecutorBuilder::default())
            .payload(BscPayloadServiceBuilder::default())
            .network(BscNetworkBuilder::new(self.engine_handle_rx.clone()))
            .consensus(BscConsensusBuilder::default())  
    }
}

impl NodeTypes for BscNode {
    type Primitives = BscPrimitives;
    type ChainSpec = BscChainSpec;
    type Storage = BscStorage;
    type Payload = BscPayloadTypes;
}

impl<N> Node<N> for BscNode
where
    N: FullNodeTypes<Types = Self>,
{
    type ComponentsBuilder = ComponentsBuilder<
        N,
        BscPoolBuilder,
        BscPayloadServiceBuilder,
        BscNetworkBuilder,
        BscExecutorBuilder,
        BscConsensusBuilder,
    >;

    type AddOns = BscNodeAddOns<NodeAdapter<N>>;

    fn components_builder(&self) -> Self::ComponentsBuilder {
        self.components()
    }

    fn add_ons(&self) -> Self::AddOns {
        BscNodeAddOns::default().extend_rpc_modules(
            |ctx: RpcContext<'_, NodeAdapter<N>, <BscEthApiBuilder as EthApiBuilder<NodeAdapter<N>>>::EthApi>| {
                let eth_config = EthConfigHandler::new(
                    ctx.node().provider().clone(),
                    ctx.node().evm_config().clone(),
                );
            ctx.modules
                .merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;
            Ok(())
            },
        )
    }
}

impl<N> DebugNode<N> for BscNode
where
    N: FullNodeComponents<Types = Self>,
{
    type RpcBlock = alloy_rpc_types::Block;

    fn rpc_to_primitive_block(rpc_block: Self::RpcBlock) -> BscBlock {
        let alloy_rpc_types::Block { header, transactions, withdrawals, .. } = rpc_block;
        BscBlock {
            header: header.inner,
            body: BscBlockBody {
                inner: BlockBody {
                    transactions: transactions
                        .into_transactions()
                        .map(|tx| tx.inner.into_inner().into())
                        .collect(),
                    ommers: Default::default(),
                    withdrawals,
                },
                sidecars: None,
            },
        }
    }

    fn local_payload_attributes_builder(
        chain_spec: &Self::ChainSpec,
    ) -> impl PayloadAttributesBuilder<<Self::Payload as PayloadTypes>::PayloadAttributes> {
        LocalPayloadAttributesBuilder::new(Arc::new(chain_spec.clone()))
    }
}
