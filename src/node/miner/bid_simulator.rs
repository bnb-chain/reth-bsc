use crate::chainspec::BscChainSpec;
use crate::consensus::eip4844::calc_blob_fee;
use crate::consensus::parlia::provider::SnapshotProvider;
use crate::consensus::parlia::Snapshot;
use crate::hardforks::BscHardforks;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::payload::DELAY_LEFT_OVER;
use crate::node::miner::util::prepare_new_attributes;
use crate::node::primitives::BscBlobTransactionSidecar;
use alloy_consensus::BlobTransactionSidecar;
use alloy_consensus::BlockHeader;
use alloy_consensus::Transaction;
use alloy_evm::Evm;
use alloy_primitives::U256;
use alloy_primitives::{Address, B256};
use parking_lot::RwLock;
use reth::payload::EthPayloadBuilderAttributes;
use reth::transaction_pool::BestTransactionsAttributes;
use reth_chain_state::{ExecutedBlock, ExecutedTrieUpdates};
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_evm::execute::BlockBuilder;
use reth_evm::execute::BlockBuilderOutcome;
use reth_evm::execute::ExecutionOutcome;
use reth_evm::execute::{BlockExecutionError, BlockValidationError};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderError;
use reth_primitives::SealedHeader;
use reth_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use reth_provider::StateProviderFactory;
use reth_provider::{BlockHashReader, HeaderProvider};
use reth_revm::{database::StateProviderDatabase, db::State};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::{debug, trace};
const NO_INTERRUPT_LEFT_OVER: u64 = 500;
const PAY_BID_TX_GAS_LIMIT: u64 = 25000;
const TX_GAS: u64 = 21000;

#[derive(Clone)]
pub struct Bid {
    pub builder: Address,
    pub block_number: u64,
    pub parent_hash: B256,
    pub txs: Vec<reth_primitives::TransactionSigned>,
    pub blob_sidecars: HashMap<B256, BlobTransactionSidecar>,
    pub un_revertible: Vec<B256>,
    pub gas_used: u64,
    pub gas_fee: U256,
    pub builder_fee: U256,
    pub committed: bool,
    pub bid_hash: B256,
    pub interrupt_flag: Arc<AtomicBool>,
}

impl Bid {
    fn is_committed(&self) -> bool {
        self.committed
    }
}

// bid loop receive bid from client and commit bid to simulator
// 1. last block number check
// 2. pack bid runtime and calculate bid value
// 3. find best bid
// 4. can be interrupt the last bid and commit
pub struct BidSimulator<Client, Pool> {
    client: Client,
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    pool: Pool,
    validator_address: Address,

    // Each map has its own lock for fine-grained concurrency control
    // This avoids writer starvation when one operation needs write access
    best_bid_to_run: Arc<RwLock<HashMap<B256, Bid>>>,
    simulating_bid: Arc<RwLock<HashMap<B256, Bid>>>,
    best_bid: Arc<RwLock<HashMap<B256, BidRuntime<Pool, BscEvmConfig>>>>,
    pending_bid: Arc<RwLock<HashMap<String, u8>>>,
    bid_receiving: bool,
    chain_spec: Arc<BscChainSpec>,
    min_gas_price: U256,
    validator_commission: u64,
    greedy_merge: bool,

    // MEV metrics
    mev_metrics: crate::metrics::BscMevMetrics,
}

#[allow(clippy::too_many_arguments)]
impl<Client, Pool> BidSimulator<Client, Pool>
where
    Client: HeaderProvider<Header = alloy_consensus::Header>
        + BlockHashReader
        + StateProviderFactory
        + Clone
        + 'static,
    Pool: reth::transaction_pool::TransactionPool<
            Transaction: reth::transaction_pool::PoolTransaction<Consensus = TransactionSigned>,
        > + 'static,
{
    pub fn new(
        client: Client,
        pool: Pool,
        chain_spec: Arc<BscChainSpec>,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        validator_address: Address,
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        validator_commission: u64,
        greedy_merge: bool,
    ) -> Self {
        Self {
            client,
            parlia,
            pool,
            validator_address,
            chain_spec,
            snapshot_provider,
            best_bid_to_run: Arc::new(RwLock::new(HashMap::new())),
            simulating_bid: Arc::new(RwLock::new(HashMap::new())),
            best_bid: Arc::new(RwLock::new(HashMap::new())),
            pending_bid: Arc::new(RwLock::new(HashMap::new())),
            bid_receiving: true,
            min_gas_price: U256::ZERO,
            mev_metrics: crate::metrics::BscMevMetrics::default(),
            validator_commission,
            greedy_merge,
        }
    }

    pub fn check_pending_bid(&self, block_number: u64, builder: Address, bid_hash: B256) -> bool {
        let key = format!("{}-{}-{}", block_number, builder, bid_hash);
        let pending_bid = self.pending_bid.read();
        if let Some(exist) = pending_bid.get(&key) {
            if *exist > 0 {
                return false;
            }
        }
        true
    }

    pub fn add_pending_bid(&self, block_number: u64, builder: Address, bid_hash: B256) {
        let key = format!("{}-{}-{}", block_number, builder, bid_hash);
        self.pending_bid.write().insert(key, 1);
        self.mev_metrics.pending_bids.increment(1);
    }

    pub fn commit_new_bid(&self, bid: Bid) -> Option<BidRuntime<Pool, BscEvmConfig>> {
        if !self.check_pending_bid(bid.block_number, bid.builder, bid.bid_hash) {
            debug!("bid is already pending, ignore");
            return None;
        }
        self.add_pending_bid(bid.block_number, bid.builder, bid.bid_hash);
        let final_block_number = match self.client.finalized_block_number() {
            Ok(Some(final_block_number)) => final_block_number,
            Ok(None) => return None,
            Err(_) => return None,
        };
        if bid.block_number <= final_block_number {
            // Bid is for a block that's already finalized, ignore it
            return None;
        }

        let parent_hash = bid.parent_hash;
        let parent_header = match self.client.header(&parent_hash) {
            Ok(Some(header)) => {
                let hash = header.hash_slow();
                SealedHeader::new(header, hash)
            }
            _ => {
                debug!("Failed to get parent header for hash: {:?}", parent_hash);
                return None;
            }
        };
        let parent_snapshot = match self.snapshot_provider.snapshot_by_hash(&parent_hash) {
            Some(snapshot) => snapshot,
            None => {
                debug!(
                    "Skip to mine new block due to no snapshot available, validator: {}, tip: {}",
                    self.validator_address, parent_hash
                );
                return None;
            }
        };
        let mut mining_ctx = MiningContext {
            parent_snapshot: Arc::new(parent_snapshot),
            parent_header: parent_header.clone(),
            header: None,
            is_inturn: true,
            cached_reads: None,
        };
        let parent_snapshot = mining_ctx.parent_snapshot.clone();
        let attributes = prepare_new_attributes(
            &mut mining_ctx,
            self.parlia.clone(),
            &parent_header,
            self.validator_address,
        );

        let mut _bid_runtime = match self.new_bid_runtime(
            &bid,
            self.validator_commission,
            attributes.clone(),
            mining_ctx.clone(),
        ) {
            Ok(bid_runtime) => bid_runtime,
            Err(err) => {
                debug!("create runtime error:{}", err);
                return None;
            }
        };
        let mut to_commit = true;
        let mut _bid_accepted = true;

        // Acquire read lock only when needed
        let best_bid_opt = self.best_bid_to_run.read().get(&parent_hash).cloned();
        if let Some(best_bid) = best_bid_opt {
            let best_bid_runtime = match self.new_bid_runtime(
                &best_bid,
                self.validator_commission,
                attributes.clone(),
                mining_ctx.clone(),
            ) {
                Ok(best_bid_runtime) => best_bid_runtime,
                Err(err) => {
                    debug!("create runtime error:{}", err);
                    return None;
                }
            };
            if _bid_runtime.is_expected_better_than(&best_bid_runtime) {
                debug!(
                    "new bid has better expectedBlockReward builder:{}, bid_hash:{}",
                    _bid_runtime.bid.builder, _bid_runtime.bid.bid_hash,
                );
            } else if !best_bid.is_committed() {
                _bid_runtime = best_bid_runtime;
                _bid_accepted = false;
                debug!("discard new bid and to simulate the non-committed bestBidToRun builder:{}, bid_hash:{}", _bid_runtime.bid.builder,"");
            } else {
                to_commit = false;
                _bid_accepted = false;
                debug!(
                    "new bid will be discarded builder:{}, bid_hash:{}",
                    _bid_runtime.bid.builder, _bid_runtime.bid.bid_hash,
                );
            }
        }

        if to_commit {
            self.best_bid_to_run
                .write()
                .insert(_bid_runtime.bid.parent_hash, _bid_runtime.bid.clone());

            if let Some(simulating_bid) = self.simulating_bid.read().get(&bid.parent_hash).cloned()
            {
                let delay_ms = self.parlia.delay_for_mining(
                    &parent_snapshot,
                    mining_ctx.header.as_ref().unwrap(),
                    DELAY_LEFT_OVER,
                );
                if delay_ms >= NO_INTERRUPT_LEFT_OVER || delay_ms == 0 {
                    simulating_bid.interrupt_flag.store(true, Ordering::Relaxed);
                    let bid_simulate_req = self.commit_bid(5, _bid_runtime);
                    return Some(bid_simulate_req);
                } else {
                    debug!("simulate in progress, no interrupt after delay_ms:{}, NO_INTERRUPT_LEFT_OVER:{},bid hash:{}", delay_ms, NO_INTERRUPT_LEFT_OVER, _bid_runtime.bid.bid_hash);
                }
            } else {
                let bid_simulate_req = self.commit_bid(5, _bid_runtime);
                return Some(bid_simulate_req);
            }
        }

        None
    }

    pub fn clear(&self, block_number: u64) {
        let clear_threshold = 5; //todo: config
        let min_block_number = block_number.saturating_sub(clear_threshold);

        // Clear old bids from best_bid_to_run, simulating_bid, and best_bid
        self.best_bid_to_run.write().retain(|_, bid| bid.block_number >= min_block_number);
        self.simulating_bid.write().retain(|_, bid| bid.block_number >= min_block_number);
        self.best_bid.write().retain(|_, bid| bid.bid.block_number >= min_block_number);

        // Clear old pending bids by parsing block_number from key prefix
        // Key format: "{block_number}-{builder}-{bid_hash}"
        self.pending_bid.write().retain(|key, _| {
            // Parse block_number from the key (first part before '-')
            if let Some(block_num_str) = key.split('-').next() {
                if let Ok(bid_block_number) = block_num_str.parse::<u64>() {
                    // Keep only if block_number >= min_block_number
                    return bid_block_number >= min_block_number;
                }
            }
            // If parsing fails, keep the entry (safe default)
            true
        });
        self.mev_metrics.pending_bids.set(self.pending_bid.read().len() as f64);
    }

    fn new_bid_runtime(
        &self,
        _bid: &Bid,
        _validator_commission: u64,
        attributes: EthPayloadBuilderAttributes,
        mining_ctx: MiningContext,
    ) -> Result<BidRuntime<Pool, BscEvmConfig>, Box<dyn std::error::Error + Send + Sync>> {
        let mut runtime = BidRuntime::new(
            _bid.clone(),
            self.pool.clone(),
            BscEvmConfig::new(self.chain_spec.clone()),
            attributes,
            self.chain_spec.clone(),
            mining_ctx,
        );
        let expected_block_reward = _bid.gas_fee;
        let mut expected_validator_reward =
            expected_block_reward * U256::from(_validator_commission);
        expected_validator_reward /= U256::from(10000u64);
        if expected_validator_reward < _bid.builder_fee {
            debug!("BidSimulator: invalid bid, builder fee exceeds validator reward, ignore expected_validator_reward:{} builder_fee:{}", expected_validator_reward, _bid.builder_fee);
            return Err("invalid bid: builder fee exceeds validator reward".into());
        }
        expected_validator_reward -= _bid.builder_fee;
        runtime.expected_block_reward = expected_block_reward;
        runtime.expected_validator_reward = expected_validator_reward;
        Ok(runtime)
    }

    fn commit_bid(
        &self,
        reason: u32,
        mut bid_runtime: BidRuntime<Pool, BscEvmConfig>,
    ) -> BidRuntime<Pool, BscEvmConfig> {
        debug!("bid committed reason:{}, bid hash:{}", reason, bid_runtime.bid.bid_hash);
        bid_runtime.bid.committed = true;

        bid_runtime
    }

    // sim_bid commit tx and set best bid
    pub fn bid_simulate(&self, mut bid_runtime: BidRuntime<Pool, BscEvmConfig>) {
        if !self.bid_receiving {
            return;
        }

        // Track simulation start time
        let sim_start = std::time::Instant::now();
        let is_first_bid = self.best_bid.read().is_empty();

        let mut success = false;
        let parent_hash = bid_runtime.bid.parent_hash;
        self.simulating_bid.write().insert(parent_hash, bid_runtime.bid.clone());

        let mut txs_except_last = bid_runtime.bid.txs.clone();
        let pay_bid_tx = txs_except_last.pop();

        let state_provider =
            match self.client.state_by_block_hash(bid_runtime.parent_header.hash_slow()) {
                Ok(provider) => provider,
                Err(e) => {
                    debug!("Failed to get state provider by block hash: {:?}", e);
                    return;
                }
            };
        let sp_db = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(sp_db).with_bundle_update().build();

        // Clone necessary fields to avoid borrow conflicts
        let evm_config = bid_runtime.evm_config.clone();
        let parent_header = bid_runtime.parent_header.clone();
        let attributes = bid_runtime.attributes.clone();
        let builder_config = bid_runtime.builder_config.clone();
        let gas_limit = builder_config.gas_limit(parent_header.gas_limit);
        let system_txs_gas = self.parlia.estimate_gas_reserved_for_system_txs(
            Some(parent_header.timestamp),
            parent_header.number + 1,
            attributes.timestamp,
        );
        if bid_runtime.bid.gas_used > gas_limit - system_txs_gas - PAY_BID_TX_GAS_LIMIT {
            debug!("bidSimulator: gas limit exceeded, ignore");
            return;
        }

        let mut builder = match evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit,
                    parent_beacon_block_root: attributes.parent_beacon_block_root(),
                    withdrawals: Some(attributes.withdrawals().clone()),
                },
            )
            .map_err(PayloadBuilderError::other)
        {
            Ok(builder) => builder,
            Err(e) => {
                debug!("Failed to create builder for next block: {:?}", e);
                return;
            }
        };
        let mut block_gas_limit: u64 =
            builder.evm_mut().block().gas_limit.saturating_sub(system_txs_gas);
        block_gas_limit = block_gas_limit.saturating_sub(PAY_BID_TX_GAS_LIMIT);

        // todo: prefetch transactions
        if let Err(e) = builder.apply_pre_execution_changes().map_err(PayloadBuilderError::other) {
            debug!("Failed to apply pre-execution changes: {:?}", e);
            return;
        }

        // First commit: bid transactions
        if let Err(e) =
            bid_runtime.commit_transaction(txs_except_last.clone(), &mut builder, block_gas_limit)
        {
            debug!("Failed to commit bid transactions: {:?}", e);
            return;
        }

        if let Err(e) =
            bid_runtime.pack_reward(self.validator_commission, bid_runtime.system_balance)
        {
            debug!("Failed to pack reward: {:?}", e);
            return;
        }
        if !bid_runtime.valid_reward() {
            debug!("bidSimulator: invalid bid, ignore");
            return;
        }

        if bid_runtime.gas_used != 0 {
            let bid_gas_price = bid_runtime.gas_fee / U256::from(bid_runtime.gas_used);
            if bid_gas_price < self.min_gas_price {
                debug!(
                    "bid gas price is lower than min gas price, bid:{}, min:{}",
                    bid_gas_price, self.min_gas_price
                );
                return;
            }
        }

        // if enable greedy merge, fill bid env with transactions from mempool
        if self.greedy_merge {
            let ending_bids_extra = 20;
            let min_time_left_for_ending_bids = DELAY_LEFT_OVER + ending_bids_extra;
            let delay_ms = self.parlia.delay_for_mining(
                &bid_runtime.mining_ctx.parent_snapshot,
                bid_runtime.mining_ctx.header.as_ref().unwrap(),
                min_time_left_for_ending_bids,
            );
            if delay_ms > 0 {
                // Track greedy merge execution time
                let greedy_merge_start = std::time::Instant::now();

                if let Err(e) = bid_runtime.fill_tx_from_pool(
                    &mut builder,
                    txs_except_last,
                    block_gas_limit,
                    delay_ms,
                ) {
                    debug!("Failed to commit tx pool transactions: {:?}", e);
                    return;
                }
                if let Err(e) =
                    bid_runtime.pack_reward(self.validator_commission, bid_runtime.system_balance)
                {
                    debug!("Failed to pack reward: {:?}", e);
                    return;
                }

                // Record greedy merge duration
                let greedy_merge_duration = greedy_merge_start.elapsed().as_secs_f64();
                self.mev_metrics.greedy_merge_duration_seconds.record(greedy_merge_duration);
                debug!(
                    "bidSimulator: greedy merge completed in {:.3}s, block_number: {}",
                    greedy_merge_duration, bid_runtime.bid.block_number
                );
            }
        }

        // Second commit: pay bid transaction (gas limit already includes space for this)
        if let Some(pay_bid_tx) = pay_bid_tx {
            block_gas_limit = block_gas_limit.saturating_add(PAY_BID_TX_GAS_LIMIT);
            let pay_bid_txs = vec![pay_bid_tx];
            if let Err(e) =
                bid_runtime.commit_transaction(pay_bid_txs, &mut builder, block_gas_limit)
            {
                debug!("Failed to commit pay bid transaction: {:?}", e);
                return;
            }
        } else {
            debug!("No pay bid transaction found, skipping bid");
            return;
        }

        // Finish the builder
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } =
            match builder.finish(&state_provider).map_err(PayloadBuilderError::other) {
                Ok(outcome) => outcome,
                Err(e) => {
                    debug!("Failed to finish builder: {:?}", e);
                    return;
                }
            };
        let mut sealed_block = Arc::new(block.sealed_block().clone());

        // Check if any un_revertible transaction failed
        // Receipts and transactions are in the same order, so we can zip them together
        let transactions: Vec<_> = sealed_block.body().transactions().collect();
        for (receipt, tx) in execution_result.receipts.iter().zip(transactions.iter()) {
            let tx_hash = *tx.hash();
            // Check if this is an un_revertible transaction that failed
            if !receipt.success && bid_runtime.un_revertible_set.contains(&tx_hash) {
                debug!(
                    "bidSimulator: un_revertible transaction failed, rejecting bid. tx_hash: {:?}, bid_hash: {:?}, block_number: {}",
                    tx_hash,
                    bid_runtime.bid.bid_hash,
                    bid_runtime.bid.block_number
                );
                return;
            }
        }

        // Update block_hash for all blob sidecars and insert into pool's blob store
        let block_hash = sealed_block.hash();
        for sidecar in bid_runtime.blob_sidecars.iter_mut() {
            sidecar.block_hash = block_hash;
        }

        let mut plain = sealed_block.clone_block();
        plain.body.sidecars = Some(bid_runtime.blob_sidecars.clone());
        sealed_block = Arc::new(plain.into());

        bid_runtime.bsc_payload = BscBuiltPayload {
            block: sealed_block.clone(),
            fees: bid_runtime.gas_fee,
            requests: Some(execution_result.requests.clone()),
            executed_block: ExecutedBlock {
                recovered_block: Arc::new(block.clone()),
                execution_output: Arc::new(ExecutionOutcome::new(
                    db.take_bundle(),
                    vec![execution_result.receipts.clone()],
                    sealed_block.header().number(),
                    vec![execution_result.requests.clone()],
                )),
                hashed_state: Arc::new(hashed_state.clone()),
            },
            executed_trie: Some(ExecutedTrieUpdates::Present(Arc::new(trie_updates))),
        };

        // Acquire write lock to update best_bid
        {
            let mut best_bid_map = self.best_bid.write();
            let best_bid = best_bid_map.get(&parent_hash);
            if let Some(best_bid) = best_bid {
                if best_bid.packed_block_reward < bid_runtime.packed_block_reward {
                    best_bid_map.insert(parent_hash, bid_runtime.clone());
                    success = true;
                } else {
                    debug!("current best bid is better than new bid, ignore");
                }
            } else {
                best_bid_map.insert(parent_hash, bid_runtime.clone());
                success = true;
            }
        }

        // Update metrics after simulation
        let sim_duration = sim_start.elapsed().as_secs_f64();
        self.mev_metrics.bid_simulation_duration_seconds.record(sim_duration);

        if is_first_bid {
            self.mev_metrics.first_bid_simulation_seconds.record(sim_duration);
        }

        if success {
            self.mev_metrics.valid_bids_total.increment(1);

            // Update best bid gas used (in MGas)
            let gas_used_mgas = bid_runtime.gas_used as f64 / 1_000_000.0;
            self.mev_metrics.best_bid_gas_used_mgas.set(gas_used_mgas);

            // Calculate simulation speed (MGas/s)
            if sim_duration > 0.0 {
                let mgasps = gas_used_mgas / sim_duration;
                self.mev_metrics.bid_simulation_speed_mgasps.set(mgasps);
            }
        } else {
            self.mev_metrics.invalid_bids_total.increment(1);
        }

        debug!("bidSimulator: sim_bid finished, block number:{}, parent hash:{}, builder:{}, bid hash:{}, gas used:{}, gas fee:{}, success:{}",
         bid_runtime.bid.block_number,
         bid_runtime.bid.parent_hash,
         bid_runtime.bid.builder,
         bid_runtime.bid.bid_hash,
         bid_runtime.gas_used,
         bid_runtime.gas_fee,
         success,
        );

        self.simulating_bid.write().remove(&parent_hash);
        bid_runtime.finished.store(true, Ordering::Relaxed);
        if !success {
            self.best_bid_to_run.write().remove(&parent_hash);
        }
    }

    /// Get the best bid for a given parent hash
    pub fn get_best_bid(&self, parent_hash: B256) -> Option<BidRuntime<Pool, BscEvmConfig>> {
        self.best_bid.read().get(&parent_hash).cloned()
    }
}

#[derive(Clone)]
pub struct BidRuntime<Pool, EvmConfig = BscEvmConfig> {
    pub bid: Bid,
    pub parent_snapshot: Arc<Snapshot>,
    pub mining_ctx: MiningContext,
    expected_block_reward: U256,
    expected_validator_reward: U256,
    packed_block_reward: U256,
    packed_validator_reward: U256,

    finished: Arc<AtomicBool>,
    pool: Pool,
    evm_config: EvmConfig,
    parent_header: SealedHeader,
    attributes: EthPayloadBuilderAttributes,
    builder_config: EthereumBuilderConfig,
    chain_spec: Arc<BscChainSpec>,
    pub bsc_payload: BscBuiltPayload,

    gas_used: u64,
    gas_fee: U256,
    system_balance: U256,
    blob_sidecars: Vec<BscBlobTransactionSidecar>,
    block_blob_count: u64,
    un_revertible_set: std::collections::HashSet<B256>,
}

impl<Pool, EvmConfig> BidRuntime<Pool, EvmConfig>
where
    Pool: reth::transaction_pool::TransactionPool<
            Transaction: reth::transaction_pool::PoolTransaction<Consensus = TransactionSigned>,
        > + Clone
        + 'static,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<
        BlockHeader = alloy_consensus::Header,
        SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>,
        Block = crate::node::primitives::BscBlock,
    >,
{
    fn new(
        bid: Bid,
        pool: Pool,
        evm_config: EvmConfig,
        attributes: EthPayloadBuilderAttributes,
        chain_spec: Arc<BscChainSpec>,
        mining_ctx: MiningContext,
    ) -> Self {
        // Convert un_revertible array to HashSet for fast lookup
        let un_revertible_set: std::collections::HashSet<B256> =
            bid.un_revertible.iter().copied().collect();

        Self {
            bid,
            pool,
            evm_config,
            builder_config: EthereumBuilderConfig::default(),
            bsc_payload: BscBuiltPayload::default(),
            expected_block_reward: U256::ZERO,
            expected_validator_reward: U256::ZERO,
            packed_block_reward: U256::ZERO,
            packed_validator_reward: U256::ZERO,
            parent_header: mining_ctx.parent_header.clone(),
            attributes,
            gas_used: 0,
            gas_fee: U256::ZERO,
            system_balance: U256::ZERO,
            block_blob_count: 0,
            finished: Arc::new(AtomicBool::new(false)),
            chain_spec,
            blob_sidecars: Vec::new(),
            parent_snapshot: mining_ctx.parent_snapshot.clone(),
            mining_ctx,
            un_revertible_set,
        }
    }

    fn is_expected_better_than(&self, ohter: &BidRuntime<Pool, EvmConfig>) -> bool {
        self.expected_block_reward >= ohter.expected_block_reward
            && self.expected_validator_reward >= ohter.expected_validator_reward
    }

    fn commit_transaction<B>(
        &mut self,
        bid_txs: Vec<TransactionSigned>,
        builder: &mut B,
        block_gas_limit: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        B: BlockBuilder,
        B::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    {
        let recovered_txs: Result<Vec<_>, _> =
            bid_txs.into_iter().map(|tx| tx.try_into_recovered()).collect();

        let recovered_txs = match recovered_txs {
            Ok(txs) => txs,
            Err(err) => {
                debug!("Failed to recover transaction signature: {:?}", err);
                return Err("Failed to recover transaction signature".into());
            }
        };
        self.commit_transaction_recovered(recovered_txs, builder, block_gas_limit, false, 0)
    }

    fn commit_transaction_recovered<B>(
        &mut self,
        recovered_txs: Vec<reth_primitives_traits::Recovered<TransactionSigned>>,
        builder: &mut B,
        block_gas_limit: u64,
        from_pool: bool,
        delay_ms: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        B: BlockBuilder,
        B::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    {
        let base_fee: u64 = builder.evm().block().basefee;
        let blob_params = self.chain_spec.blob_params_at_timestamp(self.attributes.timestamp());
        let max_blob_count =
            blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();

        let start_time = std::time::Instant::now();
        let delay_duration = std::time::Duration::from_millis(delay_ms);

        for (index, recovered_tx) in recovered_txs.into_iter().enumerate() {
            if from_pool {
                let elapsed = start_time.elapsed();
                if elapsed >= delay_duration {
                    trace!("Time limit reached ({}ms), processed {} transactions", delay_ms, index);
                    break;
                }
                if block_gas_limit - self.gas_used < TX_GAS {
                    trace!("block_gas_limit - gas_used < TX_GAS, break");
                    break;
                }
            }
            // Check interrupt flag before processing each transaction
            if self.bid.interrupt_flag.load(Ordering::Relaxed) {
                debug!("Bid runtime interrupted before processing transaction");
                return Err("bid runtime interrupted".into());
            }
            let is_blob_tx = recovered_tx.is_eip4844();
            let tx_hash = *recovered_tx.hash();
            if from_pool {
                // ensure we still have capacity for this transaction
                if self.gas_used + recovered_tx.gas_limit() > block_gas_limit {
                    // we can't fit this transaction into the block, so we need to mark it as invalid
                    // which also removes all dependent transaction from the iterator before we can
                    // continue
                    trace!("bidSimulator: gas limit exceeded, ignore tx:{}, tx gas limit:{}, block gas limit:{}, runtime gasused:{}", tx_hash, recovered_tx.gas_limit(), block_gas_limit, self.gas_used);
                    continue;
                }
            }

            // Check blob transaction limits and retrieve sidecar if needed
            if let Some(blob_tx) = recovered_tx.as_eip4844() {
                let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;

                if self.block_blob_count + tx_blob_count > max_blob_count {
                    if from_pool {
                        trace!("bidSimulator: blob transaction limit exceeded, ignore tx:{}, tx blob count:{}, block blob count:{}, max blob count:{}", tx_hash, tx_blob_count, self.block_blob_count, max_blob_count);
                        continue;
                    }
                    debug!(target: "payload_builder", tx=?tx_hash, ?self.block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
                    return Err("blob transaction limit exceeded".into());
                }

                self.block_blob_count += tx_blob_count;
            }

            let tx_gas_used = match builder.execute_transaction(recovered_tx.clone()) {
                Ok(tx_gas_used) => tx_gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error,
                    ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        debug!(target: "payload_builder", %error, ?recovered_tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        debug!(target: "payload_builder", %error, ?recovered_tx, "skipping invalid transaction and its descendants");
                    }
                    if from_pool {
                        trace!("bidSimulator: invalid transaction, ignore tx:{}, error:{}, recovered tx:{:?}", tx_hash, error, recovered_tx);
                        continue;
                    }
                    return Err("invalid transaction".into());
                }
                Err(err) => {
                    if from_pool {
                        trace!("bidSimulator: invalid transaction, ignore tx:{}, error:{}, recovered tx:{:?}", tx_hash, err, recovered_tx);
                        continue;
                    }
                    return Err(Box::new(PayloadBuilderError::evm(err)));
                }
            };

            if is_blob_tx {
                // Get sidecar from bid.blob_sidecars if available and convert to BscBlobTransactionSidecar
                if let Some(sidecar) = self.bid.blob_sidecars.get(&tx_hash) {
                    // Insert blob sidecar into pool's blob store
                    use alloy_eips::eip7594::BlobTransactionSidecarVariant;
                    if let Err(e) = self.pool.insert_blob(
                        tx_hash,
                        BlobTransactionSidecarVariant::Eip4844(sidecar.clone()),
                    ) {
                        debug!("Failed to insert blob sidecar for tx {:?}: {:?}", tx_hash, e);
                        if from_pool {
                            trace!("bidSimulator: failed to insert blob sidecar, ignore tx:{}, error:{}, recovered tx:{:?}", tx_hash, e, recovered_tx);
                            continue;
                        }
                        return Err("Failed to insert blob sidecar".into());
                    }
                    let bsc_sidecar = BscBlobTransactionSidecar {
                        inner: sidecar.clone(),
                        block_number: self.bid.block_number,
                        block_hash: B256::ZERO, // Will be set when block is sealed
                        tx_index: index as u64,
                        tx_hash,
                    };
                    self.blob_sidecars.push(bsc_sidecar);
                }
            }

            self.gas_used += tx_gas_used;
            let tx_effective_gas_price = recovered_tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            self.gas_fee += (U256::from(tx_effective_gas_price) + U256::from(base_fee))
                * U256::from(tx_gas_used);
            self.system_balance += U256::from(tx_effective_gas_price) * U256::from(tx_gas_used);
        }

        Ok(())
    }

    fn pack_reward(
        &mut self,
        validator_commission: u64,
        system_balance: U256,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.packed_block_reward = system_balance;
        self.packed_validator_reward =
            self.packed_block_reward * U256::from(validator_commission) / U256::from(10000u64);
        self.packed_validator_reward -= self.bid.builder_fee;
        Ok(())
    }

    fn valid_reward(&self) -> bool {
        self.packed_block_reward >= self.expected_block_reward
            && self.packed_validator_reward >= self.expected_validator_reward
    }

    fn fill_tx_from_pool<B>(
        &mut self,
        builder: &mut B,
        bid_txs: Vec<reth_primitives::TransactionSigned>,
        block_gas_limit: u64,
        delay_ms: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        B: BlockBuilder,
        B::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    {
        let base_fee = builder.evm_mut().block().basefee;
        let mut blob_fee = None;

        if BscHardforks::is_cancun_active_at_timestamp(
            &self.chain_spec,
            self.parent_header.number,
            self.parent_header.timestamp,
        ) {
            if let Some(excess) = self.mining_ctx.header.as_ref().unwrap().excess_blob_gas {
                if excess != 0 {
                    blob_fee = Some(calc_blob_fee(
                        &self.chain_spec,
                        self.mining_ctx.header.as_ref().unwrap(),
                    ));
                }
            }
        }
        debug!("fill_tx_from_pool: base_fee={}", base_fee);
        let best_tx_list: Vec<_> = self
            .pool
            .best_transactions_with_attributes(BestTransactionsAttributes::new(
                base_fee,
                blob_fee.map(|fee| fee as u64),
            ))
            .collect();
        debug!("fill_tx_from_pool: best_tx_list.len={}", best_tx_list.len());

        let bid_tx_hashes: std::collections::HashSet<B256> =
            bid_txs.iter().map(|tx| *tx.hash()).collect();

        let mut sender_txs_map: HashMap<
            Address,
            Vec<Arc<reth::transaction_pool::ValidPoolTransaction<Pool::Transaction>>>,
        > = HashMap::new();

        for pool_tx in best_tx_list {
            sender_txs_map.entry(pool_tx.sender()).or_insert_with(Vec::new).push(pool_tx);
        }

        for (_sender, txs) in sender_txs_map.iter_mut() {
            for i in (0..txs.len()).rev() {
                let tx_hash = txs[i].hash();
                if bid_tx_hashes.contains(tx_hash) {
                    txs.drain(0..=i);
                    break;
                }
            }
        }

        let pending_txs: Vec<reth_primitives_traits::Recovered<TransactionSigned>> =
            sender_txs_map.into_values().flatten().map(|pool_tx| pool_tx.to_consensus()).collect();
        debug!("fill_tx_from_pool: pending_txs.len={}", pending_txs.len());

        let result = self.commit_transaction_recovered(
            pending_txs,
            builder,
            block_gas_limit,
            true,
            delay_ms,
        );
        if let Err(e) = result {
            debug!("Failed to commit transactions: {:?}", e);
            return Err(e);
        }
        Ok(())
    }
}
