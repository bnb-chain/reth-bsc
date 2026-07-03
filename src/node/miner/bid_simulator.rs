use crate::{
    chainspec::BscChainSpec,
    consensus::{
        eip4844::{calc_blob_fee, is_blob_eligible_block},
        parlia::{provider::SnapshotProvider, util::calculate_millisecond_timestamp, Snapshot},
    },
    hardforks::BscHardforks,
    node::{
        engine::BscBuiltPayload,
        evm::config::{BscEvmConfig, BscNextBlockEnvAttributes, ValidatorCacheSink},
        miner::{
            bid_block::{simulate_bid_block, BidBlockTask, DecodedBidBlock},
            bsc_miner::MiningContext,
            payload::DELAY_LEFT_OVER,
            util::prepare_new_attributes,
        },
        primitives::BscBlobTransactionSidecar,
    },
};
use alloy_consensus::{BlobTransactionSidecar, BlockHeader as _, Transaction};
use alloy_eips::eip4895::Withdrawals;
use alloy_evm::Evm;
use alloy_primitives::{Address, Bytes, B256, U256};
use either::Either;
use parking_lot::RwLock;
use reth::transaction_pool::BestTransactionsAttributes;
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    execute::{BlockBuilder, BlockBuilderOutcome, BlockExecutionError, BlockValidationError},
    ConfigureEvm, NextBlockEnvAttributes,
};
use reth_execution_types::BlockExecutionOutput;
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth_payload_primitives::{BuiltPayloadExecutedBlock, PayloadBuilderError};
use reth_primitives_traits::{SealedHeader, SignerRecoverable};
use reth_provider::{BlockHashReader, HeaderProvider, StateProviderFactory};
use reth_revm::{database::StateProviderDatabase, db::State};
use revm::context_interface::Block as EvmBlock;
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tracing::{debug, trace};
const NO_INTERRUPT_LEFT_OVER: u64 = 500;
const PAY_BID_TX_GAS_LIMIT: u64 = 25000;
const TX_GAS: u64 = 21000;

#[derive(Clone)]
pub struct Bid {
    pub builder: Address,
    pub block_number: u64,
    pub parent_hash: B256,
    pub txs: Vec<TransactionSigned>,
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

/// Evicts entries older than `min_block_number` from the `best_bid_block` map, keyed by parent
/// hash. Split out as a free function (from [`BidSimulator::clear`]) so it's directly testable
/// without constructing a full `BidSimulator`.
fn retain_recent_bid_blocks(map: &mut HashMap<B256, BidBlockTask>, min_block_number: u64) {
    map.retain(|_, task| task.block.sealed_block().header().number() >= min_block_number);
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
    /// Best executed BEP-675 BidBlock payload per parent hash (go-bsc
    /// `AddBidBlock`/`GetBestBidBlock`).
    best_bid_block: Arc<RwLock<HashMap<B256, BidBlockTask>>>,
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
            best_bid_block: Arc::new(RwLock::new(HashMap::new())),
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
        let parent_header = match self.client.header(parent_hash) {
            Ok(Some(header)) => SealedHeader::new(header, parent_hash),
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
            block_timestamp_ms: 0,
            end_mining_timestamp_ms: 0,
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

        // Clear old BEP-675 BidBlocks (go-bsc `clearLoop` prunes `bestBidBlock[parentHash]` the
        // same way). Without this, every parent hash that ever received an admitted BidBlock keeps
        // its full sealed block — including blob data — in memory forever.
        retain_recent_bid_blocks(&mut self.best_bid_block.write(), min_block_number);

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
        attributes: EthPayloadAttributes,
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

        let state_provider = match self.client.state_by_block_hash(bid_runtime.parent_header.hash())
        {
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

        // Sinks transport current_validators / turn_length from the builder so that
        // pick_best_payload() can write to VALIDATOR_CACHE / TURN_LENGTH_CACHE with the
        // definitive block hash after finalize_new_header() runs.
        let bid_validator_cache_sink: ValidatorCacheSink = Arc::new(Mutex::new(None));
        let bid_turn_length_sink: Arc<Mutex<Option<u8>>> = Arc::new(Mutex::new(None));

        let mut builder = match evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                BscNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp,
                        suggested_fee_recipient: attributes.suggested_fee_recipient,
                        prev_randao: attributes.prev_randao,
                        gas_limit,
                        parent_beacon_block_root: attributes.parent_beacon_block_root,
                        withdrawals: attributes
                            .withdrawals
                            .as_ref()
                            .map(|w| Withdrawals::new(w.clone())),
                        extra_data: builder_config.extra_data.clone(),
                        slot_number: None,
                    },
                    validator_cache_sink: Some(bid_validator_cache_sink.clone()),
                    turn_length_sink: Some(bid_turn_length_sink.clone()),
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
            builder.evm_mut().block().gas_limit().saturating_sub(system_txs_gas);
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

        // Finish the builder. Bid simulation does not run alongside a sparse-trie task —
        // no precomputed root, so the builder falls through to state_root_with_updates.
        let out = match builder.finish(&state_provider, None).map_err(PayloadBuilderError::other) {
            Ok(outcome) => outcome,
            Err(e) => {
                debug!("Failed to finish builder: {:?}", e);
                return;
            }
        };
        let BlockBuilderOutcome { execution_result, hashed_state, trie_updates, block } = out;
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
        plain.body.sidecars = if bid_runtime.blob_sidecars.is_empty() {
            None
        } else {
            Some(bid_runtime.blob_sidecars.clone())
        };
        sealed_block = Arc::new(plain.into());

        let requests = execution_result.requests.clone();
        let execution_outcome =
            BlockExecutionOutput { state: db.take_bundle(), result: execution_result };
        let executed: BuiltPayloadExecutedBlock<_> = BuiltPayloadExecutedBlock {
            recovered_block: Arc::new(block.clone()),
            execution_output: Arc::new(execution_outcome),
            hashed_state: Either::Left(Arc::new(hashed_state)),
            trie_updates: Either::Left(Arc::new(trie_updates)),
        };
        let executed_block = executed.into_executed_payload();

        // Read validator/turn-length data transported via sinks from the now-consumed builder.
        let pending_validators = bid_validator_cache_sink.lock().unwrap().take();
        let pending_turn_length = bid_turn_length_sink.lock().unwrap().take();

        bid_runtime.bsc_payload = Some(BscBuiltPayload {
            block: sealed_block.clone(),
            fees: bid_runtime.gas_fee,
            requests: Some(requests),
            build_kind: crate::node::engine::BuildKind::NormalAttempt,
            exec_duration: std::time::Duration::ZERO,
            trie_root_duration: std::time::Duration::ZERO,
            executed_block,
            pending_validators,
            pending_turn_length,
            is_bid: true,
        });

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

    /// Verify, blind-sign, and seal an admitted BEP-675 BidBlock, keeping the highest-fee one per
    /// parent hash (go-bsc `AddBidBlock`). Execution and selection against the locally-built block
    /// happen in the build cycle (a follow-on slice); this is the intake half.
    pub fn commit_bid_block(&self, decoded: DecodedBidBlock) {
        let parent_hash = decoded.parent_hash();
        // go-bsc's `newBidBlockLoop` also re-checks `isRunning` / `receivingBid` here. We don't:
        // `is_mev_running()` is already enforced at admission in `MevApiImpl::admit_bid_block`, and
        // `bid_receiving` has no toggle in this codebase (no setter, no RPC) so the gate would be
        // dead code. A bid that races past `is_mev_running` between admit and pop is harmless —
        // the resulting payload sits in `best_bid_block` until evicted.
        //
        // Mirrors go-bsc `bidSimulator.newBidBlockLoop`: discard BidBlocks for a block number we
        // have already passed. Admission only checks head-relative timing, not block number, so a
        // bid admitted just before the head advanced can still reach here for a stale block.
        let head_number = self.client.last_block_number().unwrap_or(0);
        if decoded.block_number() <= head_number {
            debug!(
                "BidBlock: discard stale block, blockNumber={}, latestBlock={}, builder={}",
                decoded.block_number(),
                head_number,
                decoded.builder,
            );
            return;
        }
        let parent = match self.client.header(parent_hash) {
            Ok(Some(h)) => SealedHeader::new(h, parent_hash),
            _ => {
                debug!("BidBlock: parent header not found: {parent_hash}");
                return;
            }
        };
        let Some(parent_snap) = self.snapshot_provider.snapshot_by_hash(&parent_hash) else {
            debug!("BidBlock: no snapshot for parent {parent_hash}");
            return;
        };
        let gas_ceil = crate::shared::get_miner_gas_limit().unwrap_or(parent.gas_limit);
        let expected_gas_limit =
            EthereumBuilderConfig::new().with_gas_limit(gas_ceil).gas_limit(parent.gas_limit);
        // Use the operator-configured extra (`miner_setExtra`) as the block vanity, mirroring
        // geth's `worker.extra`; pad/truncate to the Parlia vanity length, defaulting to zeros.
        let vanity = {
            let mut v = crate::shared::get_miner_extra().map(|e| e.to_vec()).unwrap_or_default();
            v.resize(crate::consensus::parlia::EXTRA_VANITY_LEN, 0u8);
            Bytes::from(v)
        };
        let block_timestamp_ms = calculate_millisecond_timestamp(&decoded.header);

        let task = match simulate_bid_block(
            self.parlia.clone(),
            &self.chain_spec,
            &decoded,
            &parent,
            &parent_snap,
            &self.snapshot_provider,
            self.validator_address,
            expected_gas_limit,
            vanity,
            block_timestamp_ms,
        ) {
            Ok(task) => task,
            Err(e) => {
                debug!("BidBlock rejected in simulate: {e}");
                // go-bsc only revokes on blob-tx validation failure here
                // (`errInvalidBidBlockBlobTx` in `prepareBidBlockTask`); other
                // prepare failures are plain rejections. Our KZG blob verification
                // isn't ported yet, so there is no revoke-worthy variant to match
                // on — the dishonest-builder revoke lives at the state-root check below.
                return;
            }
        };

        // BEP-675 zero-simulate: do NOT execute here. Keep the highest-fee sealed BidBlock per
        // parent (go-bsc `AddBidBlock`). Execution + state-root verification are deferred until the
        // block has been selected and broadcast — see `ImportService::on_new_bid_block` — matching
        // go-bsc's broadcast-then-`InsertChain` flow in `handleBidBlockResult`. Selection is by the
        // deposit-derived `gas_fee`, which needs no execution.
        let mut best = self.best_bid_block.write();
        let replace = best.get(&parent_hash).is_none_or(|t| task.gas_fee > t.gas_fee);
        if replace {
            best.insert(parent_hash, task);
        }
    }

    /// The best stored (sealed, unexecuted) BidBlock for a parent hash (go-bsc `GetBestBidBlock`).
    ///
    /// The returned [`BidBlockTask`] is fully blind-signed and sealed but **not** executed: under
    /// BEP-675 zero-simulate the validator verifies the state root only after broadcasting.
    pub fn best_bid_block(&self, parent_hash: B256) -> Option<BidBlockTask> {
        self.best_bid_block.read().get(&parent_hash).cloned()
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
    attributes: EthPayloadAttributes,
    builder_config: EthereumBuilderConfig,
    chain_spec: Arc<BscChainSpec>,
    pub bsc_payload: Option<BscBuiltPayload>,

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
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = BscNextBlockEnvAttributes> + 'static,
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
        attributes: EthPayloadAttributes,
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
            bsc_payload: None,
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
        self.expected_block_reward >= ohter.expected_block_reward &&
            self.expected_validator_reward >= ohter.expected_validator_reward
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
        let base_fee: u64 = builder.evm().block().basefee();
        let blob_params = self.chain_spec.blob_params_at_timestamp(self.attributes.timestamp);
        let header = self.mining_ctx.header.as_ref().unwrap();
        let blob_eligible =
            is_blob_eligible_block(&self.chain_spec, header.number, header.timestamp);
        let mut max_blob_count =
            blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
        if !blob_eligible {
            max_blob_count = 0;
        }

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
            if is_blob_tx && !blob_eligible {
                if from_pool {
                    continue;
                }
                return Err("blob transactions not allowed in this block".into());
            }
            if from_pool {
                // ensure we still have capacity for this transaction
                if self.gas_used + recovered_tx.gas_limit() > block_gas_limit {
                    // we can't fit this transaction into the block, so we need to mark it as
                    // invalid which also removes all dependent transaction from
                    // the iterator before we can continue
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
                // Get sidecar from bid.blob_sidecars if available and convert to
                // BscBlobTransactionSidecar
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
                        version: 0,
                    };
                    self.blob_sidecars.push(bsc_sidecar);
                }
            }

            self.gas_used += tx_gas_used.tx_gas_used();
            let tx_effective_gas_price = recovered_tx
                .effective_tip_per_gas(base_fee)
                .expect("fee is always valid; execution succeeded");
            self.gas_fee += (U256::from(tx_effective_gas_price) + U256::from(base_fee)) *
                U256::from(tx_gas_used.tx_gas_used());
            self.system_balance +=
                U256::from(tx_effective_gas_price) * U256::from(tx_gas_used.tx_gas_used());
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
        self.packed_block_reward >= self.expected_block_reward &&
            self.packed_validator_reward >= self.expected_validator_reward
    }

    fn fill_tx_from_pool<B>(
        &mut self,
        builder: &mut B,
        bid_txs: Vec<TransactionSigned>,
        block_gas_limit: u64,
        delay_ms: u64,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
    where
        B: BlockBuilder,
        B::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    {
        let base_fee = builder.evm_mut().block().basefee();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::primitives::{BscBlock, BscBlockBody};
    use reth_primitives_traits::RecoveredBlock;

    fn bid_block_task_at(number: u64, parent_hash: B256) -> BidBlockTask {
        let header = alloy_consensus::Header { number, parent_hash, ..Default::default() };
        let body = BscBlockBody {
            inner: reth_ethereum_primitives::BlockBody {
                transactions: Vec::new(),
                ommers: Vec::new(),
                withdrawals: None,
            },
            sidecars: None,
        };
        BidBlockTask {
            block: RecoveredBlock::new_unhashed(BscBlock { header, body }, Vec::new()),
            gas_fee: U256::from(1),
            system_tx_start: 0,
            builder: Address::ZERO,
            bid_hash: B256::random(),
        }
    }

    #[test]
    fn retain_recent_bid_blocks_evicts_stale_entries_by_block_number() {
        let mut map = HashMap::new();
        let old_parent = B256::repeat_byte(0x11);
        let recent_parent = B256::repeat_byte(0x22);
        map.insert(old_parent, bid_block_task_at(100, B256::random()));
        map.insert(recent_parent, bid_block_task_at(110, B256::random()));

        // min_block_number = 105: the block-100 entry is stale and must be evicted; the
        // block-110 entry is recent and must survive.
        retain_recent_bid_blocks(&mut map, 105);

        assert!(!map.contains_key(&old_parent), "stale BidBlock must be evicted");
        assert!(map.contains_key(&recent_parent), "recent BidBlock must be retained");
    }

    #[test]
    fn retain_recent_bid_blocks_keeps_entry_at_exact_threshold() {
        let mut map = HashMap::new();
        let parent = B256::repeat_byte(0x33);
        map.insert(parent, bid_block_task_at(105, B256::random()));

        retain_recent_bid_blocks(&mut map, 105);

        assert!(map.contains_key(&parent), "entry exactly at the threshold must be retained");
    }

    #[test]
    fn retain_recent_bid_blocks_is_a_noop_on_empty_map() {
        let mut map: HashMap<B256, BidBlockTask> = HashMap::new();
        retain_recent_bid_blocks(&mut map, 1_000);
        assert!(map.is_empty());
    }
}
