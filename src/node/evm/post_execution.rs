use super::{
    config::{revm_spec_by_timestamp_and_block_number, BscExecutionMode},
    error::{BscBlockExecutionError, BscBlockValidationError},
    executor::BscBlockExecutor,
    util::{set_nonce, NativeBlockEnv},
};
use crate::{
    consensus::{
        parlia::{
            constants::COLLECT_ADDITIONAL_VOTES_REWARD_RATIO, snapshot::DEFAULT_TURN_LENGTH,
            util::is_breathe_block, VoteAddress, VoteAttestation, DIFF_INTURN,
            FF_REWARD_DISTRIBUTION_INTERVAL,
        },
        MAX_SYSTEM_REWARD, SYSTEM_ADDRESS, SYSTEM_REWARD_PERCENT,
    },
    evm::precompiles,
    node::{
        evm::pre_execution::{CallBlockEnv, TURN_LENGTH_CACHE},
        miner::signer::{is_signer_initialized, sign_system_transaction},
    },
    system_contracts::{
        feynman_fork::{
            get_top_validators_by_voting_power, ElectedValidators, ValidatorElectionInfo,
        },
        SLASH_CONTRACT, STAKE_HUB_CONTRACT, SYSTEM_REWARD_CONTRACT,
    },
};
use alloy_consensus::{
    transaction::Recovered, Header, SignableTransaction, Transaction as AlloyTransaction, TxLegacy,
    TxReceipt,
};
use alloy_primitives::{hex, Address, BlockNumber, TxKind, U256};
use bit_set::BitSet;
use evm2::{ethereum::TxEnvelope, evm::PendingState, EvmFeatures, TxResultWithState};
use reth_bsc_evm::types::BscEvmTypes;
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::{Transaction, TransactionSigned};
use reth_evm::{BlockExecutionError, Evm, ReceiptBuilder, ReceiptBuilderCtx};
type BlockEnv = evm2::env::BlockEnv<BscEvmTypes>;
use reth_primitives_traits::{GotExpected, SignerRecoverable};

use std::collections::HashMap;
use tracing::warn;

#[inline]
fn turn_length_matches(
    turn_length_from_header: Option<u8>,
    expected_turn_length: Option<u8>,
) -> bool {
    matches!(
        (turn_length_from_header, expected_turn_length),
        (Some(header), Some(expected)) if header == expected
    )
}

impl<'a, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, Spec, R>
where
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
{
    /// post check the new block, post check some parlia field and the system txs.
    /// depends on parlia, header and snapshot.
    pub(crate) fn post_check_new_block(
        &mut self,
        block: &BlockEnv,
    ) -> Result<(), BlockExecutionError> {
        tracing::debug!(
            "Start to post check new block, block_number: {}, mode: {:?}",
            block.number(),
            self.ctx.mode
        );
        self.verify_validators(
            self.inner_ctx.current_validators.clone(),
            self.inner_ctx.header.clone(),
        )?;
        self.verify_turn_length(self.inner_ctx.header.clone())?;

        // check the system txs.
        let header_difficulty = self
            .inner_ctx
            .header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing header in execution context"))?
            .difficulty;
        if header_difficulty != DIFF_INTURN {
            tracing::debug!("Start to slash spoiled validator, block_number: {}, block_difficulty: {:?}, diff_inturn: {:?}", 
                block.number(), header_difficulty, DIFF_INTURN);
            let snap =
                self.inner_ctx.snap.as_ref().ok_or_else(|| {
                    BlockExecutionError::msg("Missing snapshot in execution context")
                })?;
            let spoiled_validator = snap.inturn_validator();
            let signed_recently = if self.spec.is_plato_active_at_block(block.number().to()) {
                snap.sign_recently(spoiled_validator)
            } else {
                snap.recent_proposers.iter().any(|(_, v)| *v == spoiled_validator)
            };
            if !signed_recently {
                self.slash_spoiled_validator(block.beneficiary(), spoiled_validator)?;
                let block_hash = self.inner_ctx.header.as_ref().map(|h| h.hash_slow());
                tracing::info!(
                    target: "bsc::evm",
                    block_number = %block.number(),
                    block_hash = ?block_hash,
                    spoiled_validator = ?spoiled_validator,
                    backoff_validator = ?block.beneficiary(),
                    "Slash spoiled validator"
                );
            }
        }

        self.distribute_incoming(block.beneficiary())?;

        if self.spec.is_plato_active_at_block(block.number().to()) {
            self.distribute_finality_reward()?;
        }

        // update validator set after Feynman upgrade
        let header_number = self.evm.block().number().to::<u64>();
        let header_timestamp = self.evm.block().timestamp().to::<u64>();
        let header_beneficiary = self.evm.block().beneficiary();
        let parent_header = self
            .inner_ctx
            .parent_header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing parent header in execution context"))?
            .clone();
        if self.spec.is_feynman_active_at_timestamp(header_number, header_timestamp) &&
            is_breathe_block(parent_header.timestamp, header_timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(
                header_number,
                header_timestamp,
                parent_header.timestamp,
            )
        {
            let max_elected_validators =
                self.inner_ctx.max_elected_validators.unwrap_or(U256::from(21));
            let validators_election_info =
                self.inner_ctx.validators_election_info.clone().unwrap_or_default();

            self.update_validator_set_v2(
                max_elected_validators,
                validators_election_info.clone(),
                header_beneficiary,
            )?;
            tracing::debug!("Update validator set, block_number: {}, max_elected_validators: {}, validators_election_info: {:?}", 
                header_number, max_elected_validators, validators_election_info);
        }

        if !self.system_txs.is_empty() {
            tracing::error!(
                "Remaining system txs after block execution, block_number: {}, len: {}",
                block.number(),
                self.system_txs.len()
            );
            for tx in self.system_txs.iter() {
                tracing::error!("remaining system tx: {:?}", tx);
            }
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::UnexpectedSystemTx,
            )
            .into());
        }

        let header = self
            .inner_ctx
            .header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing header in execution context"))?
            .clone();

        // Notes: here we get the current block's snapshot (after applying this block's header) to
        // prepare cache. This is important because epoch_num may change during block
        // application
        let current_snap = self
            .snapshot_provider
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Snapshot provider is not available"))?
            .snapshot_by_hash(&header.hash_slow())
            .ok_or(BlockExecutionError::msg(
                "Failed to get current snapshot from snapshot provider",
            ))?;

        // Use epoch_num from current snapshot (after apply) for epoch boundary check
        let epoch_length = current_snap.epoch_num;
        let is_next_epoch = (header.number + 1).is_multiple_of(epoch_length);
        if is_next_epoch {
            // cache validators
            // cache it on pre block.
            // for verify validators in post-check of fullnode mode and prepare new header in miner
            // mode.
            self.get_current_validators_with_cache(
                header.number,
                header.hash_slow(),
                CallBlockEnv::Current, // unchanged: this block's own env
            )?;
        }

        {
            // cache turnlength
            let is_bohr = self.spec.is_bohr_active_at_timestamp(header.number, header.timestamp);
            tracing::debug!(
                "Check turn length cache update: block_number={}, epoch_length={}, is_next_epoch={}, is_bohr={}",
                header.number, epoch_length, is_next_epoch, is_bohr
            );

            if is_next_epoch && is_bohr {
                let turn_length = self.get_turn_length(header.number, header.timestamp)?;
                let mut cache = TURN_LENGTH_CACHE.lock().unwrap();
                cache.insert(header.hash_slow(), turn_length);
                tracing::debug!("Succeed to update turn length cache, block_number: {}, block_hash: {}, epoch_length: {}, turn_length: {}", 
                    header.number, header.hash_slow(), epoch_length, turn_length);
            }
        }
        self.verify_payment_lane(self.gas_used)?;

        tracing::trace!("Succeed to finalize new block, block_number: {}", block.number());
        Ok(())
    }

    fn verify_validators(
        &mut self,
        current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
        header: Option<Header>,
    ) -> Result<(), BlockExecutionError> {
        let header_ref = header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing header in execution context"))?;
        let epoch_length = self
            .inner_ctx
            .snap
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing snapshot in execution context"))?
            .epoch_num;
        if !header_ref.number.is_multiple_of(epoch_length) {
            tracing::trace!(
                "Skip verify validator, block_number {} is not an epoch boundary, epoch_length: {}",
                header_ref.number,
                epoch_length
            );
            return Ok(());
        }

        let (mut validators, mut vote_addrs_map) = current_validators
            .ok_or(BlockExecutionError::msg("Invalid current validators data"))?;
        validators.sort();

        let validator_num = validators.len();
        if self.spec.is_luban_transition_at_block(header_ref.number) {
            vote_addrs_map = validators
                .iter()
                .copied()
                .zip(vec![VoteAddress::default(); validator_num])
                .collect::<HashMap<_, _>>();
        }

        let validator_bytes: Vec<u8> = validators
            .into_iter()
            .flat_map(|v| {
                let mut bytes = v.to_vec();
                if self.spec.is_luban_active_at_block(header_ref.number) {
                    bytes.extend_from_slice(vote_addrs_map[&v].as_ref());
                }
                bytes
            })
            .collect();

        let expected = self
            .parlia
            .get_validator_bytes_from_header(header_ref, epoch_length)
            .ok_or_else(|| BlockExecutionError::msg("Missing validator bytes in epoch header"))?;
        if !validator_bytes.as_slice().eq(expected.as_slice()) {
            warn!("validator bytes: {:?}", hex::encode(validator_bytes));
            warn!("expected: {:?}", hex::encode(expected));
            return Err(BlockExecutionError::msg("Invalid validators"));
        }
        tracing::debug!(
            "Succeed to verify validators, block_number: {}, epoch_length: {}",
            header_ref.number,
            epoch_length
        );

        Ok(())
    }

    fn verify_turn_length(&mut self, header: Option<Header>) -> Result<(), BlockExecutionError> {
        let header_ref = header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing header in execution context"))?;
        let epoch_length = self
            .inner_ctx
            .snap
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing snapshot in execution context"))?
            .epoch_num;
        if !header_ref.number.is_multiple_of(epoch_length) ||
            !self.spec.is_bohr_active_at_timestamp(header_ref.number, header_ref.timestamp)
        {
            tracing::trace!("Skip verify turn length, block_number {} is not an epoch boundary, epoch_length: {}", header_ref.number, epoch_length);
            return Ok(());
        }
        let turn_length_from_header =
            self.parlia.get_turn_length_from_header(header_ref, epoch_length).map_err(|err| {
                BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError {
                    error: Box::new(err),
                })
            })?;
        let expected_turn_length = self.inner_ctx.expected_turn_length;
        if turn_length_matches(turn_length_from_header, expected_turn_length) {
            tracing::debug!("Succeed to verify turn length, block_number: {}", header_ref.number);
            return Ok(())
        }

        tracing::warn!(
            "Failed to verify turn length, block_number: {}, turn_length_from_header: {:?}, expected_turn_length: {:?}, epoch_length: {}",
            header_ref.number,
            turn_length_from_header,
            expected_turn_length,
            epoch_length
        );
        Err(BscBlockExecutionError::Validation(
            BscBlockValidationError::MismatchingEpochTurnLengthError,
        )
        .into())
    }

    pub(crate) fn get_turn_length(
        &mut self,
        header_number: BlockNumber,
        header_timestamp: u64,
    ) -> Result<u8, BlockExecutionError> {
        if self.spec.is_bohr_active_at_timestamp(header_number, header_timestamp) {
            let (to, data) = self.system_contracts.get_turn_length();
            let bz = self.eth_call(to, data)?;

            let turn_length = self
                .system_contracts
                .unpack_data_into_turn_length(bz.as_ref())
                .ok_or_else(|| BlockExecutionError::msg("Failed to decode system contract output"))?
                .to::<u8>();
            return Ok(turn_length);
        }

        Ok(DEFAULT_TURN_LENGTH)
    }

    fn slash_spoiled_validator(
        &mut self,
        validator: Address,
        spoiled_val: Address,
    ) -> Result<(), BlockExecutionError> {
        self.transact_system_tx(self.system_contracts.slash(spoiled_val), validator)?;

        Ok(())
    }

    pub(crate) fn transact_system_tx(
        &mut self,
        transaction: Transaction,
        sender: Address,
    ) -> Result<(), BlockExecutionError> {
        // Record system contract call
        self.executor_metrics.system_contract_calls_total.increment(1);

        // Start timing for system contract duration
        let start_time = std::time::Instant::now();

        let account =
            self.evm.account_info(&sender).map_err(BlockExecutionError::other)?.unwrap_or_default();

        let transaction = set_nonce(transaction, account.nonce);

        // Unreachable today — `finish` skips the finalization and contract-init steps that lead
        // here — but simulation must never sign, so guard rather than trust the callers.
        if self.ctx.mode == BscExecutionMode::Simulation {
            debug_assert!(false, "system tx attempted during simulation: {transaction:?}");
            tracing::warn!(
                target: "bsc::evm",
                "Skipping system transaction in simulation mode: {:?}",
                transaction.to()
            );
            return Ok(());
        }

        let signed_tx = if self.ctx.mode == BscExecutionMode::Import {
            let hash = transaction.signature_hash();
            if self.system_txs.is_empty() || hash != self.system_txs[0].signature_hash() {
                // slash tx could fail and not in the block
                if let Some(to) = transaction.to() {
                    if to == SLASH_CONTRACT &&
                        (self.system_txs.is_empty() ||
                            self.system_txs[0].to().unwrap_or_default() != SLASH_CONTRACT)
                    {
                        warn!("slash validator failed");
                        return Ok(());
                    }
                }
                warn!("unexpected transaction: {:?}", transaction);
                for tx in self.system_txs.iter() {
                    warn!("left system tx: {:?}", tx);
                }
                self.executor_metrics.system_contract_errors_total.increment(1);
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::UnexpectedSystemTx,
                )
                .into());
            }
            Some(self.system_txs.remove(0))
        } else if is_signer_initialized() {
            match sign_system_transaction(transaction.clone()) {
                Ok(signed) => Some(signed),
                Err(e) => {
                    tracing::warn!("Failed to sign system transaction: {}", e);
                    self.executor_metrics.system_contract_errors_total.increment(1);
                    return Err(BscBlockExecutionError::FailedToSignSystemTransaction {
                        error: e.to_string(),
                    }
                    .into());
                }
            }
        } else {
            tracing::warn!("Global signer not initialized for mining mode");
            self.executor_metrics.system_contract_errors_total.increment(1);
            return Err(BscBlockExecutionError::GlobalSignerNotInitializedForMiningMode.into());
        };

        // The complement of the branch above: this node signed the transaction rather than
        // taking it from the block, so it has to be recorded into the block being assembled.
        if self.ctx.mode != BscExecutionMode::Import {
            if let Some(signed) = signed_tx.clone() {
                let recovered = signed
                    .clone()
                    .try_into_recovered_unchecked()
                    .unwrap_or_else(|_| panic!("Failed to recover system transaction signature"));
                self.shared_ctx.inner.borrow_mut().assembled_system_txs.push(recovered);

                if transaction.to() == Some(STAKE_HUB_CONTRACT) {
                    if let Some(net) = crate::shared::get_network_handle() {
                        let tx_to_broadcast = signed.clone();
                        tokio::spawn(async move {
                            if let Some(txh) = net.transactions_handle().await {
                                txh.broadcast_transactions(std::iter::once(tx_to_broadcast));
                                tracing::info!(
                                    target: "bsc::evn",
                                    "Broadcasted StakeHub system tx to public network"
                                );
                            }
                        });
                    }
                }
            }
        }

        // Create TxEnv first (before moving transaction)
        let tx_to = transaction.to();
        let tx_input = transaction.input();
        let tx_selector =
            if tx_input.len() >= 4 { Some(hex::encode(&tx_input[..4])) } else { None };
        let tx_input_len = tx_input.len();
        let tx_env = Recovered::new_unchecked(
            TxEnvelope::Legacy(TxLegacy {
                to: TxKind::Call(
                    tx_to.ok_or_else(|| BlockExecutionError::msg("System tx has no recipient"))?,
                ),
                nonce: account.nonce,
                gas_limit: u64::MAX / 2,
                value: transaction.value(),
                input: tx_input.clone(),
                gas_price: 0,
                chain_id: Some(self.spec.chain().id()),
            }),
            sender,
        );

        let block_number = self.evm.block().number().to::<u64>();
        let timestamp = self.evm.block().timestamp().to::<u64>();
        let spec =
            revm_spec_by_timestamp_and_block_number(self.spec.clone(), timestamp, block_number);
        let tx_hash = signed_tx.as_ref().map(|tx| tx.tx_hash()).copied();

        precompiles::push_precompile_trace_context(
            precompiles::PrecompileTraceContext::from_parts(
                block_number,
                spec,
                true,
                tx_hash,
                tx_to,
                tx_selector.clone(),
                tx_input_len,
            ),
        );
        struct PrecompileTracePopGuard;
        impl Drop for PrecompileTracePopGuard {
            fn drop(&mut self) {
                precompiles::pop_precompile_trace_context();
            }
        }
        let _precompile_trace_pop_guard = PrecompileTracePopGuard;

        let result_and_state =
            self.evm.transact_system_transaction(&tx_env).map_err(BlockExecutionError::other)?;
        let TxResultWithState { result, pending_state: state, .. } = result_and_state;
        // NOTE(reth-v2.4.1): manual state-hook streaming removed; state hook is DB-level now.

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;

        // Record system transaction gas usage
        self.executor_metrics.system_tx_gas_used_total.increment(gas_used);

        let tx_type = {
            use alloy_consensus::TxType;
            signed_tx.as_ref().map(|tx| tx.tx_type()).unwrap_or(TxType::Legacy)
        };
        self.receipts.push(self.receipt_builder.build_receipt::<BscEvmTypes>(ReceiptBuilderCtx {
            tx_type,
            result,
            cumulative_gas_used: self.gas_used,
        }));
        self.commit_state(state);

        // Record system contract execution duration
        let duration = start_time.elapsed();
        self.executor_metrics.system_contract_duration_seconds.record(duration.as_secs_f64());

        Ok(())
    }

    fn distribute_incoming(&mut self, validator: Address) -> Result<(), BlockExecutionError> {
        let system_info = self
            .evm
            .account_info(&SYSTEM_ADDRESS)
            .map_err(BlockExecutionError::other)?
            .unwrap_or_default();
        let mut block_reward = system_info.balance.to::<u128>();

        if block_reward == 0 {
            return Ok(());
        }

        self.update_account(SYSTEM_ADDRESS, |account| account.balance = U256::ZERO)?;
        self.update_account(validator, |account| {
            account.balance = account.balance.saturating_add(U256::from(block_reward))
        })?;

        let system_reward_balance = self
            .evm
            .account_info(&SYSTEM_REWARD_CONTRACT)
            .map_err(BlockExecutionError::other)?
            .unwrap_or_default()
            .balance;

        // Kepler introduced a max system reward limit, so we need to pay the system reward to the
        // system contract if the limit is not exceeded.
        if !self.spec.is_kepler_active_at_timestamp(
            self.evm.block().number().to(),
            self.evm.block().timestamp().to(),
        ) && system_reward_balance < U256::from(MAX_SYSTEM_REWARD)
        {
            let reward_to_system = block_reward >> SYSTEM_REWARD_PERCENT;
            if reward_to_system > 0 {
                // send reward to SYSTEM_REWARD_CONTRACT from miner.
                let tx = self.system_contracts.distribute_to_system(reward_to_system);
                self.transact_system_tx(tx, validator)?;
                self.finalize_fee_collector()?;
                tracing::debug!(
                    "Distribute to system, block_number: {}, reward_to_system: {}",
                    self.evm.block().number(),
                    reward_to_system
                );

                // Track system rewards distribution
                self.rewards_metrics.system_rewards_distributed_total.increment(1);
                // Note: Truncating to u64 for metrics (large rewards unlikely)
                self.rewards_metrics
                    .system_rewards_amount_wei_total
                    .increment(reward_to_system.min(u64::MAX as u128) as u64);
            }

            block_reward -= reward_to_system;
        }

        // send all left gas fees to VALIDATOR_CONTRACT for distributing & burning.
        let tx = self.system_contracts.distribute_to_validator(validator, block_reward);
        self.transact_system_tx(tx, validator)?;
        self.finalize_fee_collector()?;
        tracing::debug!(
            "Distribute to validator, block_number: {}, block_reward: {}",
            self.evm.block().number(),
            block_reward
        );

        // Track validator rewards distribution
        self.rewards_metrics.validator_rewards_distributed_total.increment(1);
        // Note: Truncating to u64 for metrics (large rewards unlikely)
        self.rewards_metrics
            .validator_rewards_amount_wei_total
            .increment(block_reward.min(u64::MAX as u128) as u64);

        Ok(())
    }

    /// Finalizes the fee collector touched outside the EVM by reward distribution.
    fn finalize_fee_collector(&mut self) -> Result<(), BlockExecutionError> {
        // Geth's system-transaction Finalise includes the preceding protocol balance updates.
        // Native EVM finalization only sees its transaction overlay, so it can miss this account.
        // Wait until after the system call: contract execution must still observe the empty
        // account, and may fund it again. Never remove an account with a nonce or bytecode.
        if self.evm.inner.feature(EvmFeatures::EIP161) {
            let info = self.evm.account_info(&SYSTEM_ADDRESS)?;
            if info.as_ref().is_some_and(|account| account.is_empty()) {
                let mut changes = PendingState::default();
                changes.insert_account(SYSTEM_ADDRESS, info, None);
                self.commit_state(changes);
            }
        }
        Ok(())
    }

    fn distribute_finality_reward(&mut self) -> Result<(), BlockExecutionError> {
        // distribute finality reward per FF_REWARD_DISTRIBUTION_INTERVAL blocks.
        let block_number = self.evm.block().number().to::<u64>();
        if !block_number.is_multiple_of(FF_REWARD_DISTRIBUTION_INTERVAL) {
            return Ok(());
        }

        let validator = self.evm.block().beneficiary();
        let mut accumulated_weights: HashMap<Address, U256> = HashMap::new();

        let start = (block_number - FF_REWARD_DISTRIBUTION_INTERVAL).max(1);
        let end = block_number;

        // query block header and snapshot by hash from cache.
        let mut target_hash = self.ctx.base.parent_hash;
        for _ in (start..end).rev() {
            let header = self.ctx.header_by_hash(&target_hash).ok_or_else(|| {
                BlockExecutionError::msg(format!("Header not found for block hash: {target_hash}"))
            })?;
            let snap = if let Some(private) = self.ctx.private_snapshot(&header.parent_hash) {
                private
            } else {
                self.snapshot_provider
                    .as_ref()
                    .ok_or_else(|| BlockExecutionError::msg("Snapshot provider is not available"))?
                    .snapshot_by_hash(&header.parent_hash)
                    .ok_or(BlockExecutionError::msg(
                        "Failed to get snapshot from snapshot provider",
                    ))?
            };

            if let Some(attestation) =
                self.parlia.get_vote_attestation_from_header(&header, snap.epoch_num).map_err(|err| {
                    tracing::error!("Failed to distribute finality reward due to can not get vote attestation from header, block_number: {}, error: {:?}", header.number, err);
                    BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
                })?
            {
                self.process_attestation(&attestation, &header, &mut accumulated_weights)?;
            }
            target_hash = header.parent_hash;
        }

        let mut validators: Vec<Address> = accumulated_weights.keys().copied().collect();
        validators.sort();
        let weights: Vec<U256> = validators.iter().map(|val| accumulated_weights[val]).collect();

        self.transact_system_tx(
            self.system_contracts.distribute_finality_reward(validators, weights),
            validator,
        )?;
        tracing::debug!(
            "Distribute finality reward, block_number: {}, validator: {}",
            self.evm.block().number(),
            validator
        );

        Ok(())
    }

    fn process_attestation(
        &self,
        attestation: &VoteAttestation,
        parent_header: &Header,
        accumulated_weights: &mut std::collections::HashMap<Address, U256>,
    ) -> Result<(), BlockExecutionError> {
        let justified_header =
            self.ctx.header_by_hash(&attestation.data.target_hash).ok_or_else(|| {
                BlockExecutionError::msg(format!(
                    "Header not found, block_hash: {}",
                    attestation.data.target_hash
                ))
            })?;
        let parent = self.ctx.header_by_hash(&justified_header.parent_hash).ok_or_else(|| {
            BlockExecutionError::msg(format!(
                "Header not found, block_hash: {}",
                justified_header.parent_hash
            ))
        })?;
        let snapshot = if let Some(private) = self.ctx.private_snapshot(&parent.hash_slow()) {
            private
        } else {
            self.snapshot_provider
                .as_ref()
                .ok_or_else(|| BlockExecutionError::msg("Snapshot provider is not available"))?
                .snapshot_by_hash(&parent.hash_slow())
                .ok_or_else(|| {
                    BlockExecutionError::msg(format!(
                        "Snapshot not found for block hash: {}",
                        parent.hash_slow()
                    ))
                })?
        };
        let validators = &snapshot.validators;
        let mut validators_bit_set = BitSet::new();
        let vote_address_set = attestation.vote_address_set;
        for i in 0..64 {
            if (vote_address_set & (1u64 << i)) != 0 {
                validators_bit_set.insert(i);
            }
        }

        if validators_bit_set.len() > validators.len() {
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::InvalidAttestationVoteCount(GotExpected {
                    got: validators_bit_set.len() as u64,
                    expected: validators.len() as u64,
                }),
            )
            .into());
        }

        let mut valid_vote_count = 0;
        for (index, validator) in validators.iter().enumerate() {
            if validators_bit_set.contains(index) {
                *accumulated_weights.entry(*validator).or_insert(U256::ZERO) += U256::from(1);
                valid_vote_count += 1;
            }
        }

        let quorum = (validators.len() * 2).div_ceil(3); // ceil div
        if valid_vote_count > quorum {
            let reward =
                ((valid_vote_count - quorum) * COLLECT_ADDITIONAL_VOTES_REWARD_RATIO) / 100;
            *accumulated_weights.entry(parent_header.beneficiary).or_insert(U256::ZERO) +=
                U256::from(reward);
        }

        Ok(())
    }

    fn update_validator_set_v2(
        &mut self,
        max_elected_validators: U256,
        validators_election_info: Vec<ValidatorElectionInfo>,
        validator: Address,
    ) -> Result<(), BlockExecutionError> {
        let ElectedValidators { validators, voting_powers, vote_addrs } =
            get_top_validators_by_voting_power(validators_election_info, max_elected_validators);

        self.transact_system_tx(
            self.system_contracts.update_validator_set_v2(validators, voting_powers, vote_addrs),
            validator,
        )?;

        Ok(())
    }

    /// generate system txs and apply them, used by miner.
    pub(crate) fn finalize_new_block(
        &mut self,
        block: &BlockEnv,
    ) -> Result<(), BlockExecutionError> {
        tracing::debug!(
            "Start to finalize new block, block_number: {}, mode: {:?}",
            block.number(),
            self.ctx.mode
        );
        let snap = self
            .inner_ctx
            .snap
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing snapshot in execution context"))?;
        let epoch_length = snap.epoch_num;
        let expected_validator = snap.inturn_validator();
        if block.beneficiary() != expected_validator {
            let signed_recently = if self.spec.is_plato_active_at_block(block.number().to()) {
                snap.sign_recently(expected_validator)
            } else {
                snap.recent_proposers.iter().any(|(_, v)| *v == expected_validator)
            };
            if !signed_recently {
                // Note: If this is a backoff (offturn) validator trying to slash the inturn
                // validator, this block may not become part of the canonical chain.
                // The inturn validator's block has higher difficulty
                // (DIFF_INTURN=2) and will be preferred by fork choice rules.
                // This slash attempt will only succeed if the inturn validator truly failed to
                // produce a block.
                self.slash_spoiled_validator(block.beneficiary(), expected_validator)?;
                let block_hash = self.inner_ctx.header.as_ref().map(|h| h.hash_slow());
                tracing::trace!(
                    target: "bsc::evm",
                    block_number = %block.number(),
                    block_hash = ?block_hash,
                    spoiled_validator = ?expected_validator,
                    backoff_validator = ?block.beneficiary(),
                    "Try slash spoiled validator by miner"
                );
            }
        }

        self.distribute_incoming(block.beneficiary())?;

        if self.spec.is_plato_active_at_block(block.number().to()) {
            self.distribute_finality_reward()?;
        }

        // update validator set after Feynman upgrade
        let header_number = self.evm.block().number().to::<u64>();
        let header_timestamp = self.evm.block().timestamp().to::<u64>();
        let header_beneficiary = self.evm.block().beneficiary();
        let parent_header = self
            .inner_ctx
            .parent_header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing parent header in execution context"))?
            .clone();
        if self.spec.is_feynman_active_at_timestamp(header_number, header_timestamp) &&
            is_breathe_block(parent_header.timestamp, header_timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(
                header_number,
                header_timestamp,
                parent_header.timestamp,
            )
        {
            let max_elected_validators =
                self.inner_ctx.max_elected_validators.unwrap_or(U256::from(21));
            let validators_election_info =
                self.inner_ctx.validators_election_info.clone().unwrap_or_default();

            self.update_validator_set_v2(
                max_elected_validators,
                validators_election_info.clone(),
                header_beneficiary,
            )?;
            tracing::debug!("Update validator set, block_number: {}, max_elected_validators: {}, validators_election_info: {:?}", 
                header_number, max_elected_validators, validators_election_info);
        }

        // TODO: remove post cache later.
        // Use epoch_num from current snapshot (after apply) for epoch boundary check
        let is_next_epoch = (header_number + 1).is_multiple_of(epoch_length);
        if is_next_epoch {
            // cache validators
            tracing::debug!(
                "Check validator cache update: block_number={}, epoch_length={}, is_next_epoch={}",
                header_number,
                epoch_length,
                is_next_epoch
            );
            let (validators, vote_addresses) =
                self.get_current_validators(header_number, CallBlockEnv::Current)?;
            self.shared_ctx.inner.borrow_mut().current_validators =
                Some((validators, vote_addresses));
        }

        {
            // cache turnlength
            let is_bohr = self.spec.is_bohr_active_at_timestamp(header_number, header_timestamp);
            tracing::debug!(
                "Check turn length cache update: block_number={}, epoch_length={}, is_next_epoch={}, is_bohr={}",
                header_number, epoch_length, is_next_epoch, is_bohr
            );

            if is_next_epoch && is_bohr {
                let turn_length = self.get_turn_length(header_number, header_timestamp)?;
                self.shared_ctx.inner.borrow_mut().turn_length = Some(turn_length);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::turn_length_matches;

    #[test]
    fn fee_distribution_finalizes_empty_collector_in_cache_bundle_and_stream() {
        use crate::{
            chainspec::parser::parse_genesis_json,
            consensus::SYSTEM_ADDRESS,
            node::{evm::config::BscEvmConfig, primitives::BscBlock},
            system_contracts::VALIDATOR_CONTRACT,
        };
        use alloy_consensus::{Header, TxLegacy};
        use alloy_primitives::{Address, Signature, TxKind, U256};
        use evm2::{bytecode::Bytecode, evm::AccountInfo};
        use reth_ethereum_primitives::{Transaction, TransactionSigned};
        use reth_evm::{BlockExecutor, ConfigureEvm, Evm};
        use reth_primitives_traits::{Recovered, SealedBlock};
        use std::sync::{Arc, Mutex};

        let user = Address::repeat_byte(0x11);
        let validator = Address::repeat_byte(0x22);
        for collector in [
            None,
            Some(AccountInfo::default()),
            Some(AccountInfo::default().with_nonce(1)),
            Some({
                let mut info = AccountInfo::default();
                info.set_code(Bytecode::new_legacy(vec![0x00].into()));
                info
            }),
        ] {
            let spec = parse_genesis_json(
                r#"{
                    "config": {"chainId": 714, "ramanujanBlock": 0,
                               "londonBlock": 0, "keplerTime": 0},
                    "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {}
                }"#,
            )
            .unwrap();
            let block = SealedBlock::seal_slow(BscBlock {
                header: Header {
                    number: 2,
                    beneficiary: validator,
                    gas_limit: 40_000_000,
                    ..Default::default()
                },
                body: Default::default(),
            });
            let config = BscEvmConfig::bsc(spec);
            let mut db = evm2::evm::InMemoryDB::default();
            db.insert_account_info(
                &user,
                AccountInfo::default().with_balance(U256::from(1_000_000)),
            );
            if let Some(info) = collector.clone() {
                db.insert_account_info(&SYSTEM_ADDRESS, info);
            }
            let mut executor = config.executor_for_block(&mut db, &block).unwrap();
            let updates = Arc::new(Mutex::new(Vec::new()));
            let stream = updates.clone();
            executor.set_state_hook(move |update| stream.lock().unwrap().push(update));
            for nonce in 0..2 {
                let tx = TransactionSigned::new_unhashed(
                    Transaction::Legacy(TxLegacy {
                        to: TxKind::Call(user),
                        nonce,
                        gas_limit: 21_000,
                        gas_price: 1,
                        ..Default::default()
                    }),
                    Signature::test_signature(),
                );
                executor.execute_transaction(Recovered::new_unchecked(tx, user)).unwrap();
            }
            assert_eq!(
                executor.evm.account_info(&SYSTEM_ADDRESS).unwrap().unwrap().balance,
                U256::from(42_000)
            );
            let deposit = executor.system_contracts.distribute_to_validator(validator, 42_000);
            executor
                .system_txs
                .push(TransactionSigned::new_unhashed(deposit, Signature::test_signature()));
            executor.distribute_incoming(validator).unwrap();

            let expected = collector.clone().filter(|info| !info.is_empty()).map(|mut info| {
                info.code = None;
                info
            });
            assert_eq!(executor.evm.account_info(&SYSTEM_ADDRESS).unwrap(), expected);
            assert_eq!(
                executor.evm.account_info(&VALIDATOR_CONTRACT).unwrap().unwrap().balance,
                U256::from(42_000)
            );
            let bundle = executor.block_state.into_bundle();
            assert_eq!(
                bundle
                    .state
                    .get(&SYSTEM_ADDRESS)
                    .map_or(collector.is_some(), |account| account.info.is_some()),
                expected.is_some()
            );
            let updates = updates.lock().unwrap();
            let last =
                updates.iter().rev().find_map(|update| update.account(&SYSTEM_ADDRESS)).unwrap();
            assert_eq!(last.current.is_some(), expected.is_some());
        }
    }

    #[test]
    fn turn_length_match_requires_both_values_present_and_equal() {
        assert!(turn_length_matches(Some(8), Some(8)));
        assert!(!turn_length_matches(Some(8), Some(7)));
        assert!(!turn_length_matches(None, Some(8)));
        assert!(!turn_length_matches(Some(8), None));
        assert!(!turn_length_matches(None, None));
    }
}
