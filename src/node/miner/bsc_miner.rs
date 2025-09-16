use crate::node::evm::util::HEADER_CACHE_READER;
use crate::{
    consensus::parlia::provider::SnapshotProvider,
    node::{
        engine::BscBuiltPayload,
        evm::config::BscEvmConfig,
        miner::{
            payload_builder::BscPayloadBuilder, 
            util::prepare_new_attributes, 
            signer::init_global_signer,
            config::{keystore, MiningConfig}
        },
    },
    BscBlock,
};
use alloy_consensus::BlockHeader;
use alloy_primitives::{Address, B256};
use k256::ecdsa::SigningKey;
use reth::transaction_pool::PoolTransaction;
use reth::transaction_pool::TransactionPool;
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_payload_primitives::BuiltPayload;
use reth_primitives::{SealedBlock, TransactionSigned};
use reth_provider::{BlockNumReader, HeaderProvider};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, error, info, warn};
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth::payload::EthPayloadBuilderAttributes;
use reth_revm::cancelled::CancelOnDrop;
use reth_primitives_traits::BlockBody;

/// Mining Service that handles block production for BSC
pub struct BscMiner<Pool, Provider> {
    pool: Pool,
    provider: Provider,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    validator_address: Address,
    chain_spec: Arc<crate::chainspec::BscChainSpec>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    signing_key: Option<SigningKey>,
    mining_config: MiningConfig,

    // todo: add more eventloop for performance like bsc miner.
}

impl<Pool, Provider> BscMiner<Pool, Provider>
where
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>
        + Clone
        + 'static,
    Provider: HeaderProvider<Header = alloy_consensus::Header>
        + BlockNumReader
        + reth_provider::StateProviderFactory
        + Clone
        + Send
        + Sync
        + 'static,
{
    pub fn new(
        pool: Pool,
        provider: Provider,
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        mining_config: MiningConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // Validate mining configuration
        mining_config.validate()?;

        // We'll derive and trust the validator address from the configured signing key when possible.
        // If not available, fall back to configured address (may be ZERO when disabled).
        let mut validator_address = mining_config.validator_address.unwrap_or(Address::ZERO);

        // Load signing key if mining is enabled
        let signing_key = if mining_config.is_mining_enabled() {
            let key = if let Some(keystore_path) = &mining_config.keystore_path {
                let password = mining_config.keystore_password.as_deref().unwrap_or("");
                keystore::load_private_key_from_keystore(keystore_path, password)?
            } else if let Some(hex_key) = &mining_config.private_key_hex {
                keystore::load_private_key_from_hex(hex_key)?
            } else {
                return Err("No signing key configured".into());
            };

            // Derive validator address from the signing key and prefer it
            let derived_address = keystore::get_validator_address(&key);
            if derived_address != validator_address {
                if validator_address != Address::ZERO {
                    warn!(
                        "Validator address mismatch: configured={}, derived={}",
                        validator_address, derived_address
                    );
                }
                info!("Using derived address from private key: {}", derived_address);
                validator_address = derived_address;
            }

            Some(key)
        } else {
            None
        };

        Ok(Self {
            pool,
            provider,
            snapshot_provider,
            validator_address,
            chain_spec: chain_spec.clone(),
            parlia: Arc::new(crate::consensus::parlia::Parlia::new(chain_spec, 200)),
            signing_key,
            mining_config,
        })
    }

    /// Start the PoA mining loop
    pub async fn start_mining(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        if !self.mining_config.is_mining_enabled() {
            info!("Mining is disabled in configuration");
            return Ok(());
        }

        if let Some(ref signing_key) = self.signing_key {
            let private_key_bytes = signing_key.as_nonzero_scalar().to_bytes();
            let private_key = B256::from_slice(&private_key_bytes);
            
            if let Err(e) = init_global_signer(private_key) {
                warn!("Failed to initialize global signer: {}", e);
            } else {
                info!("Succeed to initialize global signer");
            }
        } else {
            warn!("No signing key available, global signer not initialized");
        }

        // Ensure the genesis block header is cached so that the snapshot provider can create the genesis snapshot
        {
            let mut cache = HEADER_CACHE_READER.lock().unwrap();
            if cache.get_header_by_number(0).is_none() {
                if let Some(genesis_header) = self.provider.header_by_number(0)? {
                    cache.insert_header_to_cache(genesis_header);
                    info!("Inserted genesis header from provider into cache");
                } else {
                    // Build the genesis header from the chain spec as fallback
                    let genesis_header = self.chain_spec.genesis_header().clone();
                    cache.insert_header_to_cache(genesis_header.clone());
                    info!("Inserted genesis header from chain spec into cache");
                }
            } else {
                info!("Genesis header already cached");
            }
        }

        info!("Starting BSC mining service for validator: {}", self.validator_address);

        // Mining interval from config or default
        let interval_ms = self.mining_config.mining_interval_ms.unwrap_or(3000);
        let mut mining_interval = interval(Duration::from_millis(interval_ms));

        info!("BSC mining interval is: {}", interval_ms);
        loop {
            mining_interval.tick().await;

            if let Err(e) = self.try_mine_block().await {
                error!("Mining attempt failed: {}", e);
                // Continue mining loop even if individual attempts fail
            }
        }
    }

    /// Attempt to mine a block if conditions are met
    async fn try_mine_block(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Get current head block from chain state
        let current_block_number = self.provider.best_block_number()?;
        let parent_header = self
            .provider
            .sealed_header(current_block_number)?
            .ok_or("Head block header not found")?;

        // let current_time = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let parent_number = parent_header.number();

        // Get snapshot for parent block to check authorization
        let snapshot = self
            .snapshot_provider
            .snapshot(parent_number)
            .ok_or("No snapshot available for parent block")?;

        // Check if we're authorized to mine
        if !snapshot.validators.contains(&self.validator_address) {
            return Err(format!("Not authorized validator: {}", self.validator_address).into());
        }

        // Check if we signed recently (avoid signing too frequently)
        if snapshot.sign_recently(self.validator_address) {
            return Err("Signed recently, must wait for others".into());
        }

        let attributes = prepare_new_attributes(self.parlia.clone(), &snapshot, &parent_header, self.validator_address);
        let evm_config = BscEvmConfig::new(self.chain_spec.clone());
        let payload_builder = BscPayloadBuilder::new(self.provider.clone(), self.pool.clone(), evm_config, EthereumBuilderConfig::new());
        let payload = payload_builder.build_payload(BuildArguments::<EthPayloadBuilderAttributes, BscBuiltPayload>::new(
            reth_revm::cached::CachedReads::default(),
            PayloadConfig::new(Arc::new(parent_header.clone()), attributes),
            CancelOnDrop::default(),
            None,
        ))?;

        // queue to engine-api for memory tree and broadcast it block_import channel.
        // todo: check it.
        debug!("Submitting block: {:?}, tx_len: {}", payload.block().header(), payload.block().body().transaction_count());
        self.submit_block(payload.block()).await?;

        Ok(())
    }

    /// Submit the sealed block (placeholder for now)
    async fn submit_block(
        &self,
        sealed_block: &SealedBlock<BscBlock>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::node::network::BscNewBlock;
        use crate::shared::get_block_import_sender;
        use alloy_primitives::U128;
        use reth_network::message::NewBlockMessage;
        use reth_network_api::PeerId;

        let block_hash = sealed_block.hash();
        let block = sealed_block.clone_block();

        // Construct the NewBlock network message
        let td = U128::from(1u64); // TODO: compute real total difficulty
        let new_block = BscNewBlock(reth_eth_wire::NewBlock { block: block.clone(), td });
        let msg = NewBlockMessage { hash: block_hash, block: Arc::new(new_block) };

        // If the block import sender is available, forward the block to the import service
        if let Some(sender) = get_block_import_sender() {
            // Wrap into IncomingBlock tuple (BlockMsg, PeerId)
            let peer_id = PeerId::default(); // `None` for self-originated blocks
            let incoming: crate::node::network::block_import::service::IncomingBlock =
                (msg, peer_id);
            if sender.send(incoming).is_err() {
                warn!("Failed to send mined block to import service: channel closed");
            }
        } else {
            warn!("Block import sender not initialised; mined block not imported");
        }

        Ok(())
    }
}