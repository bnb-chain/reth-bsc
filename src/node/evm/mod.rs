use reth::{
    api::{FullNodeTypes, NodeTypes},
    builder::{components::ExecutorBuilder, BuilderContext},
};
mod assembler;
mod builder;
pub mod config;
mod finish_metrics;
pub use config::BscEvmConfig;
mod executor;
pub use executor::{BscBlockExecutor, LaneAdmission};
mod factory;
pub use factory::BscEvmFactory;
#[cfg(test)]
mod cas20_seed_tests;
pub mod error;
#[cfg(test)]
mod jenner_transition_tests;
mod patch;
mod post_execution;
pub mod pre_execution;
#[cfg(test)]
mod pre_execution_tests;
pub mod util;
/// A regular bsc evm and executor builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscExecutorBuilder;

impl<Node> ExecutorBuilder<Node> for BscExecutorBuilder
where
    Node: FullNodeTypes,
    Node::Types: NodeTypes<
        Primitives = crate::node::primitives::BscPrimitives,
        ChainSpec = crate::chainspec::BscChainSpec,
        Payload = crate::node::engine_api::payload::BscPayloadTypes,
        Storage = crate::node::storage::BscStorage,
    >,
{
    type EVM = BscEvmConfig;

    async fn build_evm(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::EVM> {
        let evm_config = BscEvmConfig::bsc(ctx.chain_spec());
        Ok(evm_config)
    }
}

#[cfg(test)]
pub(crate) mod test_utils;
