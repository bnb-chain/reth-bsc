use super::config::evm_env_for_header;
use super::executor::BscBlockExecutor;
use super::factory::BscEvmFactory;
use crate::evm::transaction::BscTxEnv;

use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{eth::receipt_builder::ReceiptBuilder, execute::BlockExecutionError, Database, Evm, EvmFactory, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv};
use reth_ethereum_primitives::TransactionSigned;
use revm::{
    context::{result::ExecutionResult, BlockEnv, TxEnv},
    context_interface::block::Block,
    primitives::{Address, Bytes, TxKind, U256},
};
use alloy_consensus::{TxReceipt, Header, BlockHeader};
use alloy_primitives::{BlockHash, BlockNumber, B256};
use crate::consensus::parlia::{VoteAddress, Snapshot, DIFF_INTURN, DIFF_NOTURN};
use crate::consensus::parlia::util::{is_breathe_block, debug_header};
use crate::consensus::parlia::vote::MAX_ATTESTATION_EXTRA_LENGTH;
use crate::node::evm::error::{BscBlockExecutionError, BscBlockValidationError};
use crate::node::evm::util::HEADER_CACHE_READER;
use crate::system_contracts::SystemContract;
use reth_revm::{database::{EvmStateProvider, StateProviderDatabase}, db::State};
use crate::system_contracts::feynman_fork::ValidatorElectionInfo;
use std::{collections::HashMap, sync::{LazyLock, Mutex}};
use schnellru::{ByLength, LruMap};
use reth_primitives_traits::{GotExpected, SealedHeader};
use blst::{
    min_pk::{PublicKey, Signature},
    BLST_ERROR,
};
use bit_set::BitSet;
use crate::consensus::parlia::constants::K_ANCESTOR_GENERATION_DEPTH;

const BLST_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// A validator set paired with each member's BLS vote address, as `getMiningValidators()` returns
/// them post-Luban (empty vote addresses before it).
pub type EpochValidators = (Vec<Address>, Vec<VoteAddress>);

type ValidatorCache = LruMap<BlockHash, EpochValidators, ByLength>;
type TurnLengthCache = LruMap<BlockHash, u8, ByLength>;

/// `getMiningValidators()` memoized by the block it was evaluated at.
///
/// INVARIANT: the entry under `hash(B)` is the result evaluated in block `B`'s env on `B`'s
/// post-state. Block validation reads this map, so an entry derived any other way forks the node
/// (bnb-chain/reth-bsc#465).
///
/// Writers use two env constructors (`evm_env_for_header`, `next_evm_env`) that agree only on
/// `block.number` — so only a value depending on nothing else may be cached here.
pub static VALIDATOR_CACHE: LazyLock<Mutex<ValidatorCache>> = LazyLock::new(|| {
    Mutex::new(LruMap::new(ByLength::new(1024)))
});

pub static TURN_LENGTH_CACHE: LazyLock<Mutex<TurnLengthCache>> = LazyLock::new(|| {
    Mutex::new(LruMap::new(ByLength::new(1024)))
});

/// Runs a read-only system-contract call in `header`'s env, over a DB already holding `header`'s
/// post-state — go-bsc's `ethAPI.Call(args, BlockNumberOrHashWithHash(hash, false))`. Commits
/// nothing.
fn view_call_at_header<DB, Spec>(
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
    // UFCS on purpose: `BscEvm` also implements `revm::ExecuteEvm::transact`, which skips the
    // system-transaction env overrides that `Evm::transact` applies.
    let result = Evm::transact(&mut evm, tx_env).map_err(BlockExecutionError::other)?.result;
    view_call_output(to, &data, result)
}

/// `getMiningValidators()` on `parent`'s post-state, in `parent`'s [`BlockEnv`].
///
/// The twin of [`BscBlockExecutor::get_current_validators`] with [`CallBlockEnv::Parent`], for
/// callers that hold a state handle rather than an executor.
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
    Ok(if is_luban {
        system_contracts.unpack_data_into_validator_set(&output)
    } else {
        (system_contracts.unpack_data_into_validator_set_before_luban(&output), Vec::new())
    })
}

/// Which block's [`BlockEnv`] a Parlia system-contract read is evaluated in.
///
/// State is the parent's on both paths, so only reads whose *value* depends on the block env need
/// [`Self::Parent`]. Today that is only `getMiningValidators()`: it seeds its 21-of-45 shuffle on
/// `block.number / 200`, so blocks N and N-1 draw from different windows whenever N is a multiple
/// of 200, and go-bsc evaluates it at the parent (bnb-chain/reth-bsc#465).
#[derive(Debug, Clone, Copy)]
pub(crate) enum CallBlockEnv {
    /// The env of the block being executed.
    Current,
    /// The env of its parent. Required for env-dependent reads that validate this block.
    Parent,
}

/// The transaction shape used for read-only system-contract calls.
///
/// `gas_limit` is the gas limit of the block whose env the call runs in, so GASLIMIT reports it.
/// `is_system_transaction` bypasses the EIP-7825 tx gas cap (16M), which BSC block gas limits
/// exceed; it also zeroes BASEFEE and disables nonce checks. BSC's system-contract view functions
/// read none of those, but a future contract reading BASEFEE would need this revisited.
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
    // A call to a codeless address succeeds with empty returndata, which every `unpack_*` below
    // would then `unwrap()`-panic on. Report it as a failed read instead.
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
        BlockEnv = BlockEnv,
    >,
    Spec: EthereumHardforks + crate::hardforks::BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// check the new block, pre check and prepare some intermediate data for finish function.
    /// depends on parlia, header and snapshot.
    pub(crate) fn check_new_block(
        &mut self, 
        block: &BlockEnv
    ) -> Result<(), BlockExecutionError> {
        let block_number = block.number().to::<u64>();
        tracing::trace!("Check new block, block_number: {}", block_number);

        self.inner_ctx.header = self.ctx.header.clone();
        let header = self.inner_ctx.header.clone().unwrap();

        let parent_header = crate::node::evm::util::HEADER_CACHE_READER
            .lock()
            .unwrap()
            .get_header_by_hash(&header.parent_hash)
            .ok_or(BlockExecutionError::msg("Failed to get parent header from global header reader"))?;
        self.inner_ctx.parent_header = Some(parent_header.clone());

        let snap = self
            .snapshot_provider
            .as_ref()
            .unwrap()
            .snapshot_by_hash(&header.parent_hash)
            .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
        self.inner_ctx.snap = Some(snap.clone());
        self.inner_ctx.expected_turn_length = None;

        self.verify_cascading_fields(&header, &parent_header, &snap)?;

        let epoch_length = snap.epoch_num;
        if header.number.is_multiple_of(epoch_length) {
            let (validator_set, vote_addresses) = self.declared_epoch_validators(&header)?;
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
                // Keep parity with go-bsc: turn length is read from parent state.
                let expected_turn_length =
                    self.get_turn_length(parent_header.number, parent_header.timestamp)?;
                self.inner_ctx.expected_turn_length = Some(expected_turn_length);
            }

            // Also fetch on-chain NodeIDs for validators (EVN identification) and update cache.
            // Only available after Maxwell hardfork when StakeHub contract's getNodeIDs is deployed
            if self.spec.is_maxwell_active_at_timestamp(header.number, header.timestamp) {
                let (to2, data2) = self.system_contracts.get_node_ids(self.inner_ctx.current_validators.as_ref().unwrap().0.clone());
                if let Ok(output2) = self.eth_call(to2, data2) {
                    let (_consensus_addrs, node_ids_list) = self.system_contracts.unpack_data_into_node_ids(&output2);
                    tracing::debug!("node_ids_list: {:?}", node_ids_list);
                    let mut flat: Vec<[u8; 32]> = Vec::new();
                    for ids in node_ids_list { for id in ids { flat.push(id); } }
                    crate::node::network::evn_peers::update_onchain_nodeids(flat);
                }
            }
        }
    
        if self.spec.is_feynman_active_at_timestamp(header.number, header.timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(header.number, header.timestamp, parent_header.timestamp) &&
            is_breathe_block(parent_header.timestamp, header.timestamp)
        {
            let (to, data) = self.system_contracts.get_max_elected_validators();
            let bz = self.eth_call(to, data)?;
            let max_elected_validators = self.system_contracts.unpack_data_into_max_elected_validators(bz.as_ref());
            tracing::debug!("max_elected_validators: {:?}", max_elected_validators);
            self.inner_ctx.max_elected_validators = Some(max_elected_validators);

            let (to, data) = self.system_contracts.get_validator_election_info();
            let bz = self.eth_call(to, data)?;

            let (validators, voting_powers, vote_addrs, total_length) =
                self.system_contracts.unpack_data_into_validator_election_info(bz.as_ref());

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

    /// [`VALIDATOR_CACHE`] lookup for block `block_number`, computing it on a miss.
    ///
    /// The validator set epoch block `header` must declare in its extra data.
    ///
    /// go-bsc `verifyValidators` reads it at `(header.ParentHash, header.Number-1)` — the parent's
    /// env, see [`CallBlockEnv`]. Reading through [`VALIDATOR_CACHE`] is safe because every writer
    /// evaluates in the keyed block's own env.
    pub(crate) fn declared_epoch_validators(
        &mut self,
        header: &Header,
    ) -> Result<EpochValidators, BlockExecutionError> {
        self.get_current_validators_with_cache(
            header.number - 1,
            header.parent_hash,
            CallBlockEnv::Parent,
        )
    }

    /// `block_hash` must be the hash of `block_number`. Either `at` value lands on that block's own
    /// env — `Current` when it is the block being executed, `Parent` when it is that block's parent
    /// — so the stored entry honors the cache's invariant either way.
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

    /// Runs a read-only system-contract call in the **parent** block's env.
    ///
    /// go-bsc's `ethAPI.Call(args, BlockNumberOrHashWithHash(header.ParentHash, false))`. The state
    /// is already the parent's, so only the [`BlockEnv`] has to be substituted — and since [`Evm`]
    /// exposes no mutable block accessor, that means a throwaway EVM over the same DB. Nothing is
    /// committed.
    ///
    /// PRECONDITION: only valid before any of this block's state changes are applied, i.e. from
    /// `check_new_block` / `prepare_new_block`.
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

    /// Reads the active validator set from the ValidatorSet system contract.
    ///
    /// `block_number` selects the ABI (pre/post Luban, Euler) and must be the number of the block
    /// whose state is being read. `at` selects the block env — see [`CallBlockEnv`].
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
        Ok(if is_luban {
            self.system_contracts.unpack_data_into_validator_set(&output)
        } else {
            (self.system_contracts.unpack_data_into_validator_set_before_luban(&output), Vec::new())
        })
    }

    fn verify_cascading_fields(
        &self,
        header: &Header,
        parent: &Header,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        self.verify_block_time_for_ramanujan(snap, header, parent)?;
        
        // Verify vote attestation and track errors
        if let Err(err) = self.verify_vote_attestation(snap, header, parent) {
            // Update vote attestation error metric for all attestation-related errors
            self.vote_metrics.vote_attestation_errors_total.increment(1);
            return Err(err);
        }
        
        self.verify_seal(snap, header)?;

        Ok(())
    }

    fn verify_block_time_for_ramanujan(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        self.parlia.block_time_verify_for_ramanujan_fork(snap, header, parent)
    }

    fn verify_vote_attestation(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        if !self.spec.is_plato_active_at_block(header.number()) {
            return Ok(());
        }

        let attestation =
            self.parlia.get_vote_attestation_from_header(header, snap.epoch_num).map_err(|err| {
                tracing::error!("Failed to get vote attestation from header, block_number: {}, error: {:?}", header.number(), err);
                BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
            })?;
        if let Some(attestation) = attestation {
            if attestation.extra.len() > MAX_ATTESTATION_EXTRA_LENGTH {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::TooLargeAttestationExtraLen {
                        extra_len: MAX_ATTESTATION_EXTRA_LENGTH,
                    }
                ).into());
            }
    
            // the attestation target block should be direct parent.
            let target_block = attestation.data.target_number;
            let target_hash = attestation.data.target_hash;
            let mut is_match = false;
            let mut ancestor = parent.clone();
            let depth = if self.spec.is_fermi_active_at_timestamp(header.number(), header.timestamp) {
                K_ANCESTOR_GENERATION_DEPTH
            } else {
                1
            };
            for _ in 0..depth {
                if ancestor.number() == target_block && ancestor.hash_slow() == target_hash {
                    is_match = true;
                    break;
                }
                ancestor = crate::node::evm::util::HEADER_CACHE_READER
                    .lock()
                    .unwrap()
                    .get_header_by_hash(&ancestor.parent_hash())
                    .ok_or_else(|| BscBlockExecutionError::UnknownHeader { block_hash: ancestor.parent_hash() })?;
                tracing::debug!("ancestor: {:?}", ancestor);
            }

            if !is_match {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationTarget {
                        block_number: GotExpected { got: target_block, expected: ancestor.number() },
                        block_hash: GotExpected { got: target_hash, expected: ancestor.hash_slow() }
                            .into(),
                    }
                ).into());
            }
    
            // the attestation source block should be the highest justified block.
            let source_block = attestation.data.source_number;
            let source_hash = attestation.data.source_hash;
            
            let justified = self.get_justified_header(snap)?;
            if source_block != justified.number() || source_hash != justified.hash_slow() {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationSource {
                        block_number: GotExpected { got: source_block, expected: justified.number() },
                        block_hash: GotExpected { got: source_hash, expected: justified.hash_slow() }
                            .into(),
                    }
                ).into());
            }

            let pre_snap = self
                .snapshot_provider
                .as_ref()
                .unwrap()
                .snapshot_by_hash(&ancestor.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get pre snapshot from snapshot provider"))?;

            // query bls keys from snapshot.
            let validators_count = pre_snap.validators.len();
            let vote_bit_set: BitSet<usize> = BitSet::from_iter(
                (0..64).filter(|&i| (attestation.vote_address_set >> i) & 1 != 0)
            );
            let bit_set_count = vote_bit_set.len();
            if bit_set_count > validators_count {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationVoteCount(GotExpected {
                        got: bit_set_count as u64,
                        expected: validators_count as u64,
                    })
                ).into());
            }
             
            let mut vote_addrs: Vec<VoteAddress> = Vec::with_capacity(bit_set_count);
            for (i, val) in pre_snap.validators.iter().enumerate() {
                if !vote_bit_set.contains(i) {
                    continue;
                }

                let val_info = pre_snap
                    .validators_map
                    .get(val)
                    .ok_or(BscBlockExecutionError::VoteAddrNotFoundInSnap { address: *val })?;
                vote_addrs.push(val_info.vote_addr);
            }

            // check if voted validator count satisfied 2/3 + 1
            let at_least_votes = (validators_count * 2).div_ceil(3); // ceil division
            if vote_addrs.len() < at_least_votes {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationVoteCount(GotExpected {
                        got: vote_addrs.len() as u64,
                        expected: at_least_votes as u64,
                    })
                ).into());
            }
 
            // check bls aggregate sig
            let mut pubkeys: Vec<PublicKey> = Vec::with_capacity(vote_addrs.len());
            for addr in &vote_addrs {
                match PublicKey::from_bytes(addr.as_slice()) {
                    Ok(pk) => pubkeys.push(pk),
                    Err(_) => {
                        return Err(
                            BscBlockExecutionError::Validation(
                                BscBlockValidationError::InvalidAttestationSignature
                            ).into()
                        );
                    }
                }
            }
            let vote_addrs_ref: Vec<&PublicKey> = pubkeys.iter().collect();
 
            let sig = Signature::from_bytes(&attestation.agg_signature[..]).map_err(|_| {
                BscBlockExecutionError::Validation(BscBlockValidationError::InvalidAttestationSignature)
            })?;
            
            // Track BLS verification attempt
            self.vote_metrics.bls_verifications_total.increment(1);
            let start = std::time::Instant::now();
            
            let err = sig.fast_aggregate_verify(
                true,
                attestation.data.hash().as_slice(),
                BLST_DST,
                &vote_addrs_ref,
            );
            
            // Record verification duration
            self.vote_metrics.bls_verification_duration_seconds.record(start.elapsed().as_secs_f64());
 
            return match err {
                BLST_ERROR::BLST_SUCCESS => Ok(()),
                _ => {
                    // Update BLS verification failure metric (kept here as it's a specific metric)
                    self.vote_metrics.bls_verification_failures_total.increment(1);
                    Err(BscBlockExecutionError::Validation(
                        BscBlockValidationError::InvalidAttestationSignature
                    ).into())
                },
            };
        }
    
        Ok(())
    }
    
    fn verify_seal(
        &self,
        snap: &Snapshot,
        header: &Header,
    ) -> Result<(), BlockExecutionError> {
        let proposer = self.parlia.recover_proposer(header).map_err(|err| {
            tracing::error!("Failed to recover proposer from header, block_number: {}, error: {:?}", header.number(), err);
            BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
        })?;

        if proposer != header.beneficiary {
            tracing::error!("Wrong header signer, block_number: {}, proposer: {:?}, expected: {:?}", 
                header.number(), proposer, header.beneficiary);
            debug_header(header, self.spec.chain().id(), "verify_seal_header");
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::WrongHeaderSigner {
                    block_number: header.number(),
                    signer: GotExpected { got: proposer, expected: header.beneficiary }.into(),
                }
            ).into());
        }

        if !snap.validators.contains(&proposer) {
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::SignerUnauthorized { 
                    block_number: header.number(), 
                    proposer 
                }
            ).into());
        }

        if snap.sign_recently(proposer) {
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::SignerOverLimit { proposer }
            ).into());
        }

        let is_inturn = snap.is_inturn(proposer);
        if (is_inturn && header.difficulty != DIFF_INTURN) ||
            (!is_inturn && header.difficulty != DIFF_NOTURN)
        {
            let expected_difficulty = if is_inturn { DIFF_INTURN } else { DIFF_NOTURN };
            tracing::warn!(
                target: "bsc::validation",
                block_number = header.number(),
                block_hash = ?header.hash_slow(),
                proposer = ?proposer,
                is_inturn,
                actual_difficulty = %header.difficulty,
                expected_difficulty = %expected_difficulty,
                diff_inturn = %DIFF_INTURN,
                diff_noturn = %DIFF_NOTURN,
                "Block difficulty validation failed: mismatch between inturn status and difficulty"
            );
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::InvalidDifficulty { difficulty: header.difficulty }
            ).into());
        }

        Ok(())
    }

    pub(crate) fn get_justified_header(
        &self,
        snap: &Snapshot,
    ) -> Result<Header, BlockExecutionError> {
        if snap.vote_data.source_hash == B256::ZERO && snap.vote_data.target_hash == B256::ZERO {
            return HEADER_CACHE_READER
                .lock()
                .unwrap()
                .get_header_by_number(0)
                .ok_or_else(|| {
                    BscBlockExecutionError::UnknownHeader { block_hash: B256::ZERO }.into()
                });
        }

        HEADER_CACHE_READER
            .lock()
            .unwrap()
            .get_header_by_hash(&snap.vote_data.target_hash)
            .ok_or_else(|| {
                BscBlockExecutionError::UnknownHeader { block_hash: snap.vote_data.target_hash }.into()
            })
    }

    /// prepare some intermediate data for produce new block.
    pub(crate) fn prepare_new_block(
        &mut self, 
        block: &BlockEnv
    ) -> Result<(), BlockExecutionError> {
        let parent_header = crate::node::evm::util::HEADER_CACHE_READER
            .lock()
            .unwrap()
            .get_header_by_hash(&self.ctx.base.parent_hash)
            .ok_or(BlockExecutionError::msg("Failed to get parent header from global header reader"))?;
        self.inner_ctx.parent_header = Some(parent_header.clone());
        let snap = self
            .snapshot_provider
            .as_ref()
            .unwrap()
            .snapshot_by_hash(&self.ctx.base.parent_hash)
            .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
        self.inner_ctx.snap = Some(snap.clone());

        let header_number = block.number().to::<u64>();
        let header_timestamp = block.timestamp().to::<u64>();
        if self.spec.is_feynman_active_at_timestamp(header_number, header_timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(header_number, header_timestamp, parent_header.timestamp) &&
            is_breathe_block(parent_header.timestamp, header_timestamp)
        {
            let (to, data) = self.system_contracts.get_max_elected_validators();
            let bz = self.eth_call(to, data)?;
            let max_elected_validators = self.system_contracts.unpack_data_into_max_elected_validators(bz.as_ref());
            tracing::debug!("max_elected_validators: {:?}", max_elected_validators);
            self.inner_ctx.max_elected_validators = Some(max_elected_validators);

            let (to, data) = self.system_contracts.get_validator_election_info();
            let bz = self.eth_call(to, data)?;

            let (validators, voting_powers, vote_addrs, total_length) =
                self.system_contracts.unpack_data_into_validator_election_info(bz.as_ref());

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
