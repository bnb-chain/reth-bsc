use alloy_consensus::Transaction;
use alloy_primitives::U256;
use alloy_evm::Evm;
use crate::node::evm::config::BscEvmConfig;
use reth_provider::StateProviderFactory;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm::execute::BlockBuilder;
use reth_payload_primitives::{PayloadBuilderError};
use reth_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use tracing::debug;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use std::sync::Arc;
use reth::payload::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use crate::chainspec::{BscChainSpec};
use std::collections::HashMap;
use alloy_primitives::{Address, B256};
use reth_primitives::SealedHeader;
use reth_provider::StateProvider;
use crate::consensus::SYSTEM_ADDRESS;

#[derive(Clone)]
pub struct Bid {
    pub builder: Address,
    pub block_number: u64,
    pub parent_hash: B256,
    pub txs: Vec<reth_primitives::TransactionSigned>,
    pub gas_used: u64,
    pub gas_fee: U256,
    pub builder_fee: U256,
    pub committed: bool,
    pub bid_hash: B256,
}

impl Bid
{
    fn is_committed(&self) -> bool {
        return self.committed;
    }
}

pub struct NewBidPackage {
    pub bid: Bid,
    pub runtime: u64,
    pub bid_value: u64,
}   

// bid loop receive bid from client and commit bid to simulator
// 1. last block number check
// 2. pack bid runtime and calculate bid value
// 3. find best bid
// 4. can be interrupt the last bid and commit
pub struct BidSimulator<Client> {
    client: Client,
    // bid to run, the best bid to run
    best_bid_to_run: HashMap<B256, Bid>,
    simulating_bid: HashMap<B256, Bid>,
    best_bid: HashMap<B256, BidRuntime<BscEvmConfig>>,
    pending_bid: HashMap<String, u8>,
    bid_receiving: bool,
    chain_spec: Arc<BscChainSpec>,
    min_gas_price: U256,
    //max_bid_pre_builder: u64,
}

impl<Client> BidSimulator<Client> 
where Client: StateProviderFactory + Clone + 'static,
{
    pub fn new(client: Client, chain_spec: Arc<BscChainSpec>) -> Self {
        Self { 
            client,
            chain_spec,
            best_bid_to_run: HashMap::new(),
            simulating_bid: HashMap::new(),
            best_bid: HashMap::new(),
            pending_bid: HashMap::new(),
            bid_receiving: true,
            min_gas_price: U256::ZERO,
           // max_bid_pre_builder: 10,
        }   
    }

    pub fn check_pending_bid(&mut self, block_number: u64, builder: Address, bid_hash: B256) -> bool{
        let key = format!("{}-{}-{}", block_number, builder, bid_hash);
        if let Some(exist) = self.pending_bid.get(&key) {
            if *exist > 0 {
                return false;
            }
        }
        // todo: check pre builder max bid count
        return true;
    }

    pub fn add_pending_bid(&mut self, block_number: u64, builder: Address, bid_hash: B256) {
        let key = format!("{}-{}-{}", block_number, builder, bid_hash);
        self.pending_bid.insert(key, 1);
    }

    pub fn commit_new_bid(&mut self, bid: NewBidPackage) {
        debug!("commit_new_bid:{}", bid.bid.bid_hash);
        self.add_pending_bid(bid.bid.block_number, bid.bid.builder, bid.bid.bid_hash);
        let final_block_number   = match self.client.finalized_block_number() {
            Ok(Some(final_block_number)) => final_block_number,
            Ok(None) => return,
            Err(_) => return,
        };
        
        if bid.bid.block_number <= final_block_number {
            // Bid is for a block that's already finalized, ignore it
            // todo: async clear
            self.clear(bid.bid.block_number, bid.bid.bid_hash);
            return;
        }


        let parent_hash = bid.bid.parent_hash;

        let mut _bid_runtime = match self.new_bid_runtime(&bid.bid, 0) {
            Ok(bid_runtime   ) => bid_runtime,
            Err(err) => {
                debug!("create runtime error:{}",err);
                return;
            }
        };
        let mut to_commit = true;
        let mut _bid_accepted = true;
        if let Some(best_bid) = self.best_bid_to_run.get(&parent_hash).cloned() {
            let best_bid_runtime = match self.new_bid_runtime(&best_bid, 0) {
                Ok(best_bid_runtime) => best_bid_runtime,
                Err(_) => {
                    return;
                }
            };
            if _bid_runtime.is_expected_better_than(&best_bid_runtime) {
                debug!("new bid has better expectedBlockReward builder:{}, bid_hash:{}", _bid_runtime.bid.builder,"");
            } else if best_bid.is_committed() {
                _bid_runtime = best_bid_runtime;
                _bid_accepted = false;
                debug!("discard new bid and to simulate the non-committed bestBidToRun builder:{}, bid_hash:{}", _bid_runtime.bid.builder,"");
            }else {
                to_commit = false;
                _bid_accepted = false;
                debug!("new bid will be discarded builder:{}, bid_hash:{}",  _bid_runtime.bid.builder,"");
            }
        }

        if to_commit {
            self.best_bid_to_run.insert(_bid_runtime.bid.parent_hash, _bid_runtime.bid.clone());
            // todo: can be interrupted
            // if let Some(simulating_bid) = self.simulating_bid.get(&bid.bid.parent_hash) {

            // }
            self.commit_bid(5,&mut _bid_runtime)

        }
    }

    fn clear(&mut self, block_number: u64, _block_hash: B256) {
        let clear_threshold = 5; //todo: config
        let min_block_number = block_number.saturating_sub(clear_threshold);

        // Clear old bids from best_bid_to_run, simulating_bid, and best_bid
        self.best_bid_to_run.retain(|_, bid| bid.block_number >= min_block_number);
        self.simulating_bid.retain(|_, bid| bid.block_number >= min_block_number);
        self.best_bid.retain(|_, bid| bid.bid.block_number >= min_block_number);

        // Clear old pending bids by parsing block_number from key prefix
        // Key format: "{block_number}-{builder}-{bid_hash}"
        self.pending_bid.retain(|key, _| {
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
    }

    fn new_bid_runtime(&self, _bid: &Bid, _validator_commission: u64) -> Result<BidRuntime<BscEvmConfig>, Box<dyn std::error::Error + Send + Sync>>{
        let mut runtime = BidRuntime::new(_bid.clone(), BscEvmConfig::new(self.chain_spec.clone()));
        let expected_block_reward = _bid.gas_fee;
        let mut expected_validator_reward = expected_block_reward * U256::from(_validator_commission);
        expected_validator_reward = expected_validator_reward / U256::from(10000u64);

        if expected_validator_reward < _bid.builder_fee {
            debug!("BidSimulator: invalid bid, builder fee exceeds validator reward, ignore");
            return Err("invalid bid: builder fee exceeds validator reward".into());
        }
        expected_validator_reward = expected_validator_reward - _bid.builder_fee;
        runtime.expected_block_reward = expected_block_reward;
        runtime.expected_validator_reward = expected_validator_reward;
        Ok(runtime)
    }

    fn commit_bid(&mut self,reason: u32, bid_runtime: &mut BidRuntime<BscEvmConfig>) {
        // todo: interrupt
        debug!("bid committed reason:{}, bid hash:{}",reason, "");
        bid_runtime.bid.committed = true;
        self.sim_bid(bid_runtime);
    }

    // sim_bid commit tx and set best bid
    fn sim_bid(&mut self, bid_runtime: &mut BidRuntime<BscEvmConfig>) {
        if !self.bid_receiving {
            return 
        }
        let mut success = false;
        //let startTs = std::time::Instant::now();
        let parent_hash = bid_runtime.bid.parent_hash;
        self.simulating_bid.insert(parent_hash, bid_runtime.bid.clone());
        
        // todo: gas check


        let mut txs_except_last = bid_runtime.bid.txs.clone();
        let pay_bid_tx = txs_except_last.pop();
        let state_provider = self.client.state_by_block_hash(bid_runtime.parent_header.hash_slow()).unwrap();
        let sp_db = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder()
            .with_database(sp_db)
            .with_bundle_update()
            .build();

        // Clone necessary fields to avoid borrow conflicts
        let evm_config = bid_runtime.evm_config.clone();
        let parent_header = bid_runtime.parent_header.clone();
        let attributes = bid_runtime.attributes.clone();
        let builder_config = bid_runtime.builder_config.clone();
        
        let mut builder = evm_config.builder_for_next_block(&mut db, &parent_header, NextBlockEnvAttributes {
                timestamp:        attributes.timestamp(),
                suggested_fee_recipient: attributes.suggested_fee_recipient(),
                prev_randao:      attributes.prev_randao(),
                gas_limit:        builder_config.gas_limit(parent_header.gas_limit),
                parent_beacon_block_root: attributes.parent_beacon_block_root(),
                withdrawals:     Some(attributes.withdrawals().clone()),
            }).map_err(PayloadBuilderError::other).unwrap();
        builder.apply_pre_execution_changes().map_err(PayloadBuilderError::other).unwrap();
        
        // First commit: bid transactions
        bid_runtime.commit_transaction(txs_except_last, &mut builder);

        // if let Some(payBidTx) = payBidTx {
        //     bid_runtime.commit_transaction(payBidTx, bid_runtime.parent_header, bid_runtime.attributes, bid_runtime.builder_config);
        // }
        // todo: check whether time `NoInterruptLeftOver-delayLeftOver` is enough for simulating
        if let Err(e) = bid_runtime.pack_reward(5, &state_provider) {
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
                debug!("bid gas price is lower than min gas price, bid:{}, min:{}", bid_gas_price, self.min_gas_price);
                return;
            }
        }
        // todo: if enable greedy merge, fill bid env with transactions from mempool

        // Second commit: pay bid transaction
        let mut pay_bid_txs = Vec::new();
        pay_bid_txs.push(pay_bid_tx.unwrap());
        bid_runtime.commit_transaction(pay_bid_txs, &mut builder);
        
        // Finish the builder
        let _outcome = builder.finish(&state_provider).map_err(PayloadBuilderError::other).unwrap();


        let best_bid = self.best_bid.get(&parent_hash);
        if let Some(best_bid) = best_bid {
            if best_bid.packed_block_reward < bid_runtime.packed_block_reward {
                self.best_bid.insert(parent_hash, bid_runtime.clone());
                success = true;
            }
        }else {
            self.best_bid.insert(parent_hash, bid_runtime.clone());
            success = true;
        }

        debug!("bidSimulator: sim_bid finished, block number:{}, parent hash:{}, builder:{}, bid hash:{}, gas used:{}",
         bid_runtime.bid.block_number,
         bid_runtime.bid.parent_hash,
         bid_runtime.bid.builder,
         "",
         bid_runtime.gas_used,
        );

        self.simulating_bid.remove(&parent_hash);
        if !success {
            self.best_bid_to_run.remove(&parent_hash);
        }
        // todo: recommit

    }
}

#[derive(Clone)]
pub struct BidRuntime<EvmConfig = BscEvmConfig> {
    bid: Bid,
    expected_block_reward: U256,
    expected_validator_reward: U256,
    packed_block_reward: U256,
    packed_validator_reward: U256,

    //finished: bool,
    // todo: duration

    // evn
    evm_config: EvmConfig,
    parent_header: SealedHeader,
    attributes: EthPayloadBuilderAttributes,
    builder_config: EthereumBuilderConfig,
    
    gas_used: u64,
    gas_fee: U256,
}

impl<EvmConfig> BidRuntime<EvmConfig> 
where 
EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes> + 'static,
<EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<BlockHeader = alloy_consensus::Header, SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>, Block = crate::node::primitives::BscBlock>,
{
    fn new(bid: Bid, evm_config: EvmConfig) -> Self {

        Self {
            bid,
            evm_config,
            builder_config: EthereumBuilderConfig::default(),
            expected_block_reward: U256::ZERO,
            expected_validator_reward: U256::ZERO,
            packed_block_reward: U256::ZERO,
            packed_validator_reward: U256::ZERO,
            parent_header: SealedHeader::default(),
            attributes: EthPayloadBuilderAttributes::default(),
            gas_used: 0,
            gas_fee: U256::ZERO,
        }
    }

    fn is_expected_better_than(&self, ohter: &BidRuntime<EvmConfig>) -> bool {
        if self.expected_block_reward >= ohter.expected_block_reward {
            if self.expected_validator_reward >= ohter.expected_validator_reward {
                return true;
            }
        }
        return false;
    }

    fn commit_transaction<B>(&mut self, bid_txs: Vec<TransactionSigned>, builder: &mut B)
    where
        B: BlockBuilder,
        B::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    {
        let mut gas_used: u64 = 0;
        let mut gas_fee: U256 = U256::ZERO;
        let base_fee = builder.evm().block().basefee;
        for tx in bid_txs {
            let tx_effective_gas_price = tx.effective_gas_price(Some(base_fee));
            let recovered_tx = match tx.try_into_recovered() {
                Ok(recovered) => recovered,
                Err(err) => {
                    debug!("Failed to recover transaction signature: {:?}", err);
                    continue;
                }
            };
            let _gas_used = builder.execute_transaction(recovered_tx).map_err(PayloadBuilderError::other).unwrap();
            gas_used += _gas_used;
            gas_fee += (U256::from(tx_effective_gas_price) + U256::from(base_fee)) * U256::from(_gas_used);
        }
        self.gas_used += gas_used;
        self.gas_fee += gas_fee;
    }

    fn pack_reward(&mut self, validator_commission: u64, state_provider: &impl StateProvider) -> Result<(), Box<dyn std::error::Error>> {
        self.packed_block_reward = state_provider.account_balance(&SYSTEM_ADDRESS)?.unwrap_or_default();
        self.packed_validator_reward = self.packed_block_reward * U256::from(validator_commission) / U256::from(10000u64);
        self.packed_validator_reward = self.packed_validator_reward - self.bid.builder_fee;
        Ok(())
    }

    fn valid_reward(&self) -> bool {
        return self.packed_block_reward >= self.expected_block_reward && self.packed_validator_reward >= self.expected_validator_reward;
    }
}

pub trait BidFetcher<Client>
where
    Client: StateProviderFactory + Clone + 'static,
{
    fn get_best_bid(&self, parent_hash: B256) -> Option<BidRuntime<BscEvmConfig>>;
}

impl<Client> BidFetcher<Client> for BidSimulator<Client>
where
    Client: StateProviderFactory + Clone + 'static,
{
    fn get_best_bid(&self, parent_hash: B256) -> Option<BidRuntime<BscEvmConfig>> {
        self.best_bid.get(&parent_hash).cloned()
    }
}