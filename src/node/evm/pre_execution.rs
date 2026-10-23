use super::config::{evm_env_for_header, BscExecutionMode};
use super::executor::BscBlockExecutor;
use super::factory::BscEvmFactory;
use crate::evm::transaction::BscTxEnv;

use crate::{
    consensus::{
        parlia::{util::is_breathe_block, VoteAddress},
        payment_lane::{state::LaneState, LaneError, LaneParentState, GETTER_GAS_LIMIT},
    },
    node::evm::error::lane_reject,
    system_contracts::{feynman_fork::ValidatorElectionInfo, SystemContract},
};
use alloy_consensus::{BlockHeader, Header, TxReceipt};
use alloy_primitives::{BlockHash, BlockNumber};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    eth::receipt_builder::ReceiptBuilder, execute::BlockExecutionError, Database, Evm, EvmFactory,
    FromRecoveredTx, FromTxWithEncoded, IntoTxEnv,
};
use reth_primitives_traits::SealedHeader;
use reth_revm::{
    database::{EvmStateProvider, StateProviderDatabase},
    db::State,
};
use revm::{
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    context_interface::block::Block,
    primitives::{Address, Bytes, TxKind, U256},
};
use schnellru::{ByLength, LruMap};
use std::{
    collections::HashMap,
    sync::{LazyLock, Mutex},
};

pub type EpochValidators = (Vec<Address>, Vec<VoteAddress>);

/// An address's role in the validator set used for BEP-675 bad-block evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidatorRole {
    /// The address is not in the validator set.
    None,
    /// The address is in the cabinet prefix of the validator set.
    Cabinet,
    /// The address is a candidate validator outside the cabinet prefix.
    Candidate,
}

impl std::fmt::Display for ValidatorRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::None => "none",
            Self::Cabinet => "cabinet",
            Self::Candidate => "candidate",
        })
    }
}

/// Fallback used by `BSCValidatorSet` while `numOfCabinets` is unset.
const INIT_NUM_OF_CABINETS: usize = 21;

fn classify_working_validator(
    index: usize,
    validator_count: usize,
    cabinet_count: usize,
) -> ValidatorRole {
    let guaranteed_cabinets = cabinet_count.min(validator_count);
    if index < guaranteed_cabinets {
        ValidatorRole::Cabinet
    } else {
        ValidatorRole::Candidate
    }
}

type ValidatorCache = LruMap<BlockHash, EpochValidators, ByLength>;
type TurnLengthCache = LruMap<BlockHash, u8, ByLength>;

pub static VALIDATOR_CACHE: LazyLock<Mutex<ValidatorCache>> = LazyLock::new(|| {
    Mutex::new(LruMap::new(ByLength::new(1024)))
});

pub static TURN_LENGTH_CACHE: LazyLock<Mutex<TurnLengthCache>> = LazyLock::new(|| {
    Mutex::new(LruMap::new(ByLength::new(1024)))
});

/// Runs a read-only system-contract call in `header`'s env over `header`'s post-state.
pub(crate) fn view_call_at_header<DB, Spec>(
    db: DB,
    spec: &Spec,
    header: &Header,
    to: Address,
    data: Bytes,
) -> Result<Bytes, BlockExecutionError>
where
    DB: Database,
    Spec: EthChainSpec + crate::hardforks::BscHardforks + Clone,
{
    let tx_env = view_call_tx_env(to, data.clone(), header.gas_limit, spec.chain().id());
    let mut evm = BscEvmFactory::default().create_evm(db, evm_env_for_header(spec, header));
    // Use `Evm::transact` so system-transaction overrides still apply.
    let result = Evm::transact(&mut evm, tx_env).map_err(BlockExecutionError::other)?.result;
    view_call_output(to, &data, result)
}

/// Classify `address` against the full cabinet-first validator set at `parent`, returning
/// the cabinet and total counts for this event's majority thresholds.
///
/// The counts are returned even when `address` is absent ([`ValidatorRole::None`]) so callers
/// can fall back to another membership source without losing the thresholds in force.
pub(crate) fn validator_role_at_parent<S, Spec>(
    state: S,
    spec: Spec,
    parent: &SealedHeader,
    address: Address,
) -> Result<(ValidatorRole, usize, usize), BlockExecutionError>
where
    S: EvmStateProvider,
    Spec: EthChainSpec + crate::hardforks::BscHardforks + Clone,
{
    let mut db = State::builder().with_database(StateProviderDatabase::new(state)).build();
    let system_contracts = SystemContract::new(spec.clone());

    let (to, data) = system_contracts.get_all_validators();
    let output = view_call_at_header(&mut db, &spec, parent.header(), to, data)?;
    let validators = system_contracts
        .unpack_all_validators(&output)
        .map_err(BlockExecutionError::msg)?;
    let (to, data) = system_contracts.get_num_of_cabinets();
    let output = view_call_at_header(&mut db, &spec, parent.header(), to, data)?;
    let configured_cabinets =
        system_contracts.unpack_num_of_cabinets(&output).map_err(BlockExecutionError::msg)?;
    let configured_cabinets =
        if configured_cabinets.is_zero() || configured_cabinets > U256::from(i64::MAX as u64) {
            INIT_NUM_OF_CABINETS
        } else {
            usize::try_from(configured_cabinets).unwrap_or(INIT_NUM_OF_CABINETS)
        };

    let cabinets = configured_cabinets.min(validators.len());
    let role = match validators.iter().position(|validator| *validator == address) {
        Some(index) => classify_working_validator(index, validators.len(), cabinets),
        None => ValidatorRole::None,
    };
    Ok((role, cabinets, validators.len()))
}

#[cfg(test)]
mod bid_block_evidence_tests {
    use super::*;

    #[test]
    fn full_set_uses_cabinet_prefix() {
        assert_eq!(classify_working_validator(20, 45, 21), ValidatorRole::Cabinet);
        assert_eq!(classify_working_validator(21, 45, 21), ValidatorRole::Candidate);
        assert_eq!(classify_working_validator(44, 45, 21), ValidatorRole::Candidate);
    }

    #[test]
    fn cabinet_prefix_is_capped_by_set_size() {
        assert_eq!(classify_working_validator(4, 5, 21), ValidatorRole::Cabinet);
    }
}

/// `getMiningValidators()` on `parent`'s post-state in `parent`'s env.
pub(crate) fn validators_at_parent<S, Spec>(
    state: S,
    spec: Spec,
    parent: &SealedHeader,
) -> Result<EpochValidators, BlockExecutionError>
where
    S: EvmStateProvider,
    Spec: EthChainSpec + crate::hardforks::BscHardforks + Clone,
{
    let mut db = State::builder().with_database(StateProviderDatabase::new(state)).build();
    let system_contracts = SystemContract::new(spec.clone());
    let is_luban = spec.is_luban_active_at_block(parent.number());
    let (to, data) = if is_luban {
        system_contracts.get_current_validators()
    } else {
        system_contracts.get_current_validators_before_luban(parent.number())
    };
    let output = view_call_at_header(&mut db, &spec, parent.header(), to, data)?;
    if is_luban {
        system_contracts
            .unpack_data_into_validator_set(&output)
            .ok_or_else(|| BlockExecutionError::msg("Failed to decode system contract output"))
    } else {
        let validators = system_contracts
            .unpack_data_into_validator_set_before_luban(&output)
            .ok_or_else(|| BlockExecutionError::msg("Failed to decode system contract output"))?;
        Ok((validators, Vec::new()))
    }
}

/// Which block env a Parlia system-contract read uses.
///
/// Only `getMiningValidators()` needs `Parent`: the state is already the parent's on both paths.
#[derive(Debug, Clone, Copy)]
pub(crate) enum CallBlockEnv {
    /// The env of the block being executed.
    Current,
    /// The env of its parent. Required for env-dependent reads that validate this block.
    Parent,
}

/// The transaction shape used for read-only system-contract calls.
fn view_call_tx_env(to: Address, data: Bytes, gas_limit: u64, chain_id: u64) -> BscTxEnv {
    BscTxEnv {
        base: TxEnv {
            caller: Address::default(),
            kind: TxKind::Call(to),
            nonce: 0,
            gas_limit,
            value: U256::ZERO,
            data,
            gas_price: 0,
            chain_id: Some(chain_id),
            gas_priority_fee: None,
            access_list: Default::default(),
            blob_hashes: Vec::new(),
            max_fee_per_blob_gas: 0,
            tx_type: 0,
            authorization_list: Default::default(),
        },
        is_system_transaction: true,
    }
}

/// Extracts the return data of a read-only system-contract call.
fn view_call_output<H>(
    to: Address,
    data: &Bytes,
    result: ExecutionResult<H>,
) -> Result<Bytes, BlockExecutionError> {
    if !result.is_success() {
        tracing::error!("Failed to eth call, to: {:?}, data: {:?}", to, data);
        return Err(BlockExecutionError::msg("ETH call failed"));
    }
    let output = result
        .into_output()
        .ok_or_else(|| BlockExecutionError::msg("ETH call output is None"))?;
    // Treat empty returndata as an invalid read instead of letting ABI unpack panic.
    if output.is_empty() {
        tracing::error!("Empty eth call output, to: {:?}, data: {:?}", to, data);
        return Err(BlockExecutionError::msg("ETH call returned no data"));
    }
    Ok(output)
}

impl<'a, EVM, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, EVM, Spec, R>
where
    EVM: Evm<
        DB: alloy_evm::block::StateDB,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
        BlockEnv = crate::evm::block_env::BscBlockEnv,
    >,
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// Validate block fields that depend on Parlia, the header, and the parent snapshot.
    pub(crate) fn check_new_block(&mut self, block: &BlockEnv) -> Result<(), BlockExecutionError> {
        let block_number = block.number().to::<u64>();
        tracing::trace!("Check new block, block_number: {}", block_number);

        self.inner_ctx.header = self.ctx.header.clone();
        let header = self
            .inner_ctx
            .header
            .clone()
            .ok_or_else(|| BlockExecutionError::msg("Missing header in execution context"))?;

        let parent_header =
            crate::node::evm::util::get_header_by_hash_from_cache(&header.parent_hash).ok_or(
                BlockExecutionError::msg("Failed to get parent header from global header reader"),
            )?;
        self.inner_ctx.parent_header = Some(parent_header.clone());

        let snap = self
            .snapshot_provider
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Snapshot provider is not available"))?
            .snapshot_by_hash(&header.parent_hash)
            .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
        self.inner_ctx.snap = Some(snap.clone());
        self.inner_ctx.expected_turn_length = None;

        crate::consensus::parlia::header_verifier::HeaderVerifier::new(
            &self.parlia,
            self.snapshot_provider.as_deref(),
            &self.vote_metrics,
        )
        .verify(&header, &parent_header, &snap)?;

        // Initialize before execution; validate total gas in `post_check_new_block`.
        self.init_payment_lane(&parent_header, header.gas_limit)?;

        let epoch_length = snap.epoch_num;
        if header.number.is_multiple_of(epoch_length) {
            let (validator_set, vote_addresses) = self.get_current_validators_with_cache(
                header.number - 1,
                header.parent_hash,
                CallBlockEnv::Parent,
            )?;
            tracing::debug!("validator_set: {:?}, vote_addresses: {:?}", validator_set, vote_addresses);
            
            let vote_addrs_map = if vote_addresses.is_empty() {
                HashMap::new()
            } else {
                validator_set
                    .iter()
                    .copied()
                    .zip(vote_addresses)
                    .collect::<std::collections::HashMap<_, _>>()
            };
            tracing::debug!("vote_addrs_map: {:?}", vote_addrs_map);
            self.inner_ctx.current_validators = Some((validator_set, vote_addrs_map));

            if self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp) {
                // Turn length is read from the parent state.
                let expected_turn_length =
                    self.get_turn_length(parent_header.number, parent_header.timestamp)?;
                self.inner_ctx.expected_turn_length = Some(expected_turn_length);
            }

            // Also fetch validator NodeIDs after Maxwell.
            if self.spec.is_maxwell_active_at_timestamp(header.number, header.timestamp) {
                let current_validators = self
                    .inner_ctx
                    .current_validators
                    .as_ref()
                    .ok_or_else(|| BlockExecutionError::msg("Invalid current validators data"))?
                    .0
                    .clone();
                let (to2, data2) = self.system_contracts.get_node_ids(current_validators);
                if let Ok(output2) = self.eth_call(to2, data2) {
                    match self.system_contracts.unpack_data_into_node_ids(&output2) {
                        Some((_consensus_addrs, node_ids_list)) => {
                            tracing::debug!("node_ids_list: {:?}", node_ids_list);
                            let mut flat: Vec<[u8; 32]> = Vec::new();
                            for ids in node_ids_list { for id in ids { flat.push(id); } }
                            crate::node::network::evn_peers::update_onchain_nodeids(flat);
                        }
                        // Advisory data for EVN peering only, so a decode failure must not
                        // fail the block.
                        None => tracing::warn!("Failed to decode getNodeIDs output"),
                    }
                }
            }
        }
    
        if self.spec.is_feynman_active_at_timestamp(header.number, header.timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(header.number, header.timestamp, parent_header.timestamp) &&
            is_breathe_block(parent_header.timestamp, header.timestamp)
        {
            let (to, data) = self.system_contracts.get_max_elected_validators();
            let bz = self.eth_call(to, data)?;
            let max_elected_validators = self
                .system_contracts
                .unpack_data_into_max_elected_validators(bz.as_ref())
                .ok_or_else(|| {
                    BlockExecutionError::msg("Failed to decode system contract output")
                })?;
            tracing::debug!("max_elected_validators: {:?}", max_elected_validators);
            self.inner_ctx.max_elected_validators = Some(max_elected_validators);

            let (to, data) = self.system_contracts.get_validator_election_info();
            let bz = self.eth_call(to, data)?;

            let (validators, voting_powers, vote_addrs, total_length) = self
                .system_contracts
                .unpack_data_into_validator_election_info(bz.as_ref())
                .ok_or_else(|| {
                    BlockExecutionError::msg("Failed to decode system contract output")
                })?;

            let total_length = total_length.to::<u64>() as usize;
            if validators.len() != total_length ||
                voting_powers.len() != total_length ||
                vote_addrs.len() != total_length
            {
                return Err(BlockExecutionError::msg("Failed to get top validators"));
            }

            let validator_election_info: Vec<ValidatorElectionInfo> = validators
                .into_iter()
                .zip(voting_powers)
                .zip(vote_addrs)
                .map(|((validator, voting_power), vote_addr)| ValidatorElectionInfo {
                    address: validator,
                    voting_power,
                    vote_address: vote_addr,
                })
                .collect();
            tracing::debug!("validator_election_info: {:?}", validator_election_info);
            self.inner_ctx.validators_election_info = Some(validator_election_info);
        }

        Ok(())
    }

    pub(crate) fn get_current_validators_with_cache(
        &mut self, 
        block_number: BlockNumber,
        block_hash: BlockHash,
        at: CallBlockEnv,
    ) -> Result<EpochValidators, BlockExecutionError> {
        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            if let Some(cached_result) = cache.get(&block_hash) {
                tracing::debug!("Succeed to query cached validator result, block_number: {}, block_hash: {}, evm_block_number: {}", 
                block_number, block_hash, self.evm.block().number());
                return Ok(cached_result.clone());
            }
        }

        let result = self.get_current_validators(block_number, at)?;

        {
            let mut cache = VALIDATOR_CACHE.lock().unwrap();
            cache.insert(block_hash, result.clone());
            tracing::debug!("Succeed to update cache, block_number: {}, block_hash: {}, evm_block_number: {}", 
                block_number, block_hash, self.evm.block().number());
        }

        Ok(result)
    }


    /// Runs a read-only system-contract call in the env of the block being executed.
    pub(crate) fn eth_call(
        &mut self,
        to: Address,
        data: Bytes
    ) -> Result<Bytes, BlockExecutionError> {
        let tx_env =
            view_call_tx_env(to, data.clone(), self.evm.block().gas_limit(), self.spec.chain().id());
        let result_and_state = self.evm.transact(tx_env.into_tx_env()).map_err(BlockExecutionError::other)?;
        view_call_output(to, &data, result_and_state.result)
    }

    /// Initialize before state changes. Jenner installs `0x2007` on activation, so the lane
    /// starts in its child; gate on the parent and use this block's full gas limit.
    fn init_payment_lane(
        &mut self,
        parent: &Header,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        if !self.spec.is_jenner_active_at_timestamp(parent.number, parent.timestamp) {
            return Ok(());
        }
        let (block, parent_hash) = (parent.number + 1, self.ctx.base.parent_hash);
        let producing =
            matches!(self.ctx.mode, BscExecutionMode::Mining | BscExecutionMode::BidSimulation);
        self.lane = LaneState::resolve(self, parent_hash, gas_limit)
            .inspect_err(|err| {
                if producing {
                    crate::metrics::LANE_METRICS.produce_declined.increment(1);
                }
                tracing::error!(
                    target: "bsc::payment_lane",
                    block,
                    parent = %parent_hash,
                    gas_limit,
                    error = %err,
                    "cannot derive the payment lane"
                );
            })
            .map_err(lane_reject)?;
        tracing::debug!(
            target: "bsc::payment_lane",
            block,
            ratio = self.lane.ratio(),
            quota = self.lane.quota(),
            listed = self.lane.listed_len(),
            "payment lane active"
        );
        Ok(())
    }

    /// Runs the same read-only system call against the current DB, but under `parent`'s env.
    ///
    /// PRECONDITION: only valid before this block mutates state.
    fn eth_call_at_parent(
        &mut self,
        to: Address,
        data: Bytes,
    ) -> Result<Bytes, BlockExecutionError> {
        let parent = self.inner_ctx.parent_header.clone().ok_or_else(|| {
            BlockExecutionError::msg("Missing parent header for parent-env eth call")
        })?;
        debug_assert_eq!(
            parent.number + 1,
            self.evm.block().number().to::<u64>(),
            "parent-env call must run while executing the parent's direct child"
        );

        view_call_at_header(self.evm.db_mut(), &self.spec, &parent, to, data)
    }

    /// Reads the active validator set.
    ///
    /// `block_number` selects the ABI; `at` selects the block env.
    pub(crate) fn get_current_validators(
        &mut self,
        block_number: BlockNumber,
        at: CallBlockEnv,
    ) -> Result<EpochValidators, BlockExecutionError> {
        let is_luban = self.spec.is_luban_active_at_block(block_number);
        let (to, data) = if is_luban {
            self.system_contracts.get_current_validators()
        } else {
            self.system_contracts.get_current_validators_before_luban(block_number)
        };
        let output = match at {
            CallBlockEnv::Current => self.eth_call(to, data)?,
            CallBlockEnv::Parent => self.eth_call_at_parent(to, data)?,
        };
        if is_luban {
            self.system_contracts
                .unpack_data_into_validator_set(&output)
                .ok_or_else(|| BlockExecutionError::msg("Failed to decode system contract output"))
        } else {
            let validators = self
                .system_contracts
                .unpack_data_into_validator_set_before_luban(&output)
                .ok_or_else(|| {
                    BlockExecutionError::msg("Failed to decode system contract output")
                })?;
            Ok((validators, Vec::new()))
        }
    }

    /// prepare some intermediate data for produce new block.
    pub(crate) fn prepare_new_block(
        &mut self, 
        block: &BlockEnv
    ) -> Result<(), BlockExecutionError> {
        let parent_header =
            crate::node::evm::util::get_header_by_hash_from_cache(&self.ctx.base.parent_hash)
                .ok_or(BlockExecutionError::msg(
                    "Failed to get parent header from global header reader",
                ))?;
        self.inner_ctx.parent_header = Some(parent_header.clone());

        // Only the Parlia finalization in `finish` reads the snapshot, and simulation skips
        // that entirely. Requiring one here would make `eth_simulateV1` fail — or panic on
        // the `unwrap` below — whenever the snapshot provider is unavailable, e.g. during
        // early startup before consensus has published it.
        if self.ctx.mode != BscExecutionMode::Simulation {
            let snap = self
                .snapshot_provider
                .as_ref()
                .ok_or(BlockExecutionError::msg("Snapshot provider is not available"))?
                .snapshot_by_hash(&self.ctx.base.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
            self.inner_ctx.snap = Some(snap.clone());
        }

        // Use the full block limit, before subtracting producer reserves.
        if self.ctx.mode != BscExecutionMode::Simulation {
            self.init_payment_lane(&parent_header, block.gas_limit())?;
        }

        let header_number = block.number().to::<u64>();
        let header_timestamp = block.timestamp().to::<u64>();
        // The election data below feeds `update_validator_set_v2`, which only runs during
        // finalization.
        if self.ctx.mode != BscExecutionMode::Simulation &&
            self.spec.is_feynman_active_at_timestamp(header_number, header_timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(header_number, header_timestamp, parent_header.timestamp) &&
            is_breathe_block(parent_header.timestamp, header_timestamp)
        {
            let (to, data) = self.system_contracts.get_max_elected_validators();
            let bz = self.eth_call(to, data)?;
            let max_elected_validators = self
                .system_contracts
                .unpack_data_into_max_elected_validators(bz.as_ref())
                .ok_or_else(|| {
                    BlockExecutionError::msg("Failed to decode system contract output")
                })?;
            tracing::debug!("max_elected_validators: {:?}", max_elected_validators);
            self.inner_ctx.max_elected_validators = Some(max_elected_validators);

            let (to, data) = self.system_contracts.get_validator_election_info();
            let bz = self.eth_call(to, data)?;

            let (validators, voting_powers, vote_addrs, total_length) = self
                .system_contracts
                .unpack_data_into_validator_election_info(bz.as_ref())
                .ok_or_else(|| {
                    BlockExecutionError::msg("Failed to decode system contract output")
                })?;

            let total_length = total_length.to::<u64>() as usize;
            if validators.len() != total_length ||
                voting_powers.len() != total_length ||
                vote_addrs.len() != total_length
            {
                return Err(BlockExecutionError::msg("Failed to get top validators"));
            }

            let validator_election_info: Vec<ValidatorElectionInfo> = validators
                .into_iter()
                .zip(voting_powers)
                .zip(vote_addrs)
                .map(|((validator, voting_power), vote_addr)| ValidatorElectionInfo {
                    address: validator,
                    voting_power,
                    vote_address: vote_addr,
                })
                .collect();
            tracing::debug!("validator_election_info: {:?}", validator_election_info);
            self.inner_ctx.validators_election_info = Some(validator_election_info);
        }
        Ok(())
    }
}

/// Reject late reads to avoid caching this block's mutations under its parent's hash.
impl<'a, EVM, Spec, R: ReceiptBuilder> LaneParentState for BscBlockExecutor<'a, EVM, Spec, R>
where
    EVM: Evm<
        DB: alloy_evm::block::StateDB,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
        BlockEnv = crate::evm::block_env::BscBlockEnv,
    >,
    Spec: EthereumHardforks + crate::hardforks::BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    fn call_lane_getter(&mut self, to: Address, data: Bytes) -> Result<Bytes, LaneError> {
        if !self.db_at_parent_state {
            return Err(LaneError::StateUnavailable(
                "payment lane read after this block mutated state".into(),
            ));
        }

        let tx_env = view_call_tx_env(to, data, GETTER_GAS_LIMIT, self.spec.chain().id());
        // Execution errors abort validation locally; getter reverts/halts indicate bad config.
        let result = self
            .evm
            .transact(tx_env.into_tx_env())
            .map_err(|err| LaneError::StateUnavailable(err.to_string()))?
            .result;

        // Report the reason without retaining potentially large return data.
        let reason = match result {
            ExecutionResult::Success { output, .. } => {
                let data = output.into_data();
                if !data.is_empty() {
                    return Ok(data);
                }
                "returned no data".to_string()
            }
            ExecutionResult::Revert { .. } => "reverted".to_string(),
            ExecutionResult::Halt { reason, .. } => format!("halted: {reason:?}"),
        };
        Err(LaneError::CorruptConfig(format!("getter at {to} {reason}")))
    }
}
