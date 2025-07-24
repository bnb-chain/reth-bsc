use super::BscEngineApi;
use reth::{
    api::{AddOnsContext, FullNodeComponents},
    builder::rpc::EngineApiBuilder,
};

/// Builder for mocked [`BscEngineApi`] implementation.
#[derive(Debug, Default)]
pub struct BscEngineApiBuilder;

impl<N> EngineApiBuilder<N> for BscEngineApiBuilder
where
    N: FullNodeComponents,
{
    type EngineApi = BscEngineApi;

    async fn build_engine_api(self, _ctx: &AddOnsContext<'_, N>) -> eyre::Result<Self::EngineApi> {
        Ok(BscEngineApi::default())
    }
}
