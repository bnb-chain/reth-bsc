use super::{
    config::{revm_spec_by_timestamp_and_block_number, BscBlockExecutionCtx, BscExecutionMode},
    patch::HertzPatchManager,
    util::NativeBlockEnv,
};
use crate::{
    consensus::{
        parlia::{
            util::calculate_millisecond_timestamp, Parlia, Snapshot, SnapshotProvider, VoteAddress,
        },
        payment_lane::{
            state::LaneState, LaneError, LaneLiveState, LaneType, PAYMENT_LANE_CONTRACT,
        },
    },
    evm::{
        blacklist::{self, BlacklistedAddressError},
        precompiles,
    },
    hardforks::BscHardforks,
    metrics::{
        BscBlockchainMetrics, BscConsensusMetrics, BscExecutorMetrics, BscRewardsMetrics,
        BscVoteMetrics,
    },
    node::evm::{
        config::BscExecutionSharedCtx,
        error::{lane_reject, BscBlockExecutionError, BscBlockValidationError},
    },
    system_contracts::{
        feynman_fork::ValidatorElectionInfo, get_upgrade_system_contracts, is_system_transaction,
        SystemContract,
    },
};
use alloy_consensus::{constants::KECCAK_EMPTY, Header, Transaction as _, TxReceipt, TxType};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip7685::Requests,
    Encodable2718,
};
use alloy_primitives::{hex, uint, Address, BlockNumber, Bytes, U256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Bal, PendingState},
    TxResultWithState,
};
use reth_bsc_evm::{evm::BscEvm, types::BscEvmTypes};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    BlockExecutionError, BlockExecutor, BlockTransactionResult, BlockValidationError, Evm,
    ExecutorTx, GasOutput, ReceiptBuilder, ReceiptBuilderCtx, RecoveredTx,
};
use reth_execution_types::{BlockExecutionOutput, BlockState, StateUpdate};
use reth_primitives_traits::Recovered;
use reth_provider::BlockExecutionResult;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, error, info, trace, warn};

/// Result of executing a single BSC transaction.
pub struct BscTxResult {
    pub inner: TxResultWithState<BscEvmTypes>,
    pub blob_gas_used: u64,
    pub tx_type: TxType,
    pub tx: TransactionSigned,
    pub is_system: bool,
    pub lane_type: LaneType,
}
impl BlockTransactionResult<BscEvmTypes> for BscTxResult {
    fn result(&self) -> &TxResultWithState<BscEvmTypes> {
        &self.inner
    }
}

/// Helper type for the input of post execution.
#[allow(clippy::type_complexity)]
#[derive(Debug, Clone)]
pub(crate) struct InnerExecutionContext {
    pub(crate) current_validators: Option<(Vec<Address>, HashMap<Address, VoteAddress>)>,
    pub(crate) expected_turn_length: Option<u8>,
    pub(crate) max_elected_validators: Option<U256>,
    pub(crate) validators_election_info: Option<Vec<ValidatorElectionInfo>>,
    pub(crate) snap: Option<Snapshot>,
    pub(crate) header: Option<Header>,
    pub(crate) parent_header: Option<Header>,
}

pub struct BscBlockExecutor<'a, Spec, R: ReceiptBuilder>
where
    Spec: EthChainSpec,
{
    /// Reference to the specification object.
    pub(super) spec: Spec,
    /// Inner EVM.
    pub(super) evm: BscEvm<'a>,
    /// Gas used in the block.
    pub(super) gas_used: u64,
    /// Whether the DB still holds the parent's post-state, i.e. nothing in this block has been
    /// committed yet. The `LaneParentState` impl refuses to read `0x2007` once it is false.
    pub(super) db_at_parent_state: bool,
    /// Whether this block has changed `0x2007` — its code as much as its storage, since a
    /// replaced contract can change what the getters mean. Invalidates the lane config the
    /// block's children would otherwise inherit. Maintained by [`Self::commit_state`].
    pub(super) lane_contract_changed: bool,
    /// This block's BEP-703 lane. Switched off until the parent is Jenner-active.
    pub(super) lane: LaneState,
    /// Total blob gas used in the block.
    pub(super) blob_gas_used: u64,
    /// Receipts of executed transactions.
    pub(super) receipts: Vec<R::Receipt>,
    /// System txs
    pub(super) system_txs: Vec<R::Transaction>,
    /// Receipt builder.
    pub(super) receipt_builder: R,
    /// System contracts used to trigger fork specific logic.
    pub(super) system_contracts: SystemContract<Spec>,
    /// Hertz patch manager for compatibility.
    hertz_patch_manager: HertzPatchManager,
    /// Context for block execution.
    pub(super) ctx: BscBlockExecutionCtx<'a>,
    /// Utility to call system caller.
    pub(super) block_state: BlockState,
    state_hook: Option<Box<dyn FnMut(StateUpdate) + Send>>,
    /// Snapshot provider for accessing Parlia validator snapshots.
    pub(super) snapshot_provider: Option<Arc<dyn SnapshotProvider + Send + Sync>>,
    /// Parlia consensus instance.
    pub(crate) parlia: Arc<Parlia<Spec>>,
    /// Inner execution context.
    pub(super) inner_ctx: InnerExecutionContext,
    /// Shared context for block execution.
    pub(super) shared_ctx: BscExecutionSharedCtx,
    /// Consensus metrics for tracking block height and other consensus stats.
    pub(super) consensus_metrics: BscConsensusMetrics,
    /// Blockchain metrics for tracking receipts and block processing.
    pub(super) blockchain_metrics: BscBlockchainMetrics,
    /// Vote metrics for tracking attestation errors.
    pub(super) vote_metrics: BscVoteMetrics,
    /// Executor metrics for tracking block execution.
    pub(super) executor_metrics: BscExecutorMetrics,
    /// Rewards metrics for tracking reward distributions.
    pub(super) rewards_metrics: BscRewardsMetrics,
    execution_diagnostics: crate::node::miner::state_read_diagnostics::ExecutionObservation,
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
    /// Creates a new BscBlockExecutor.
    pub(crate) fn new(
        evm: BscEvm<'a>,
        ctx: BscBlockExecutionCtx<'a>,
        shared_ctx: BscExecutionSharedCtx,
        spec: Spec,
        receipt_builder: R,
        system_contracts: SystemContract<Spec>,
    ) -> Self {
        let is_mainnet = spec.chain().id() == 56; // BSC mainnet chain ID
        let hertz_patch_manager = HertzPatchManager::new(is_mainnet);

        trace!("Succeed to new block executor, header: {:?}", ctx.header);
        if let Some(ref header) = ctx.header {
            crate::node::evm::util::insert_header_to_cache_with_hash(
                header.clone(),
                ctx.header_hash,
            );
        } else if ctx.mode == BscExecutionMode::Import {
            warn!(
                "No header found in the context, block_number: {:?}",
                evm.block().number().to::<u64>()
            );
        }

        let parlia = Arc::new(Parlia::new(Arc::new(spec.clone()), 200));
        let _spec_clone = spec.clone();
        let execution_diagnostics =
            crate::node::miner::state_read_diagnostics::ExecutionObservation::new(
                ctx.finish_diagnostics.map(|(kind, turn, _)| (kind, turn)),
            );
        Self {
            execution_diagnostics,
            spec,
            evm,
            gas_used: 0,
            db_at_parent_state: true,
            lane_contract_changed: false,
            lane: LaneState::off(),
            blob_gas_used: 0,
            receipts: vec![],
            system_txs: vec![],
            receipt_builder,
            system_contracts,
            hertz_patch_manager,
            ctx,
            shared_ctx,
            block_state: BlockState::default(),
            state_hook: None,
            snapshot_provider: crate::shared::get_snapshot_provider().cloned(),
            parlia,
            inner_ctx: InnerExecutionContext {
                current_validators: None,
                expected_turn_length: None,
                max_elected_validators: None,
                validators_election_info: None,
                snap: None,
                header: None,
                parent_header: None,
            },
            consensus_metrics: BscConsensusMetrics::default(),
            blockchain_metrics: BscBlockchainMetrics::default(),
            vote_metrics: BscVoteMetrics::default(),
            executor_metrics: BscExecutorMetrics::default(),
            rewards_metrics: BscRewardsMetrics::default(),
        }
    }

    /// Applies system contract upgrades if the Feynman fork is not yet active.
    ///
    /// Pre-Feynman upgrades run at block begin, later ones at block end. The upgraded state reaches
    /// the incremental state-root computation automatically: the DB-level state hook streams on
    /// every `commit` (see the note in `commit_transaction`).
    fn upgrade_contracts(
        &mut self,
        block_number: BlockNumber,
        block_timestamp: u64,
        parent_timestamp: u64,
    ) -> Result<(), BlockExecutionError> {
        trace!(
            target: "bsc::executor::upgrade",
            block_number,
            block_timestamp,
            parent_timestamp,
            "Calling get_upgrade_system_contracts"
        );

        let contracts = get_upgrade_system_contracts(
            &self.spec,
            block_number,
            block_timestamp,
            parent_timestamp,
        )
        .map_err(|_| BlockExecutionError::msg("Failed to get upgrade system contracts"))?;

        for (address, maybe_code) in contracts {
            if let Some(code) = maybe_code {
                debug!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    address = ?address,
                    code_len = code.len(),
                    "Upgrading system contract"
                );
                self.upgrade_system_contract(address, code)?;
            }
        }

        Ok(())
    }

    /// Mimics Geth-BSC's TryUpdateBuildInSystemContract function
    fn try_update_build_in_system_contract(
        &mut self,
        block_number: BlockNumber,
        block_timestamp: u64,
        parent_timestamp: u64,
        at_block_begin: bool,
    ) -> Result<(), BlockExecutionError> {
        if at_block_begin {
            // Upgrade system contracts before Feynman at block begin
            if !self.spec.is_feynman_active_at_timestamp(block_number, parent_timestamp) {
                trace!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    parent_timestamp,
                    "Upgrading system contracts at block begin (before Feynman)"
                );
                self.upgrade_contracts(block_number, block_timestamp, parent_timestamp)?;
            }

            // HistoryStorageAddress is a special system contract in BSC, which can't be upgraded
            // This must be done at block begin when Prague activates
            if self.spec.is_prague_transition_at_block_and_timestamp(
                block_number,
                block_timestamp,
                parent_timestamp,
            ) {
                info!(
                    target: "bsc::executor::prague",
                    block_number,
                    block_timestamp,
                    "Deploying HistoryStorageAddress contract (Prague transition at block begin)"
                );
                self.apply_history_storage_account(block_number)?;
            }

            // BEP-702 3.16: the CAS20 registries get their account sentinels when
            // Jenner activates, so EIP-161 cannot clear them and GovHub sees code.
            if self.spec.is_jenner_transition_at_timestamp(
                block_number,
                block_timestamp,
                parent_timestamp,
            ) {
                self.seed_cas20_registries(block_number)?;
            }
        } else {
            // Upgrade system contracts after Feynman at block end
            if self.spec.is_feynman_active_at_timestamp(block_number, parent_timestamp) {
                trace!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    parent_timestamp,
                    "Upgrading system contracts at block end (Feynman active)"
                );
                self.upgrade_contracts(block_number, block_timestamp, parent_timestamp)?;
            }
        }
        Ok(())
    }

    /// Initializes the feynman contracts
    fn initialize_feynman_contracts(
        &mut self,
        beneficiary: Address,
    ) -> Result<(), BlockExecutionError> {
        let txs = self.system_contracts.feynman_contracts_txs();
        for tx in txs {
            self.transact_system_tx(tx.into(), beneficiary)?;
        }
        Ok(())
    }

    /// Initializes the genesis contracts
    fn deploy_genesis_contracts(
        &mut self,
        beneficiary: Address,
    ) -> Result<(), BlockExecutionError> {
        let txs = self.system_contracts.genesis_contracts_txs();
        for tx in txs {
            self.transact_system_tx(tx.into(), beneficiary)?;
        }
        Ok(())
    }

    /// Applies a protocol account update and sends the same change to both root paths.
    pub(super) fn update_account(
        &mut self,
        address: Address,
        update: impl FnOnce(&mut AccountInfo),
    ) -> Result<(), BlockExecutionError> {
        let original = self.evm.account_info(&address)?;
        let mut current = original.clone().unwrap_or_default();
        update(&mut current);
        let mut changes = PendingState::default();
        changes.insert_account(address, original, Some(current));
        self.commit_state(changes);
        Ok(())
    }

    fn upgrade_system_contract(
        &mut self,
        address: Address,
        code: revm::state::Bytecode,
    ) -> Result<(), BlockExecutionError> {
        let code = Bytecode::new_legacy(code.original_bytes());
        self.update_account(address, |info| {
            info.code_hash = code.hash_slow();
            info.code = Some(code);
        })
    }

    pub(crate) fn seed_cas20_registries(
        &mut self,
        block_number: BlockNumber,
    ) -> Result<(), BlockExecutionError> {
        use crate::evm::precompiles::cas20;
        for address in [cas20::ACTIVATION_REGISTRY_ADDRESS, cas20::POLICY_REGISTRY_ADDRESS] {
            let info = self.evm.account_info(&address)?.unwrap_or_default();
            if info.code_hash != KECCAK_EMPTY && info.code_hash != alloy_primitives::B256::ZERO {
                error!(target: "bsc::executor::cas20", block_number, ?address, code_hash = ?info.code_hash, "CAS20 registry already carries code");
                continue;
            }
            self.upgrade_system_contract(address, cas20::marker_bytecode())?;
        }
        Ok(())
    }

    pub(crate) fn apply_history_storage_account(
        &mut self,
        _block_number: BlockNumber,
    ) -> Result<bool, BlockExecutionError> {
        let code = Bytecode::new_legacy(Bytes::from_static(&HISTORY_STORAGE_CODE));
        self.update_account(HISTORY_STORAGE_ADDRESS, |info| {
            info.code_hash = code.hash_slow();
            info.code = Some(code);
            info.nonce = 1;
            info.balance = U256::ZERO;
        })?;
        Ok(true)
    }

    /// Protocol updates and transaction updates share one accumulator and one streaming hook.
    pub(super) fn commit_state(&mut self, changes: PendingState) {
        let mut update = StateUpdate::default();
        for change in changes.account_changes() {
            if change.address == PAYMENT_LANE_CONTRACT {
                self.lane_contract_changed |= change.created ||
                    change.selfdestructed ||
                    change.original.map(|info| info.code_hash) !=
                        change.current.map(|info| info.code_hash) ||
                    change.is_changed();
            }
            if change.is_changed() {
                self.db_at_parent_state = false;
            }
            self.block_state
                .commit_account_changes(change, self.state_hook.as_ref().map(|_| &mut update));
        }
        if !update.is_empty() {
            self.db_at_parent_state = false;
        }
        self.evm.commit_state(&changes);
        if let Some(hook) = self.state_hook.as_mut() {
            if !update.is_empty() {
                hook(update);
            }
        }
    }

    /// Remaining producer gas after Parlia's system reserve and executed transactions.
    /// Unlike final block validation, this withholds the full reserve, not actual system gas.
    fn producer_shared_gas(&self) -> u64 {
        let reserved = self.parlia.estimate_gas_reserved_for_system_txs(
            self.inner_ctx.parent_header.as_ref().map(|p| p.timestamp),
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
        );
        self.evm.block().gas_limit().saturating_sub(reserved).saturating_sub(self.gas_used)
    }

    /// Validates total block gas after all system transactions have executed.
    pub(crate) fn verify_payment_lane(&self, gas_used: u64) -> Result<(), BlockExecutionError> {
        if !self.lane.on() {
            return Ok(());
        }

        if let Err(err) = self.lane.verify(gas_used) {
            if matches!(self.ctx.mode, BscExecutionMode::Mining | BscExecutionMode::BidSimulation) {
                crate::metrics::LANE_METRICS.produce_declined.increment(1);
            }
            error!(
                target: "bsc::payment_lane",
                mode = ?self.ctx.mode,
                block = self.evm.block().number().to::<u64>(),
                timestamp = self.evm.block().timestamp().to::<u64>(),
                parent = %self.ctx.base.parent_hash,
                gas_limit = self.evm.block().gas_limit(),
                gas_used,
                quota = self.lane.quota(),
                payment_gas_used = self.lane.used(),
                idle = self.lane.idle_lane(),
                ratio = self.lane.ratio(),
                listed = self.lane.listed_len(),
                receipts = self.receipts.len(),
                system_txs = self.system_txs.len(),
                "payment lane violated"
            );
            return Err(lane_reject(err));
        }

        self.record_imported();
        self.try_inherit_lane_meta();
        Ok(())
    }

    /// Report imported blocks only, so bid simulations cannot overwrite chain gauges.
    fn record_imported(&self) {
        if self.ctx.mode != BscExecutionMode::Import {
            return;
        }
        let metrics = &crate::metrics::LANE_METRICS;
        metrics.quota.set(self.lane.quota() as f64);
        metrics.payment_gas_used.set(self.lane.used() as f64);
        metrics.idle.set(self.lane.idle_lane() as f64);
    }

    fn try_inherit_lane_meta(&self) {
        if self.lane_contract_changed {
            return;
        }
        if let Some(hash) = self.ctx.header_hash {
            self.lane.inherit_to(hash);
        }
    }
}

impl<Spec, R> LaneLiveState for BscBlockExecutor<'_, Spec, R>
where
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
{
    fn lane_code_is_empty(&mut self, addr: Address) -> Result<bool, LaneError> {
        let block = self.evm.block().number().to::<u64>();
        match self.evm.state_db_mut().get_account(&addr) {
            Ok(None) => Ok(true),
            Ok(Some(acc)) => Ok(acc.code_hash.is_zero() || acc.code_hash == KECCAK_EMPTY),
            Err(err) => {
                error!(
                    target: "bsc::payment_lane",
                    block,
                    address = %addr,
                    error = %err,
                    "cannot read code for lane classification"
                );
                Err(LaneError::StateUnavailable(err.to_string()))
            }
        }
    }
}

/// Payment-lane admission for producer-selected transactions.
pub trait LaneAdmission {
    /// `shared` is remaining gas, excluding system and pay-bid gas reservations.
    /// State-read errors must abort the build, not skip the transaction.
    fn lane_admits_pool_tx(
        &mut self,
        tx: &Recovered<TransactionSigned>,
        shared: u64,
    ) -> Result<bool, BlockExecutionError>;
}

impl<Spec, R> BscBlockExecutor<'_, Spec, R>
where
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
{
    fn lane_admits(&self, tx: &TransactionSigned, lane_type: LaneType, shared: u64) -> bool {
        if self.lane.admits(shared, lane_type, tx.gas_limit()) {
            return true;
        }
        crate::metrics::LANE_METRICS.general_lane_yielded.increment(1);
        debug!(
            target: "bsc::payment_lane",
            block = self.evm.block().number().to::<u64>(),
            tx = %tx.hash(),
            ?lane_type,
            tx_gas_limit = tx.gas_limit(),
            shared,
            quota = self.lane.quota(),
            idle = self.lane.idle_lane(),
            "yielding the idle payment lane"
        );
        false
    }
}

impl<Spec, R> LaneAdmission for BscBlockExecutor<'_, Spec, R>
where
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
{
    fn lane_admits_pool_tx(
        &mut self,
        tx: &Recovered<TransactionSigned>,
        shared: u64,
    ) -> Result<bool, BlockExecutionError> {
        let signed = tx.inner();
        let is_system = is_system_transaction(signed, tx.signer(), self.evm.block().beneficiary());
        let lane_type = self.lane.clone().classify(self, is_system, signed).map_err(lane_reject)?;
        Ok(self.lane_admits(signed, lane_type, shared))
    }
}

impl<'a, Spec, R> BlockExecutor for BscBlockExecutor<'a, Spec, R>
where
    Spec: EthereumHardforks
        + crate::hardforks::BscHardforks
        + EthChainSpec
        + Hardforks
        + Clone
        + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
{
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;
    type Evm = BscEvm<'a>;
    type TransactionResultWithState = BscTxResult;
    type BlockAccessList = Bal;

    fn set_state_hook(&mut self, hook: impl FnMut(StateUpdate) + Send + 'static) -> bool {
        self.state_hook = Some(Box::new(hook));
        true
    }
    fn convert_block_access_list(
        bal: &alloy_eip7928::BlockAccessList,
    ) -> Result<Bal, BlockExecutionError> {
        Bal::try_from(bal.as_slice()).map_err(BlockExecutionError::other)
    }
    fn set_block_access_list(&mut self, bal: Arc<Bal>) {
        self.evm.inner.state_mut().set_bal(bal);
    }
    fn set_block_access_index(&mut self, index: alloy_eip7928::BlockAccessIndex) {
        self.evm.inner.state_mut().set_bal_index(index);
    }
    fn enable_block_access_list_builder(&mut self) {
        self.evm.inner.state_mut().enable_bal_builder();
        self.evm.inner.state_mut().reset_bal_index();
    }
    fn take_block_access_list(&mut self) -> Option<alloy_eip7928::BlockAccessList> {
        self.evm.inner.state_mut().take_bal_builder().map(Into::into)
    }
    fn into_state(self) -> reth_trie_common::bundle_state::BundleState {
        self.block_state.into_bundle()
    }
    fn validate_transaction_gas_limit(
        &mut self,
        gas_limit: u64,
    ) -> Result<(), BlockExecutionError> {
        let block_available_gas = self.evm.block().gas_limit().saturating_sub(self.gas_used);
        if gas_limit > block_available_gas {
            return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                transaction_gas_limit: gas_limit,
                block_available_gas,
            }
            .into());
        }
        Ok(())
    }

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let block_env = *self.evm.block();
        trace!(
            target: "bsc::executor",
            block_id = %block_env.number(),
            mode = ?self.ctx.mode,
            "Start to apply_pre_execution_changes"
        );

        // Update current block height and header height metrics
        let block_number = block_env.number().to::<u64>();
        self.consensus_metrics.current_block_height.set(block_number as f64);

        // pre check and prepare some intermediate data for commit parlia snapshot in finish
        // function.
        if self.ctx.mode == BscExecutionMode::Import {
            self.check_new_block(&block_env)
                .map_err(crate::node::evm::error::mark_pre_execution_error)?;
        } else {
            self.prepare_new_block(&block_env)?;
        }

        let parent_timestamp = self
            .inner_ctx
            .parent_header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing parent header in execution context"))?
            .timestamp;
        self.try_update_build_in_system_contract(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
            parent_timestamp,
            true,
        )?;

        // Apply historical block hashes if Prague is active
        if self.spec.is_prague_active_at_block_and_timestamp(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
        ) {
            trace!(
                target: "bsc::executor::prague",
                block_number = self.evm.block().number().to::<u64>(),
                parent_hash = ?self.ctx.base.parent_hash,
                "Calling apply_blockhashes_contract_call (Prague active)"
            );
            let output = self
                .evm
                .inner
                .system_call(evm2::evm::SystemTx::new(
                    HISTORY_STORAGE_ADDRESS,
                    Bytes::copy_from_slice(self.ctx.base.parent_hash.as_slice()),
                ))
                .map_err(reth_evm::execute::map_handler_error)?
                .detach();
            self.commit_state(output.pending_state);
        }

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutorTx<Self>,
    ) -> Result<BscTxResult, BlockExecutionError> {
        let started = self.execution_diagnostics.start();
        let result = (|| {
            let (tx_env, recovered) = tx.into_parts();
            let signer = *recovered.signer();
            let tx_signed: TransactionSigned = recovered.tx().clone();
            let tx_type = tx_signed.tx_type();

            // Detect system transactions: skip EVM execution, accumulate for later.
            let is_system =
                is_system_transaction(&tx_signed, signer, self.evm.block().beneficiary());

            let lane_type =
                self.lane.clone().classify(self, is_system, &tx_signed).map_err(lane_reject)?;

            if is_system {
                let dummy = TxResultWithState::<BscEvmTypes> {
                    result: evm2::TxResult::<BscEvmTypes> { status: true, ..Default::default() },
                    pending_state: PendingState::default(),
                    ..Default::default()
                };
                return Ok(BscTxResult {
                    inner: dummy,
                    blob_gas_used: 0,
                    tx_type,
                    tx: tx_signed,
                    is_system: true,
                    lane_type,
                });
            }

            // From Cancun, system transactions must be at the end of the block.
            if self.ctx.mode == BscExecutionMode::Import &&
                !self.system_txs.is_empty() &&
                BscHardforks::is_cancun_active_at_timestamp(
                    &self.spec,
                    self.evm.block().number().to::<u64>(),
                    self.evm.block().timestamp().to::<u64>(),
                )
            {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::UnexpectedNormalTx,
                )
                .into());
            }

            // Nano consensus rule: a blacklisted sender or recipient makes the block invalid.
            if self.spec.is_nano_active_at_block(self.evm.block().number().to::<u64>()) &&
                blacklist::check_tx_basic_blacklist(signer, tx_signed.to())
            {
                return Err(BlockValidationError::InvalidTx {
                    hash: tx_signed.trie_hash(),
                    error: Box::new(BlacklistedAddressError()),
                }
                .into());
            }

            // The Hertz patches replay historical state fixes, so they only apply to a block that
            // already exists.
            if self.ctx.mode == BscExecutionMode::Import {
                let changes =
                    self.hertz_patch_manager.native_patch(&tx_signed, true, &mut self.evm.inner)?;
                self.commit_state(changes);
            }

            let block_available_gas = self.evm.block().gas_limit() - self.gas_used;
            let tx_gas_limit = tx_signed.gas_limit();
            if tx_gas_limit > block_available_gas {
                return Err(BlockValidationError::TransactionGasLimitMoreThanAvailableBlockGas {
                    transaction_gas_limit: tx_gas_limit,
                    block_available_gas,
                }
                .into());
            }

            let tx_hash = tx_signed.trie_hash();
            let block_number = self.evm.block().number().to::<u64>();
            let timestamp = self.evm.block().timestamp().to::<u64>();
            let spec =
                revm_spec_by_timestamp_and_block_number(self.spec.clone(), timestamp, block_number);
            let (to, selector, input_len) = {
                let to = tx_signed.to();
                let input = tx_signed.input();
                let selector = if input.len() >= 4 { Some(hex::encode(&input[..4])) } else { None };
                (to, selector, input.len())
            };

            // Greedy merge checks admission before execution. Mining checks it here and returns
            // InvalidTx so the pool loop skips this sender instead of aborting the build.
            if self.ctx.mode == BscExecutionMode::Mining &&
                self.lane.on() &&
                !self.lane_admits(&tx_signed, lane_type, self.producer_shared_gas())
            {
                return Err(BlockValidationError::InvalidTx {
                    hash: tx_hash,
                    error: Box::new(super::error::InvalidNativeTransaction(
                        evm2::registry::HandlerError::GasLimitMoreThanBlock {
                            gas_limit: tx_gas_limit,
                            block_gas_limit: U256::from(block_available_gas),
                        },
                    )),
                }
                .into());
            }

            precompiles::push_precompile_trace_context(
                precompiles::PrecompileTraceContext::from_parts(
                    block_number,
                    spec,
                    false,
                    Some(tx_hash),
                    to,
                    selector,
                    input_len,
                ),
            );
            struct PrecompileTracePopGuard;
            impl Drop for PrecompileTracePopGuard {
                fn drop(&mut self) {
                    precompiles::pop_precompile_trace_context();
                }
            }
            let _precompile_trace_pop_guard = PrecompileTracePopGuard;

            let blob_gas_used =
                if BscHardforks::is_cancun_active_at_timestamp(&self.spec, block_number, timestamp)
                {
                    tx_signed.blob_gas_used().unwrap_or_default()
                } else {
                    0
                };

            let started = self.execution_diagnostics.start();
            let result = self.evm.transact(&tx_env);
            self.execution_diagnostics.record(1, started);
            let inner =
                result.map_err(|error| super::error::with_transaction_hash(error, tx_hash))?;

            Ok(BscTxResult {
                inner,
                blob_gas_used,
                tx_type,
                tx: tx_signed,
                is_system: false,
                lane_type,
            })
        })();
        self.execution_diagnostics.record(0, started);
        result
    }

    fn commit_transaction(
        &mut self,
        output: BscTxResult,
    ) -> Result<GasOutput, BlockExecutionError> {
        if output.is_system {
            self.system_txs.push(output.tx);
            return Ok(GasOutput::new(0, 0));
        }

        let started = self.execution_diagnostics.start();
        let TxResultWithState { result, pending_state: state, .. } = output.inner;

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;
        // Payment gas uses the same accounting as total gas.
        self.lane.record_used(output.lane_type, gas_used);
        self.blob_gas_used = self.blob_gas_used.saturating_add(output.blob_gas_used);

        self.receipts.push(self.receipt_builder.build_receipt::<BscEvmTypes>(ReceiptBuilderCtx {
            tx_type: output.tx_type,
            result,
            cumulative_gas_used: self.gas_used,
        }));

        self.commit_state(state);

        if self.ctx.mode == BscExecutionMode::Import {
            let changes =
                self.hertz_patch_manager.native_patch(&output.tx, false, &mut self.evm.inner)?;
            self.commit_state(changes);
        }

        self.execution_diagnostics.record(2, started);
        Ok(GasOutput::new(gas_used, 0))
    }

    fn finish_with_block_access_list(
        mut self,
    ) -> Result<
        (BlockExecutionOutput<R::Receipt>, Option<alloy_eip7928::BlockAccessList>),
        BlockExecutionError,
    > {
        let block_env = *self.evm.block();
        debug!(
            target: "bsc::executor",
            block_id = %block_env.number(),
            mode = ?self.ctx.mode,
            "Start to finish"
        );

        let parent_timestamp = self
            .inner_ctx
            .parent_header
            .as_ref()
            .ok_or_else(|| BlockExecutionError::msg("Missing parent header in execution context"))?
            .timestamp;
        self.try_update_build_in_system_contract(
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
            parent_timestamp,
            false,
        )?;

        // Both contract-initialization steps below issue Parlia system transactions via
        // `transact_system_tx`, so they require either a block to consume them from (import)
        // or a validator key to sign them with (mining). A simulation has neither, and its
        // caller asked a hypothetical rather than for a sealed block — so skip them, along
        // with the finalization below.
        if self.ctx.mode != BscExecutionMode::Simulation {
            // Initialize Feynman contracts on transition block
            if self.spec.is_feynman_transition_at_timestamp(
                self.evm.block().number().to::<u64>(),
                self.evm.block().timestamp().to::<u64>(),
                parent_timestamp,
            ) {
                info!(
                    target: "bsc::executor::feynman",
                    block_number = self.evm.block().number().to::<u64>(),
                    "Initializing Feynman contracts"
                );
                self.initialize_feynman_contracts(self.evm.block().beneficiary())?;
            }

            // Deploy genesis contracts on Block 1
            if self.evm.block().number() == uint!(1U256) {
                info!(
                    target: "bsc::executor::genesis",
                    "Deploying genesis contracts on Block 1"
                );
                self.deploy_genesis_contracts(self.evm.block().beneficiary())?;
            }
        }

        // Validate the complete bid before finalization consumes the system gas reservation.
        if self.ctx.mode == BscExecutionMode::BidSimulation {
            self.lane.verify_packed_bid(self.producer_shared_gas()).map_err(lane_reject)?;
        }

        match self.ctx.mode {
            // Finalize system transactions, then apply the importer's lane rule.
            BscExecutionMode::Mining | BscExecutionMode::BidSimulation => {
                self.finalize_new_block(&block_env)?;
                self.verify_payment_lane(self.gas_used)?;
            }
            // Validate the received system transactions and total lane budget.
            BscExecutionMode::Import => self.post_check_new_block(&block_env)?,
            // Neither: return the executed block as-is, matching BSC geth's simulation path.
            BscExecutionMode::Simulation => {
                trace!(
                    target: "bsc::executor",
                    block_id = %block_env.number(),
                    "Skipping Parlia finalization for simulated block"
                );
            }
        }

        // Update receipt height metric
        let block_number = self.evm.block().number().to::<u64>();
        self.blockchain_metrics.current_receipt_height.set(block_number as f64);

        // Update block execution metrics
        self.executor_metrics.executed_blocks_total.increment(1);

        // Update block insert metrics
        // Calculate total transaction size in bytes (simplified estimation)
        // Each receipt contributes approximately:
        // - Base tx overhead: ~100 bytes
        // - Per log: ~100 bytes (address + topics + data average)
        let tx_size_bytes: usize = self
            .receipts
            .iter()
            .map(|r| {
                let logs_count = r.logs().len();
                100 + logs_count * 100 // Base + logs estimation
            })
            .sum();
        self.blockchain_metrics.block_tx_size_bytes.set(tx_size_bytes as f64);

        // Record the block timestamp interval, including BSC's sub-second component.
        if let Some(parent_header) = &self.inner_ctx.parent_header {
            let current_timestamp_ms = self.evm.env.milli_timestamp();
            let parent_timestamp_ms = calculate_millisecond_timestamp(parent_header);
            let time_diff_ms = current_timestamp_ms as i128 - parent_timestamp_ms as i128;
            self.blockchain_metrics
                .block_receive_time_diff_seconds
                .set(time_diff_ms as f64 / 1000.0);
        }

        // Note: For gas-related metrics, use reth's ExecutorMetrics:
        // - sync.execution.gas_used_histogram
        // - sync.execution.gas_per_second (can be converted to MGas/s)
        // - sync.execution.execution_duration

        let bal = self.take_block_access_list();
        let result = BlockExecutionResult {
            receipts: self.receipts,
            requests: Requests::default(),
            gas_used: self.gas_used,
            blob_gas_used: self.blob_gas_used,
        };
        Ok((BlockExecutionOutput::new(result, self.block_state.into_bundle()), bal))
    }

    fn evm_mut(&mut self) -> &mut Self::Evm {
        &mut self.evm
    }

    fn evm(&self) -> &Self::Evm {
        &self.evm
    }

    fn receipts(&self) -> &[Self::Receipt] {
        &self.receipts
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        chainspec::parser::parse_genesis_json,
        node::{
            evm::{config::BscEvmConfig, error::BscBlockValidationError},
            primitives::BscBlock,
        },
        system_contracts::VALIDATOR_CONTRACT,
    };
    use alloy_consensus::{Header, TxLegacy};
    use alloy_primitives::{address, Address, Signature, TxKind};
    use reth_ethereum_primitives::{Transaction, TransactionSigned};
    use reth_evm::{BlockExecutionError, BlockExecutor, BlockValidationError, ConfigureEvm, Evm};
    use reth_primitives_traits::{Recovered, SealedBlock};

    const NANO_BLOCK: u64 = 100;
    const CANCUN_TIME: u64 = 1_000;
    const BLACKLISTED: Address = address!("0x489A8756C18C0b8B24EC2a2b9FF3D4d447F79BEc");
    const NORMAL: Address = address!("0x1000000000000000000000000000000000000001");
    const VALIDATOR: Address = address!("0x2000000000000000000000000000000000000002");
    const SYSTEM_TX: (Address, Address) = (VALIDATOR, VALIDATOR_CONTRACT);
    const NORMAL_TX: (Address, Address) = (NORMAL, NORMAL);

    #[test]
    fn miner_observation_preserves_execution_gas_state_and_validation_error() {
        use crate::node::miner::state_read_diagnostics::ExecutionObservation;
        use metrics_exporter_prometheus::PrometheusBuilder;

        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let run = |enabled: bool| {
            let spec = parse_genesis_json(
                r#"{
                "config": {"chainId": 714, "ramanujanBlock": 0},
                "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {}
            }"#,
            )
            .unwrap();
            let block = SealedBlock::seal_slow(BscBlock {
                header: Header { number: 1, gas_limit: 40_000_000, ..Default::default() },
                body: Default::default(),
            });
            let config = BscEvmConfig::bsc(spec);
            let mut db = evm2::evm::InMemoryDB::default();
            let mut executor = config.executor_for_block(&mut db, &block).unwrap();
            executor.execution_diagnostics =
                ExecutionObservation::new(enabled.then_some(("regular", "inturn")));
            let tx = TransactionSigned::new_unhashed(
                Transaction::Legacy(TxLegacy {
                    to: TxKind::Call(NORMAL),
                    gas_limit: 21_000,
                    ..Default::default()
                }),
                Signature::test_signature(),
            );
            let tx = Recovered::new_unchecked(tx, NORMAL);
            let output = executor.execute_transaction_without_commit(tx.clone()).unwrap();
            let gas = executor.commit_transaction(output).unwrap().tx_gas_used();
            let error = executor
                .execute_transaction_without_commit(tx)
                .err()
                .expect("reused nonce must fail")
                .to_string();
            let receipts = executor.receipts.clone();
            let account = executor.evm_mut().account_info(&NORMAL).unwrap();
            drop(executor);
            (gas, error, receipts, account)
        };
        metrics::with_local_recorder(&recorder, || {
            let baseline = run(false);
            assert!(!handle.render().contains("bsc_build_diagnostics_transaction_stage"));
            let observed = run(true);
            assert_eq!(baseline, observed);
            assert_eq!(observed.0, 21_000);
            assert_eq!(observed.3.unwrap().nonce, 1);
        });
        let rendered = handle.render();
        for (stage, count) in [("executor_prepare", 2), ("evm_transact", 2), ("receipt_commit", 1)]
        {
            assert!(
                rendered.lines().any(|l| l
                    .starts_with("bsc_build_diagnostics_transaction_stage_calls_total") &&
                    l.contains(stage) &&
                    l.ends_with(&format!(" {count}"))),
                "{rendered}"
            );
        }
    }

    #[test]
    fn block_interval_metric_preserves_milliseconds() {
        use crate::{
            consensus::parlia::util::set_millisecond_part_of_timestamp,
            metrics::BscBlockchainMetrics, node::evm::config::BscExecutionMode,
        };
        use metrics_exporter_prometheus::PrometheusBuilder;

        let spec = parse_genesis_json(
            r#"{
                "config": {"chainId": 714, "ramanujanBlock": 0},
                "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {}
            }"#,
        )
        .expect("genesis should parse");
        let evm_config = BscEvmConfig::bsc(spec);

        for (parent_ms, current_ms, expected_seconds) in [
            (1_000_100, 1_000_550, 0.45), // Both blocks within the same second.
            (1_000_800, 1_001_250, 0.45), // Cross a second boundary.
            (1_000_500, 1_001_250, 0.75),
            (1_000_750, 1_002_250, 1.5),
            (1_000_000, 1_003_000, 3.0), // Legacy whole-second timestamps.
        ] {
            let mut parent = Header {
                number: 100,
                timestamp: parent_ms / 1000,
                gas_limit: 40_000_000,
                ..Default::default()
            };
            set_millisecond_part_of_timestamp(parent_ms, &mut parent);
            let mut header = Header {
                number: parent.number + 1,
                timestamp: current_ms / 1000,
                gas_limit: parent.gas_limit,
                ..Default::default()
            };
            set_millisecond_part_of_timestamp(current_ms, &mut header);
            let block = SealedBlock::seal_slow(BscBlock { header, body: Default::default() });
            let mut db = evm2::evm::InMemoryDB::default();
            let mut executor = evm_config.executor_for_block(&mut db, &block).unwrap();
            executor.inner_ctx.parent_header = Some(parent);
            // Exercise finish's metric emission without requiring Parlia system transactions.
            executor.ctx.mode = BscExecutionMode::Simulation;
            let recorder = PrometheusBuilder::new().build_recorder();
            executor.blockchain_metrics = metrics::with_local_recorder(&recorder, || {
                BscBlockchainMetrics::new_with_labels(&[] as &[(&str, &str)])
            });
            executor.finish().expect("empty block should finish");

            let output = recorder.handle().render();
            let value = output
                .lines()
                .find_map(|line| {
                    line.strip_prefix("bsc_blockchain_block_receive_time_diff_seconds ")
                })
                .expect("block interval gauge should be exported")
                .parse::<f64>()
                .unwrap();
            assert_eq!(value, expected_seconds, "parent={parent_ms}ms, block={current_ms}ms");
        }
    }

    /// Imports `txs`, zero-gas-price calls given as `(from, to)`, into a block mined by
    /// `VALIDATOR`, so a call from it to a system contract is a system transaction.
    fn import(
        number: u64,
        timestamp: u64,
        txs: &[(Address, Address)],
    ) -> Result<(), BlockExecutionError> {
        let spec = parse_genesis_json(&format!(
            r#"{{
                "config": {{
                    "chainId": 714, "ramanujanBlock": 0, "nielsBlock": 0, "nanoBlock": {NANO_BLOCK},
                    "berlinBlock": 0, "londonBlock": 0, "shanghaiTime": 0, "keplerTime": 0,
                    "cancunTime": {CANCUN_TIME}
                }},
                "difficulty": "0x1", "gasLimit": "0x2625a00", "alloc": {{}}
            }}"#
        ))
        .expect("genesis should parse");
        let block = SealedBlock::seal_slow(BscBlock {
            header: Header {
                number,
                timestamp,
                beneficiary: VALIDATOR,
                gas_limit: 40_000_000,
                blob_gas_used: Some(0),
                excess_blob_gas: Some(0),
                ..Default::default()
            },
            body: Default::default(),
        });
        let evm_config = BscEvmConfig::bsc(spec);
        let mut db = evm2::evm::InMemoryDB::default();
        let mut executor = evm_config.executor_for_block(&mut db, &block).unwrap();
        for &(from, to) in txs {
            let tx = TransactionSigned::new_unhashed(
                Transaction::Legacy(TxLegacy {
                    to: TxKind::Call(to),
                    gas_limit: 21_000,
                    ..Default::default()
                }),
                Signature::test_signature(),
            );
            executor.execute_transaction(Recovered::new_unchecked(tx, from))?;
        }
        Ok(())
    }

    #[test]
    fn blacklisted_sender_or_recipient_is_rejected_from_nano() {
        for tx in [(BLACKLISTED, NORMAL), (NORMAL, BLACKLISTED)] {
            assert!(matches!(
                import(NANO_BLOCK, 0, &[tx]),
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx { .. }))
            ));
            import(NANO_BLOCK - 1, 0, &[tx]).expect("blacklist is not enforced before Nano");
        }
        import(NANO_BLOCK, 0, &[NORMAL_TX]).expect("clean tx executes");
    }

    #[test]
    fn normal_tx_after_system_tx_is_rejected_from_cancun() {
        let expected = BscBlockValidationError::UnexpectedNormalTx.to_string();
        assert!(matches!(
            import(NANO_BLOCK, CANCUN_TIME, &[SYSTEM_TX, NORMAL_TX]),
            Err(BlockExecutionError::Validation(BlockValidationError::DepositRequestDecode(msg)))
                if msg.ends_with(&expected)
        ));
        import(NANO_BLOCK, CANCUN_TIME - 1, &[SYSTEM_TX, NORMAL_TX])
            .expect("order is not enforced before Cancun");
        import(NANO_BLOCK, CANCUN_TIME, &[NORMAL_TX, SYSTEM_TX])
            .expect("system txs at the end are allowed");
    }
}

#[cfg(test)]
mod original_info_ordering_tests {
    use super::*;
    use alloy_primitives::keccak256;
    use revm::state::{Account as RevmAccount, AccountInfo};

    /// `impl From<AccountInfo> for Account` snapshots whatever it is handed into
    /// `original_info`, and reth's `evm_state_to_hashed_post_state` decides whether an account
    /// changed with `info != original_info()`. So a hand-built account must be constructed
    /// from the PRE-state info and mutated afterwards; building it from already-mutated info
    /// makes the two equal and the change is silently dropped from the state-root task's
    /// delta while `db.commit` still applies it, producing a wrong sealed state root.
    ///
    /// Guards the ordering in `upgrade_system_contract` and
    /// `apply_history_storage_account`.
    #[test]
    fn mutating_after_conversion_keeps_original_info_distinct() {
        let old = AccountInfo { nonce: 7, balance: U256::from(1_000u64), ..Default::default() };

        // Correct ordering: construct from the pre-state, then mutate.
        let mut account = RevmAccount::from(old.clone());
        account.info.code_hash = keccak256([0xaa_u8, 0xbb]);
        account.mark_touch();

        assert_ne!(
            account.info,
            account.original_info(),
            "account built from pre-state info must read as changed"
        );
        assert_eq!(account.original_info(), old, "original_info must hold the pre-state value");
    }

    /// The inverse: the ordering this fix removed. Kept so the failure mode stays visible --
    /// if this ever stops holding, revm changed its semantics and the guard above can be
    /// revisited.
    #[test]
    fn mutating_before_conversion_hides_the_change() {
        let info = AccountInfo {
            nonce: 7,
            balance: U256::from(1_000u64),
            code_hash: keccak256([0xaa_u8, 0xbb]),
            ..Default::default()
        };

        let mut account = RevmAccount::from(info);
        account.mark_touch();

        assert_eq!(
            account.info,
            account.original_info(),
            "mutating before conversion makes the change invisible to the state-root task"
        );
    }
}
