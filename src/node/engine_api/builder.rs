use super::BscEngineApi;
use reth::{
    api::{AddOnsContext, FullNodeComponents, NodeTypes},
    builder::rpc::EngineApiBuilder,
};
use std::sync::Arc;

/// Builder for [`BscEngineApi`] implementation.
#[derive(Debug, Default)]
pub struct BscEngineApiBuilder;

impl<N> EngineApiBuilder<N> for BscEngineApiBuilder
where
    N: FullNodeComponents,
    N::Types: NodeTypes<Payload = crate::node::engine_api::payload::BscPayloadTypes>,
{
    type EngineApi = BscEngineApi;

    async fn build_engine_api(self, ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        // Get the engine handle from the context
        let engine_handle = ctx.beacon_engine_handle.clone();
        Ok(BscEngineApi::new(Arc::new(engine_handle)))
    }
}
