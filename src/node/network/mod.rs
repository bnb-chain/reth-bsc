#![allow(clippy::owned_cow)]
use crate::{
    BscBlock, node::{
        BscNode, engine_api::payload::BscPayloadTypes, network::{block_import::{BscBlockImport, handle::ImportHandle}, evn_peers::peer_id_to_node_id}, primitives::{BscBlobTransactionSidecar, BscPrimitives}
    }
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
use reth_engine_primitives::ConsensusEngineHandle;
use reth_network::{NetworkConfig, NetworkHandle, NetworkManager};
use reth_network_api::PeersInfo;
use reth_provider::{BlockNumReader, HeaderProvider};
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
pub(crate) mod blocks_by_range;
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
        Mutex<Option<oneshot::Receiver<ConsensusEngineHandle<BscPayloadTypes>>>>,
    >,
}

impl BscNetworkBuilder {
    pub fn new(
        engine_handle_rx: Arc<Mutex<Option<oneshot::Receiver<ConsensusEngineHandle<BscPayloadTypes>>>>>,
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

        let (to_import_net, from_network) = mpsc::unbounded_channel();
        let (to_import_mined, from_builder) = mpsc::unbounded_channel();
        let (to_network, import_outcome) = mpsc::unbounded_channel();

        let (to_hashes, from_hashes) = mpsc::unbounded_channel();
        let handle = ImportHandle::new(to_import_net.clone(), to_hashes, import_outcome);

        // Expose the sender globally so that the miner can submit newly mined blocks
        if crate::shared::set_block_import_mined_sender(to_import_mined.clone()).is_err() {
            warn!(target: "bsc", "Block import mined sender already initialised; overriding skipped");
        }
        if crate::shared::set_block_import_sender(to_import_net.clone()).is_err() {
            warn!(target: "bsc", "Block import network sender already initialised; overriding skipped");
        }
        
        // Import the necessary types for block import service
        use crate::node::network::block_import::service::ImportService;
        
        // Clone needed values before moving into the async closure
        let provider = ctx.provider().clone();
        let chain_spec = ctx.chain_spec().clone();

        // Install a cached full block provider so that BSC BlocksByRange replies
        // can include full bodies if they were recently imported. External callers
        // can override by setting a richer provider before network starts.
        {
            use reth_provider::{HeaderProvider, BlockNumReader};
            struct CachedFullBlockProvider<P> { inner: P }
            impl<P> crate::shared::FullBlockProvider for CachedFullBlockProvider<P>
            where
                P: HeaderProvider<Header = alloy_consensus::Header> + BlockNumReader + Clone + Send + Sync + 'static,
            {
                fn block_by_hash(&self, hash: &alloy_primitives::B256) -> Option<crate::node::primitives::BscBlock> {
                    crate::shared::get_cached_block_by_hash(hash).or_else(|| {
                        self.inner.header(hash).ok().flatten().map(|h| crate::node::primitives::BscBlock {
                            header: h,
                            body: crate::node::primitives::BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None },
                        })
                    })
                }
                fn block_by_number(&self, number: u64) -> Option<crate::node::primitives::BscBlock> {
                    crate::shared::get_cached_block_by_number(number).or_else(|| {
                        self.inner.header_by_number(number).ok().flatten().map(|h| crate::node::primitives::BscBlock {
                            header: h,
                            body: crate::node::primitives::BscBlockBody { inner: reth_ethereum_primitives::BlockBody::default(), sidecars: None },
                        })
                    })
                }
            }

            let _ = crate::shared::set_full_block_provider(Arc::new(CachedFullBlockProvider { inner: provider.clone() }));
        }

        // Spawn the critical ImportService task exactly like the official implementation
        ctx.task_executor().spawn_critical("block import", async move {
            let handle = engine_handle_rx
                .lock()
                .await
                .take()
                .expect("node should only be launched once")
                .await
                .unwrap();

            ImportService::new(
                provider,
                chain_spec,
                handle,
                from_network,
                from_builder,
                from_hashes,
                to_network,
            ).await.unwrap();
        });

        // TODO: update network with the latest canonical head, but has a fork id issue, can fix it later.
        let network_builder = network_builder
            .boot_nodes(ctx.chain_spec().bootnodes().unwrap_or_default())
            .set_head(ctx.chain_spec().head())
            .with_pow()
            .block_import(Box::new(BscBlockImport::new(handle)))
            .discovery(discv4)
            .eth_rlpx_handshake(Arc::new(BscHandshake::default()))
            // Advertise both bsc/2 (with range messages) and bsc/1 (votes only)
            .add_rlpx_sub_protocol(bsc_protocol::protocol::handler::BscProtocolHandlerV2)
            .add_rlpx_sub_protocol(bsc_protocol::protocol::handler::BscProtocolHandlerV1);
        
        let peer_id = network_builder.get_peer_id();
        let mut network_config = ctx.build_network_config(network_builder);
        network_config.status.forkid = network_config.fork_filter.current();
        let provider = ctx.provider();
        if let Ok(number) = provider.best_block_number() {
            let td = provider.header_td_by_number(number).unwrap_or_default();
            network_config.status.total_difficulty = td;
        }
        debug!(
            target: "bsc::net",
            peer_id = ?peer_id_to_node_id(peer_id),
            version = ?network_config.status.version,
            td = ?network_config.status.total_difficulty,
            blockhash = ?network_config.status.blockhash,
            genesis = ?network_config.status.genesis,
            "Initialized BSC network configuration"
        );

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

        if let Err(_h) = crate::shared::set_network_handle(handle.clone()) {
            warn!(target: "bsc::evn", "Network handle already initialised; overriding skipped");
        }

        if crate::node::network::evn::is_evn_enabled() {
            spawn_evn_sync_watcher(ctx, handle.clone());
        }

        Ok(handle)
    }
}

fn spawn_evn_sync_watcher<Node>(ctx: &BuilderContext<Node>, net: NetworkHandle<BscNetworkPrimitives>)
where
    Node: FullNodeTypes<Types = BscNode>,
{
    use reth_provider::StateProviderFactory;
    let max_lag = std::env::var("BSC_EVN_SYNC_LAG_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(30);
    let provider = ctx.provider().clone();
    let chain_spec = ctx.chain_spec().clone();
    ctx.task_executor().spawn_critical("evn-sync-watcher", async move {
        use std::time::{SystemTime, UNIX_EPOCH, Duration};
        use alloy_consensus::BlockHeader;
        use alloy_primitives::U256;
        use reth_primitives::TransactionSigned;
        use crate::node::miner::signer::{is_signer_initialized, sign_system_transaction};

        loop {
            if crate::node::network::evn::is_evn_synced() { break; }
            if let Some(n) = crate::shared::get_best_canonical_block_number() {
                if let Some(h) = crate::shared::get_canonical_header_by_number(n) {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(u64::MAX);
                    let ts = h.timestamp();
                    if now >= ts && now.saturating_sub(ts) <= max_lag {
                        crate::node::network::evn::set_evn_synced(true);
                        info!(target: "bsc::evn", head=%n, head_ts=%ts, lag = now - ts, "EVN armed: node considered synced");
                        if let Some((to_add, to_remove)) = crate::node::network::evn::take_nodeids_actions_once() {
                            if let Some(cfg) = crate::node::miner::config::get_global_mining_config() {
                                if cfg.is_mining_enabled() {
                                    if let Some(validator) = cfg.validator_address {
                                        let mut waited = 0u64;
                                        while !is_signer_initialized() && waited < 100 {
                                            tokio::time::sleep(Duration::from_millis(100)).await;
                                            waited += 1;
                                        }

                                        if is_signer_initialized() {
                                            if let Ok(state) = provider.state_by_block_hash(h.hash_slow()) {
                                                if let Ok(Some(acc)) = state.basic_account(&validator) {
                                                    let mut next_nonce = acc.nonce;
                                                    let chain_id = chain_spec.chain().id();

                                                    let mut signed_batch: Vec<TransactionSigned> = Vec::new();

                                                    if !to_add.is_empty() {
                                                        let (_to, data) = crate::system_contracts::encode_add_node_ids_call(to_add.clone());
                                                        let tx = reth_primitives::Transaction::Legacy(alloy_consensus::TxLegacy {
                                                            chain_id: Some(chain_id),
                                                            nonce: next_nonce,
                                                            gas_limit: u64::MAX / 2,
                                                            gas_price: 0,
                                                            value: U256::ZERO,
                                                            input: data,
                                                            to: alloy_primitives::TxKind::Call(crate::system_contracts::STAKE_HUB_CONTRACT),
                                                        });
                                                        if let Ok(signed) = sign_system_transaction(tx) {
                                                            next_nonce += 1;
                                                            signed_batch.push(signed);
                                                            info!(target: "bsc::evn", count = to_add.len(), "Prepared StakeHub.addNodeIDs for broadcast");
                                                        }
                                                    }

                                                    if !to_remove.is_empty() {
                                                        let (_to, data) = crate::system_contracts::encode_remove_node_ids_call(to_remove.clone());
                                                        let tx = reth_primitives::Transaction::Legacy(alloy_consensus::TxLegacy {
                                                            chain_id: Some(chain_id),
                                                            nonce: next_nonce,
                                                            gas_limit: u64::MAX / 2,
                                                            gas_price: 0,
                                                            value: U256::ZERO,
                                                            input: data,
                                                            to: alloy_primitives::TxKind::Call(crate::system_contracts::STAKE_HUB_CONTRACT),
                                                        });
                                                        if let Ok(signed) = sign_system_transaction(tx) {
                                                            signed_batch.push(signed);
                                                            info!(target: "bsc::evn", count = to_remove.len(), "Prepared StakeHub.removeNodeIDs for broadcast");
                                                        }
                                                    }

                                                    if !signed_batch.is_empty() {
                                                        if let Some(txh) = net.transactions_handle().await {
                                                            let count = signed_batch.len();
                                                            txh.broadcast_transactions(signed_batch);
                                                            info!(target: "bsc::evn", n = count, "Broadcasted StakeHub NodeIDs system txs to public network");
                                                        }
                                                    }
                                                }
                                            }
                                        } else {
                                            info!(target: "bsc::evn", "Skipping NodeIDs broadcast: miner signer not initialised");
                                        }
                                    }
                                }
                            }
                        }

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
