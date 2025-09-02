use super::executor::BscBlockExecutor;
use super::error::BscBlockExecutionError;
use super::util::set_nonce;
use crate::consensus::parlia::{DIFF_INTURN, VoteAddress, VoteAttestation, snapshot::DEFAULT_TURN_LENGTH, constants::COLLECT_ADDITIONAL_VOTES_REWARD_RATIO, util::is_breathe_block};
use crate::consensus::{SYSTEM_ADDRESS, MAX_SYSTEM_REWARD, SYSTEM_REWARD_PERCENT};
use crate::evm::transaction::BscTxEnv;
use crate::system_contracts::{SLASH_CONTRACT, SYSTEM_REWARD_CONTRACT, feynman_fork::{ValidatorElectionInfo, get_top_validators_by_voting_power, ElectedValidators}};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_evm::{eth::receipt_builder::{ReceiptBuilder, ReceiptBuilderCtx}, execute::BlockExecutionError, Database, Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, block::StateChangeSource};
use reth_primitives::{TransactionSigned, Transaction};
use reth_revm::State;
use crate::node::evm::ResultAndState;
use revm::{context::{BlockEnv, TxEnv}, Database as RevmDatabase, DatabaseCommit};
use alloy_consensus::{Header, TxReceipt, Transaction as AlloyTransaction, SignableTransaction};
use alloy_primitives::{Address, hex, TxKind, U256};
use std::collections::HashMap;
use tracing::warn;
use reth_primitives_traits::GotExpected;
use bit_set::BitSet;


impl<'a, DB, EVM, Spec, R: ReceiptBuilder> BscBlockExecutor<'a, EVM, Spec, R>
where
    DB: Database + 'a,
    EVM: Evm<
        DB = &'a mut State<DB>,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
    >,
    Spec: EthereumHardforks + crate::hardforks::BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// finalize the new block, post check and finalize the system tx.
    /// depends on parlia, header and snapshot.
    pub(crate) fn finalize_new_block(
        &mut self, 
        block: &BlockEnv
    ) -> Result<(), BlockExecutionError> {
        tracing::trace!("Start to finalize new block, block_number: {}", block.number); 
        self.verify_validators(self.inner_ctx.current_validators.clone(), self.inner_ctx.header.clone())?;
        self.verify_turn_length(self.inner_ctx.header.clone())?;

        // finalize the system txs.
        if self.inner_ctx.header.as_ref().unwrap().difficulty != DIFF_INTURN {
            tracing::debug!("Start to slash spoiled validator, block_number: {}, block_difficulty: {:?}, diff_inturn: {:?}", 
                block.number, self.inner_ctx.header.as_ref().unwrap().difficulty, DIFF_INTURN);
            let snap = self.inner_ctx.snap.as_ref().unwrap();
            let spoiled_validator = snap.inturn_validator();
            let signed_recently = if self.spec.is_plato_active_at_block(block.number.to()) {
                snap.sign_recently(spoiled_validator)
            } else {
                snap.recent_proposers.iter().any(|(_, v)| *v == spoiled_validator)
            };
            if !signed_recently {
                self.slash_spoiled_validator(block.beneficiary, spoiled_validator)?;
                tracing::debug!("Slash spoiled validator, block_number: {}, spoiled_validator: {}", block.number, spoiled_validator);
            }
        }

        self.distribute_incoming(block.beneficiary)?;

        if self.spec.is_plato_active_at_block(block.number.to()) {
            let header = self.inner_ctx.header.as_ref().unwrap().clone();
            self.distribute_finality_reward(&header)?;
        }

        // update validator set after Feynman upgrade
        let header = self.inner_ctx.header.as_ref().unwrap().clone();
        let parent_header = self.inner_ctx.parent_header.as_ref().unwrap().clone();
        if self.spec.is_feynman_active_at_timestamp(header.timestamp) &&
            is_breathe_block(parent_header.timestamp, header.timestamp) &&
            !self.spec.is_feynman_transition_at_timestamp(header.timestamp, parent_header.timestamp)
        {
            let max_elected_validators = self.inner_ctx.max_elected_validators.unwrap_or(U256::from(21));
            let validators_election_info = self.inner_ctx.validators_election_info.clone().unwrap_or_default();
 
            self.update_validator_set_v2(
                 max_elected_validators,
                 validators_election_info.clone(),
                 header.beneficiary,
             )?;
            tracing::debug!("Update validator set, block_number: {}, max_elected_validators: {}, validators_election_info: {:?}", 
                header.number, max_elected_validators, validators_election_info);
        }

        if !self.system_txs.is_empty() {
            tracing::error!("Unexpected system tx, block_number: {}, len: {}", block.number, self.system_txs.len());
            for tx in self.system_txs.iter() {
                tracing::error!("system tx: {:?}", tx);
            }
            return Err(BscBlockExecutionError::UnexpectedSystemTx.into());
        }

        let epoch_length = self.parlia.get_epoch_length(&header);
        if (header.number + 1)% epoch_length == 0 {
            // cache it on pre block.
            self.get_current_validators(header.number)?;
        }

        tracing::trace!("Succeed to finalize new block, block_number: {}", block.number);
        Ok(())
    }

    fn verify_validators(
        &mut self, 
        current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>, 
        header: Option<Header>
    ) -> Result<(), BlockExecutionError> {
        let header_ref = header.as_ref().unwrap();
        let epoch_length = self.parlia.get_epoch_length(header_ref);
        if header_ref.number % epoch_length != 0 {
            tracing::trace!("Skip verify validator, block_number {} is not an epoch boundary, epoch_length: {}", header_ref.number, epoch_length);
            return Ok(());
        }

        let (mut validators, mut vote_addrs_map) =
            current_validators.ok_or(BlockExecutionError::msg("Invalid current validators data"))?;
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

        let expected = self.parlia.get_validator_bytes_from_header(header_ref, epoch_length).unwrap();
        if !validator_bytes.as_slice().eq(expected.as_slice()) {
            // TODO: recheck it, maybe still has bugs.
            warn!("validator bytes: {:?}", hex::encode(validator_bytes));
            warn!("expected: {:?}", hex::encode(expected));
            return Err(BlockExecutionError::msg("Invalid validators"));
        }
        tracing::debug!("Succeed to verify validators, block_number: {}, epoch_length: {}", header_ref.number, epoch_length);

        Ok(())
    }

    fn verify_turn_length(
        &mut self, 
        header: Option<Header>
    ) -> Result<(), BlockExecutionError> {
        let header_ref = header.as_ref().unwrap();
        let epoch_length = self.inner_ctx.snap.as_ref().unwrap().epoch_num;
        if header_ref.number % epoch_length != 0 || !self.spec.is_bohr_active_at_timestamp(header_ref.timestamp) {
            tracing::trace!("Skip verify turn length, block_number {} is not an epoch boundary, epoch_length: {}", header_ref.number, epoch_length);
            return Ok(());
        }
        let turn_length_from_header = {
            match self.parlia.get_turn_length_from_header(header_ref, epoch_length) {
                Ok(Some(length)) => length,
                Ok(None) => return Ok(()),
                Err(err) => return Err(BscBlockExecutionError::ParliaConsensusInnerError { error: Box::new(err) }.into()),
            }
        };
        let turn_length_from_contract = self.get_turn_length(header_ref)?.unwrap();
        if turn_length_from_header == turn_length_from_contract {
            tracing::debug!("Succeed to verify turn length, block_number: {}", header_ref.number);
            return Ok(())
        }

        tracing::warn!("Failed to verify turn length, block_number: {}, turn_length_from_header: {}, turn_length_from_contract: {}, epoch_length: {}", 
            header_ref.number, turn_length_from_header, turn_length_from_contract, epoch_length);
        Err(BscBlockExecutionError::MismatchingEpochTurnLengthError.into())
    }

    fn get_turn_length(
        &mut self,
        header: &Header,
    ) -> Result<Option<u8>, BlockExecutionError> {
        if self.spec.is_bohr_active_at_timestamp(header.timestamp) {
            let (to, data) = self.system_contracts.get_turn_length();
            let bz = self.eth_call(to, data)?;

            let turn_length = self.system_contracts.unpack_data_into_turn_length(bz.as_ref()).to::<u8>();
            return Ok(Some(turn_length))
        }

        Ok(Some(DEFAULT_TURN_LENGTH))
    }

    fn slash_spoiled_validator(
        &mut self,
        validator: Address,
        spoiled_val: Address
    ) -> Result<(), BlockExecutionError> {
        self.transact_system_tx(
            self.system_contracts.slash(spoiled_val),
            validator,
        )?;

        Ok(())
    }

    pub(crate) fn transact_system_tx(
        &mut self, 
        transaction: Transaction, 
        sender: Address
    ) -> Result<(), BlockExecutionError> {
        let account = self.evm
            .db_mut()
            .basic(sender)
            .map_err(BlockExecutionError::other)?
            .unwrap_or_default();

        let transaction = set_nonce(transaction, account.nonce);
        let hash = transaction.signature_hash();
        if self.system_txs.is_empty() || hash != self.system_txs[0].signature_hash() {
            // slash tx could fail and not in the block
            if let Some(to) = transaction.to() {
                if to == SLASH_CONTRACT &&
                    (self.system_txs.is_empty() ||
                        self.system_txs[0].to().unwrap_or_default() !=
                            SLASH_CONTRACT)
                {
                    warn!("slash validator failed");
                    return Ok(());
                }
            }
            warn!("unexpected transaction: {:?}", transaction);
            for tx in self.system_txs.iter() {
                warn!("left system tx: {:?}", tx);
            }
            return Err(BscBlockExecutionError::UnexpectedSystemTx.into());
        }
        let signed_tx = self.system_txs.remove(0);

        // Create TxEnv first (before moving transaction)
        let tx_env = BscTxEnv {
            base: TxEnv {
                caller: sender,
                kind: TxKind::Call(transaction.to().unwrap()),
                nonce: account.nonce,
                gas_limit: u64::MAX / 2,
                value: transaction.value(),
                data: transaction.input().clone(),
                gas_price: 0,
                chain_id: Some(self.spec.chain().id()),
                gas_priority_fee: None,
                access_list: Default::default(),
                blob_hashes: Vec::new(),
                max_fee_per_blob_gas: 0,
                tx_type: 0,
                authorization_list: Default::default(),
            },
            is_system_transaction: true,
        };

        let result_and_state = self.evm.transact(tx_env).map_err(BlockExecutionError::other)?;
        let ResultAndState { result, state } = result_and_state;
        let mut temp_state = state.clone();
        temp_state.remove(&SYSTEM_ADDRESS);
        self.system_caller.on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);

        let gas_used = result.gas_used();
        self.gas_used += gas_used;
        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx: &signed_tx,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));
        self.evm.db_mut().commit(state);

        Ok(())
    }

    fn distribute_incoming(
        &mut self,
        validator: Address,
    ) -> Result<(), BlockExecutionError> {
        let system_account = self
            .evm
            .db_mut()
            .load_cache_account(SYSTEM_ADDRESS)
            .map_err(BlockExecutionError::other)?;

        if system_account.account.is_none() ||
            system_account.account.as_ref().unwrap().info.balance == U256::ZERO
        {
            return Ok(());
        }

        let (mut block_reward, mut transition) = system_account.drain_balance();
        transition.info = None;
        self.evm.db_mut().apply_transition(vec![(SYSTEM_ADDRESS, transition)]);
        let balance_increment = vec![(validator, block_reward)];

        self.evm
            .db_mut()
            .increment_balances(balance_increment)
            .map_err(BlockExecutionError::other)?;

        let system_reward_balance = self
            .evm
            .db_mut()
            .basic(SYSTEM_REWARD_CONTRACT)
            .map_err(BlockExecutionError::other)?
            .unwrap_or_default()
            .balance;

        // Kepler introduced a max system reward limit, so we need to pay the system reward to the
        // system contract if the limit is not exceeded.
        if !self.spec.is_kepler_active_at_timestamp(self.evm.block().timestamp.to()) &&
            system_reward_balance < U256::from(MAX_SYSTEM_REWARD)
        {
            let reward_to_system = block_reward >> SYSTEM_REWARD_PERCENT;
            if reward_to_system > 0 {
                let tx = self.system_contracts.distribute_to_system(reward_to_system);
                self.transact_system_tx(tx, validator)?;
                tracing::debug!("Distribute to system, block_number: {}, reward_to_system: {}", self.evm.block().number, reward_to_system);
            }

            block_reward -= reward_to_system;
        }

        let tx = self.system_contracts.distribute_to_validator(validator, block_reward);
        self.transact_system_tx(tx, validator)?;
        tracing::debug!("Distribute to validator, block_number: {}, block_reward: {}", self.evm.block().number, block_reward);
        
        Ok(())
    }

    fn distribute_finality_reward(
        &mut self,
        header: &Header
    ) -> Result<(), BlockExecutionError> {
        // distribute finality reward per 200 blocks.
        let distribute_interval = 200;
        if header.number % distribute_interval != 0 {
            return Ok(());
        }

        let validator = header.beneficiary;
        let mut accumulated_weights: HashMap<Address, U256> = HashMap::new();

        let start = (header.number - distribute_interval).max(1);
        let end = header.number;
        let mut target_number = header.number - 1;
        for _ in (start..end).rev() {
            let header = self.snapshot_provider.
                as_ref().
                unwrap().
                get_header(target_number)
                .ok_or_else(|| BlockExecutionError::msg(format!("Header not found for block number: {target_number}")))?;
            let snap = self.snapshot_provider.
                as_ref().
                unwrap().
                snapshot(target_number).
                ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;

            if let Some(attestation) =
                self.parlia.get_vote_attestation_from_header(&header, snap.epoch_num).map_err(|err| {
                    tracing::error!("Failed to distribute finality reward due to can not get vote attestation from header, block_number: {}, error: {:?}", header.number, err);
                    BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                })?
            {
                self.process_attestation(&attestation, &header, &mut accumulated_weights)?;
            }
            target_number = header.number - 1;
        }

        let mut validators: Vec<Address> = accumulated_weights.keys().copied().collect();
        validators.sort();
        let weights: Vec<U256> = validators.iter().map(|val| accumulated_weights[val]).collect();

        self.transact_system_tx(
            self.system_contracts.distribute_finality_reward(validators, weights),
            validator,
        )?;
        tracing::debug!("Distribute finality reward, block_number: {}, validator: {}", self.evm.block().number, validator);

        Ok(())
    }

    fn process_attestation(
        &self,
        attestation: &VoteAttestation,
        parent_header: &Header,
        accumulated_weights: &mut std::collections::HashMap<Address, U256>,
    ) -> Result<(), BlockExecutionError> {
        let justified_header = self.snapshot_provider.as_ref().unwrap().get_header(attestation.data.target_number)
            .ok_or_else(|| BlockExecutionError::msg(format!("Header not found for block number: {}", attestation.data.target_number)))?;
        let parent = self.snapshot_provider.as_ref().unwrap().get_header(justified_header.number - 1)
            .ok_or_else(|| BlockExecutionError::msg(format!("Header not found for block number: {}", justified_header.number - 1)))?;
        let snapshot = self.snapshot_provider.as_ref().unwrap().snapshot(parent.number);
        let validators = &snapshot.unwrap().validators;  
        let mut validators_bit_set = BitSet::new();
        let vote_address_set = attestation.vote_address_set;
        for i in 0..64 {
            if (vote_address_set & (1u64 << i)) != 0 {
                validators_bit_set.insert(i);
            }
        }

        if validators_bit_set.len() > validators.len() {
            return Err(BscBlockExecutionError::InvalidAttestationVoteCount(GotExpected {
                got: validators_bit_set.len() as u64,
                expected: validators.len() as u64,
            })
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
}