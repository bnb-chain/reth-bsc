use alloy_consensus::{EthereumTxEnvelope, TxEip4844};
use alloy_json_abi::Error;
use alloy_primitives::{Address, B256, U256};
use reth_provider::StateProviderFactory;
use tracing::debug;
use std::collections::HashMap;
use crate::node::evm::config::BscEvmConfig;
use crate::chainspec::BscChainSpec;
use std::sync::Arc;
use reth_payload_primitives::PayloadBuilderError;

#[derive(Clone)]
pub struct Bid {
    pub builder: Address,
    pub block_number: u64,
    pub parent_hash: B256,
    pub txs: Vec<EthereumTxEnvelope<TxEip4844>>,
    pub gas_used: u64,
    pub gas_fee: U256,
    pub builder_fee: U256,
    pub committed: bool,
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
    best_bid_to_run: HashMap<B256, Bid>,
    simulating_bid: HashMap<B256, Bid>,
    best_bid: HashMap<B256, BidRuntime>,
    bid_receiving: bool,
    chain_spec: Arc<BscChainSpec>,
}

impl<Client> BidSimulator<Client> 
where Client: StateProviderFactory,
{
    pub fn new(client: Client, chain_spec: Arc<BscChainSpec>) -> Self {
        Self { 
            client ,
            chain_spec,
            best_bid_to_run: HashMap::new(),
            simulating_bid: HashMap::new(),
            best_bid: HashMap::new(),
            bid_receiving: true,
        }
    }
    pub fn commit_new_bid(&mut self, bid: NewBidPackage) {
        let final_block_number   = match self.client.finalized_block_number() {
            Ok(Some(final_block_number)) => final_block_number,
            Ok(None) => return,
            Err(_) => return,
        };
        
        if bid.bid.block_number <= final_block_number {
            // Bid is for a block that's already finalized, ignore it
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
            self.commit_bid(5,&_bid_runtime)

        }
    }

    fn new_bid_runtime(&self, _bid: &Bid, _validator_commission: u64) -> Result<BidRuntime, Box<dyn std::error::Error + Send + Sync>>{
        let expected_block_reward = _bid.gas_fee;
        let mut expected_validator_reward = expected_block_reward * U256::from(_validator_commission);
        expected_validator_reward = expected_validator_reward / U256::from(10000u64);

        if expected_validator_reward < _bid.builder_fee {
            debug!("BidSimulator: invalid bid, builder fee exceeds validator reward, ignore");
            return Err("invalid bid: builder fee exceeds validator reward".into());
        }
        expected_validator_reward = expected_validator_reward - _bid.builder_fee;
        let evm_config = BscEvmConfig::new(self.chain_spec.clone());

        let runtime = BidRuntime{
            bid: _bid.clone(),
            expected_block_reward: expected_block_reward,
            expected_validator_reward: expected_validator_reward,
            packed_block_reward: U256::from(0),
            packed_validator_reward: U256::from(0),
            finished: false,
            evm_config: evm_config,
        };
        Ok(runtime)
    }

    fn commit_bid(&mut self,reason: u32, bid_runtime: &BidRuntime) {
        // todo: interrupt
        debug!("bid committed reason:{}, bid hash:{}",reason, "");
        self.sim_bid(bid_runtime);
    }

    // sim_bid commit tx and set best bid
    fn sim_bid(&mut self, bid_runtime: &BidRuntime) {
        if !self.bid_receiving {
            return 
        }
        //let startTs = std::time::Instant::now();
        let parent_hash = bid_runtime.bid.parent_hash;
        self.simulating_bid.insert(parent_hash, bid_runtime.bid.clone());
        
        // todo: gas check

        let mut payBidTx = None;
        for (idx,tx) in bid_runtime.bid.txs.clone().iter().enumerate() {
            // todo: interrupt
            if idx==bid_runtime.bid.txs.len()-1 {
                payBidTx = Some(tx.clone());
                break;
            }
            bid_runtime.commit_transaction(tx.clone());
        }

        //todo: greedy merge txs

        if let Some(payBidTx) = payBidTx {
            bid_runtime.commit_transaction(payBidTx);
        }

        let bestBid = self.best_bid.get(&parent_hash);
        if let Some(bestBid) = bestBid {
            if bestBid.packed_block_reward < bid_runtime.packed_block_reward {
                self.best_bid.insert(parent_hash, bid_runtime.clone());
            }
        }else {
            self.best_bid.insert(parent_hash, bid_runtime.clone());
        }

    }
}

#[derive(Clone)]
pub struct BidRuntime<EvmConfig = BscEvmConfig> {
    bid: Bid,
    expected_block_reward: U256,
    expected_validator_reward: U256,
    packed_block_reward: U256,
    packed_validator_reward: U256,

    finished: bool,
    // todo: duration

    // evn
    evm_config: EvmConfig,
}

impl BidRuntime 
{
    fn is_expected_better_than(&self, ohter: &BidRuntime) -> bool {
        if self.expected_block_reward >= ohter.expected_block_reward {
            if self.expected_validator_reward >= ohter.expected_validator_reward {
                return true;
            }
        }
        return false;
    }

    fn commit_transaction(&self,tx:  EthereumTxEnvelope<TxEip4844>) -> Result<(), Error> {
        // todo: check eip4844
        // if tx.is_eip4844() {
           
        // }
        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();

        let mut builder = self.evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                    parent_beacon_block_root: attributes.parent_beacon_block_root(),
                    withdrawals: Some(attributes.withdrawals().clone()),
                },
            )
            .map_err(PayloadBuilderError::other)?;
        Ok(())
    }
}