#![allow(clippy::owned_cow)]
use crate::{
    node::{
        engine_api::payload::BscPayloadTypes,
        network::block_import::{handle::ImportHandle, BscBlockImport},
        primitives::{BscBlobTransactionSidecar, BscPrimitives},
        BscNode,
    },
    BscBlock,
};
use alloy_rlp::{Decodable, Encodable};
use handshake::BscHandshake;
use reth::{
    api::{FullNodeTypes, TxTy},
    builder::{components::NetworkBuilder, BuilderContext},
    transaction_pool::{PoolTransaction, TransactionPool},
};
use reth_chainspec::EthChainSpec;
use reth_discv4::Discv4Config;
use reth_eth_wire::{BasicNetworkPrimitives, NewBlock, NewBlockPayload};
use reth_ethereum_primitives::PooledTransactionVariant;
use reth_engine_primitives::BeaconConsensusEngineHandle;
use reth_network::{NetworkConfig, NetworkHandle, NetworkManager};
use reth_network_api::PeersInfo;
use std::{sync::Arc, time::Duration};
use tokio::sync::{mpsc, oneshot, Mutex};
use tracing::{info, warn, debug};

pub mod block_import;
pub mod bootnodes;
pub mod handshake;
pub mod evn;
pub mod evn_peers;
pub(crate) mod upgrade_status;
pub(crate) mod votes;
pub(crate) mod bsc_protocol {
    pub mod protocol {
        pub mod handler;
        pub mod proto;
    }
    pub mod stream;
    pub mod registry;
}
/// BSC `NewBlock` message value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscNewBlock(pub NewBlock<BscBlock>);

mod rlp {
    use super::*;
    use crate::BscBlockBody;
    use alloy_consensus::{BlockBody, Header};
    use alloy_primitives::U128;
    use alloy_rlp::{RlpDecodable, RlpEncodable};
    use alloy_rpc_types::Withdrawals;
    use reth_primitives::TransactionSigned;
    use std::borrow::Cow;

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    struct BlockHelper<'a> {
        header: Cow<'a, Header>,
        transactions: Cow<'a, Vec<TransactionSigned>>,
        ommers: Cow<'a, Vec<Header>>,
        withdrawals: Option<Cow<'a, Withdrawals>>,
    }

    #[derive(RlpEncodable, RlpDecodable)]
    #[rlp(trailing)]
    struct BscNewBlockHelper<'a> {
        block: BlockHelper<'a>,
        td: U128,
        sidecars: Option<Cow<'a, Vec<BscBlobTransactionSidecar>>>,
    }

    impl<'a> From<&'a BscNewBlock> for BscNewBlockHelper<'a> {
        fn from(value: &'a BscNewBlock) -> Self {
            let BscNewBlock(NewBlock {
                block:
                    BscBlock {
                        header,
                        body:
                            BscBlockBody {
                                inner: BlockBody { transactions, ommers, withdrawals },
                                sidecars,
                            },
                    },
                td,
            }) = value;

            Self {
                block: BlockHelper {
                    header: Cow::Borrowed(header),
                    transactions: Cow::Borrowed(transactions),
                    ommers: Cow::Borrowed(ommers),
                    withdrawals: withdrawals.as_ref().map(Cow::Borrowed),
                },
                td: *td,
                sidecars: sidecars.as_ref().map(Cow::Borrowed),
            }
        }
    }

    impl Encodable for BscNewBlock {
        fn encode(&self, out: &mut dyn bytes::BufMut) {
            BscNewBlockHelper::from(self).encode(out);
        }

        fn length(&self) -> usize {
            BscNewBlockHelper::from(self).length()
        }
    }

    impl Decodable for BscNewBlock {
        fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
            let BscNewBlockHelper {
                block: BlockHelper { header, transactions, ommers, withdrawals },
                td,
                sidecars,
            } = BscNewBlockHelper::decode(buf)?;

            Ok(BscNewBlock(NewBlock {
                block: BscBlock {
                    header: header.into_owned(),
                    body: BscBlockBody {
                        inner: BlockBody {
                            transactions: transactions.into_owned(),
                            ommers: ommers.into_owned(),
                            withdrawals: withdrawals.map(|w| w.into_owned()),
                        },
                        sidecars: sidecars.map(|s| s.into_owned()),
                    },
                },
                td,
            }))
        }
    }
}

impl NewBlockPayload for BscNewBlock {
    type Block = BscBlock;

    fn block(&self) -> &Self::Block {
        &self.0.block
    }
}

/// Network primitives for BSC.
pub type BscNetworkPrimitives =
    BasicNetworkPrimitives<BscPrimitives, PooledTransactionVariant, BscNewBlock>;

/// A basic bsc network builder.
#[derive(Debug)]
pub struct BscNetworkBuilder {
    engine_handle_rx: Arc<
        Mutex<Option<oneshot::Receiver<BeaconConsensusEngineHandle<BscPayloadTypes>>>>,
    >,
}

impl BscNetworkBuilder {
    pub fn new(
        engine_handle_rx: Arc<Mutex<Option<oneshot::Receiver<BeaconConsensusEngineHandle<BscPayloadTypes>>>>>,
    ) -> Self {
        Self { engine_handle_rx }
    }
}

impl Default for BscNetworkBuilder {
    fn default() -> Self {
        let (_tx, rx) = oneshot::channel();
        Self::new(Arc::new(Mutex::new(Some(rx))))
    }
}

impl BscNetworkBuilder {
    /// Returns the [`NetworkConfig`] that contains the settings to launch the p2p network.
    ///
    /// This applies the configured [`BscNetworkBuilder`] settings.
    pub fn network_config<Node>(
        self,
        ctx: &BuilderContext<Node>,
    ) -> eyre::Result<NetworkConfig<Node::Provider, BscNetworkPrimitives>>
    where
        Node: FullNodeTypes<Types = BscNode>,
    {
        let Self { engine_handle_rx } = self;

        // Check if CLI bootnodes are provided and set bootnode override
        if let Some(cli_bootnodes) = ctx.config().network.resolved_bootnodes() {
            use crate::chainspec::bootnode_override;
            if let Err(e) = bootnode_override::set_bootnode_override(Some(cli_bootnodes)) {
                warn!(target: "bsc", "Failed to set bootnode override: {}", e);
            }
        } else {
            // No CLI bootnodes provided, disable override
            use crate::chainspec::bootnode_override;
            if let Err(e) = bootnode_override::set_bootnode_override(None) {
                warn!(target: "bsc", "Failed to disable bootnode override: {}", e);
            }
        }

        let network_builder = ctx.network_config_builder()?;
        let mut discv4 = Discv4Config::builder();

        if let Some(boot_nodes) = ctx.chain_spec().bootnodes() {
            discv4.add_boot_nodes(boot_nodes);
        }
        discv4.lookup_interval(Duration::from_millis(500));

        let (to_import, from_network) = mpsc::unbounded_channel();
        let (to_network, import_outcome) = mpsc::unbounded_channel();

        let (to_hashes, from_hashes) = mpsc::unbounded_channel();
        let handle = ImportHandle::new(to_import.clone(), to_hashes, import_outcome);

        // Expose the sender globally so that the miner can submit newly mined blocks
        if crate::shared::set_block_import_sender(to_import.clone()).is_err() {
            warn!(target: "bsc", "Block import sender already initialised; overriding skipped");
        }
        
        // Import the necessary types for consensus
        use crate::consensus::ParliaConsensus;
        use crate::node::network::block_import::service::ImportService;
        
        // Create consensus instance for ImportService
        let consensus = Arc::new(ParliaConsensus::new(
            ctx.provider().clone(),
            ctx.chain_spec().clone(),
        ));
        
        // Spawn the critical ImportService task exactly like the official implementation
        ctx.task_executor().spawn_critical("block import", async move {
            let handle = engine_handle_rx
                .lock()
                .await
                .take()
                .expect("node should only be launched once")
                .await
                .unwrap();

            ImportService::new(consensus, handle, from_network, from_hashes, to_network).await.unwrap();
        });

        let network_builder = network_builder
            .boot_nodes(ctx.chain_spec().bootnodes().unwrap_or_default())
            .set_head(ctx.chain_spec().head())
            .with_pow()
            .block_import(Box::new(BscBlockImport::new(handle)))
            .discovery(discv4)
            .eth_rlpx_handshake(Arc::new(BscHandshake::default()))
            .add_rlpx_sub_protocol(bsc_protocol::protocol::handler::BscProtocolHandler);
        
        let mut network_config = ctx.build_network_config(network_builder);
        // Ensure our advertised fork ID matches the fork filter we validate against.
        network_config.status.forkid = network_config.fork_filter.current();

        Ok(network_config)
    }
}

impl<Node, Pool> NetworkBuilder<Node, Pool> for BscNetworkBuilder
where
    Node: FullNodeTypes<Types = BscNode>,
    Pool: TransactionPool<
            Transaction: PoolTransaction<
                Consensus = TxTy<Node::Types>,
                Pooled = PooledTransactionVariant,
            >,
        > + Unpin
        + 'static,
{
    type Network = NetworkHandle<BscNetworkPrimitives>;

    async fn build_network(
        self,
        ctx: &BuilderContext<Node>,
        pool: Pool,
    ) -> eyre::Result<Self::Network> {
        let network_config = self.network_config(ctx)?;
        let network = NetworkManager::builder(network_config).await?;
        let handle = ctx.start_network(network, pool);
        info!(target: "reth::cli", enode=%handle.local_node_record(), "P2P networking initialized");
        
        let local_peer_id = handle.peer_id();
        if crate::shared::set_local_peer_id(*local_peer_id).is_err() {
            warn!(target: "reth::cli", "Failed to set global local peer ID - already set");
        } else {
            info!(target: "reth::cli", peer_id=%local_peer_id, "Local peer ID set globally");
        }

        // Share network handle for dynamic EVN actions (trust marking, targeted ETH messages).
        if let Err(_h) = crate::shared::set_network_handle(handle.clone()) {
            warn!(target: "bsc::evn", "Network handle already initialised; overriding skipped");
        }

        // EVN sync watcher: arm EVN only once node is considered synced (by head timestamp lag)
        {
            // Only spawn if EVN is enabled; otherwise skip
            if crate::node::network::evn::is_evn_enabled() {
                let max_lag = std::env::var("BSC_EVN_SYNC_LAG_SECS")
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(30);
                // Placeholder: if needed later, capture provider/chain_spec here for extended EVN logic
                ctx.task_executor().spawn_critical("evn-sync-watcher", async move {
                    use std::time::{SystemTime, UNIX_EPOCH, Duration};
                    use alloy_consensus::BlockHeader;
                    loop {
                        // If already armed, stop watcher
                        if crate::node::network::evn::is_evn_synced() { break; }
                        // Query latest header via global providers
                        if let Some(n) = crate::shared::get_best_block_number() {
                            if let Some(h) = crate::shared::get_header_by_number(n) {
                                let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(u64::MAX);
                                let ts = h.timestamp();
                                if now >= ts && now.saturating_sub(ts) <= max_lag {
                                    crate::node::network::evn::set_evn_synced(true);
                                    info!(target: "bsc::evn", head=%n, head_ts=%ts, lag = now - ts, "EVN armed: node considered synced");
                                    // Once EVN is armed, query on-chain StakeHub for validator NodeIDs and
                                    // mark currently connected peers that match as EVN (on-chain reason).
                                    break;
                                } else {
                                    debug!(target: "bsc::evn", head=%n, head_ts=%ts, lag = now.saturating_sub(ts), max_lag=%max_lag, "EVN not armed yet: syncing");
                                }
                            }
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                });
            }
        }

        Ok(handle)
    }
}
