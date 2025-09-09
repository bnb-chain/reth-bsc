
use alloy_consensus::Header;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;

/// BSC payload builder
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscPayloadBuilder<Pool, Client, EvmConfig = BscEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    // builder_config: EthereumBuilderConfig,
    // todo: aborted build task by new header.
}

impl<Pool, Client, BscEvmConfig> BscPayloadBuilder<Pool, Client, BscEvmConfig> {
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: BscEvmConfig,
        //builder_config: EthereumBuilderConfig,
    ) -> Self {
        Self { client, pool, evm_config }
    }

    pub fn build_payload(&self, _parent: &Header) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        // 1.prepare header field by parlia, such as timestamp, difficulty etc.
        // 2.apply change before execute
        // 3.fetch tx-list from tx pool
        // 4.simulate tx execute
        // 5.assemble system txs by parlia
        // 6.seal block by parlia
        // 7.queue to engine-api for memory tree and broadcast it block_import channel(maybe in here)
        todo!()
    }
}