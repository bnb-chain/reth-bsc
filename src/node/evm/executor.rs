use super::config::{
    revm_spec_by_timestamp_and_block_number, BscBlockExecutionCtx, BscExecutionMode,
};
use super::patch::HertzPatchManager;
use crate::consensus::parlia::SnapshotProvider;
use crate::{
    consensus::{
        parlia::{Parlia, Snapshot, VoteAddress},
        SYSTEM_ADDRESS,
    },
    evm::{precompiles, transaction::BscTxEnv},
    hardforks::BscHardforks,
    metrics::{
        BscBlockchainMetrics, BscConsensusMetrics, BscExecutorMetrics, BscRewardsMetrics,
        BscVoteMetrics,
    },
    node::evm::config::BscExecutionSharedCtx,
    system_contracts::{
        feynman_fork::ValidatorElectionInfo, get_upgrade_system_contracts, is_system_transaction,
        SystemContract,
    },
};
use alloy_consensus::{Header, Transaction as _, TxReceipt, TxType, Typed2718 as _};
use alloy_eips::eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE};
use alloy_eips::{eip7685::Requests, Encodable2718};
use alloy_evm::{
    block::{
        ExecutableTx, GasOutput, StateChangePostBlockSource, StateChangePreBlockSource,
        StateChangeSource, TxResult,
    },
    eth::receipt_builder::ReceiptBuilderCtx,
};
use crate::node::evm::error::lane_reject;
use crate::consensus::payment_lane::{state::LaneState, Lane, LaneError, LaneLiveState};
use alloy_consensus::constants::KECCAK_EMPTY;
use alloy_primitives::keccak256;
use alloy_primitives::{hex, uint, Address, BlockNumber, Bytes, U256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    block::BlockValidationError,
    eth::receipt_builder::ReceiptBuilder,
    execute::{BlockExecutionError, BlockExecutor},
    system_calls::SystemCaller,
    Evm, FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, OnStateHook,
};
use reth_provider::BlockExecutionResult;
use revm::Database as _;
use revm::{
    context::result::{ExecutionResult, Output, ResultAndState, ResultGas, SuccessReason},
    context_interface::{block::Block, result::InvalidTransaction},
    state::{Account as RevmAccount, Bytecode, EvmState},
    DatabaseCommit,
};
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, error, info, trace, warn};

/// Result of executing a single BSC transaction.
pub struct BscTxResult<H> {
    pub inner: ResultAndState<H>,
    pub blob_gas_used: u64,
    pub tx_type: TxType,
    pub tx: TransactionSigned,
    pub is_system: bool,
    pub lane: Lane,
}

impl<H: Send + 'static> TxResult for BscTxResult<H> {
    type HaltReason = H;

    fn result(&self) -> &ResultAndState<H> {
        &self.inner
    }

    fn into_result(self) -> ResultAndState<H> {
        self.inner
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

pub struct BscBlockExecutor<'a, EVM, Spec, R: ReceiptBuilder>
where
    Spec: EthChainSpec,
{
    /// Reference to the specification object.
    pub(super) spec: Spec,
    /// Inner EVM.
    pub(super) evm: EVM,
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
    pub(super) system_caller: SystemCaller<Spec>,
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
    /// Deferred error from commit_transaction (e.g. hertz patch), returned from finish().
    pub(super) deferred_error: Option<BlockExecutionError>,
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
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <EVM as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<EVM as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    /// Creates a new BscBlockExecutor.
    pub(crate) fn new(
        evm: EVM,
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
        } else if !ctx.mode.authors_block() {
            // Block-authoring modes (mining, simulation) have no current header.
            warn!(
                "No header found in the context, block_number: {:?}",
                evm.block().number().to::<u64>()
            );
        }

        let parlia = Arc::new(Parlia::new(Arc::new(spec.clone()), 200));
        let spec_clone = spec.clone();
        Self {
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
            system_caller: SystemCaller::new(spec_clone),
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
            deferred_error: None,
        }
    }

    /// Applies system contract upgrades if the Feynman fork is not yet active.
    ///
    /// `source` identifies the upgrade to the state hook and therefore to the incremental
    /// state-root computation; pre-Feynman upgrades run at block begin, later ones at block end.
    fn upgrade_contracts(
        &mut self,
        block_number: BlockNumber,
        block_timestamp: u64,
        parent_timestamp: u64,
        source: StateChangeSource,
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
                self.upgrade_system_contract(address, code, source)?;
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
                self.upgrade_contracts(
                    block_number,
                    block_timestamp,
                    parent_timestamp,
                    StateChangeSource::PreBlock(StateChangePreBlockSource::Other(
                        "bsc_system_contract_upgrade",
                    )),
                )?;
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
        } else {
            // Upgrade system contracts after Feynman at block end
            if self.spec.is_feynman_active_at_timestamp(block_number, parent_timestamp) {
                trace!(
                    target: "bsc::executor::upgrade",
                    block_number,
                    parent_timestamp,
                    "Upgrading system contracts at block end (Feynman active)"
                );
                self.upgrade_contracts(
                    block_number,
                    block_timestamp,
                    parent_timestamp,
                    StateChangeSource::PostBlock(StateChangePostBlockSource::Other(
                        "bsc_system_contract_upgrade",
                    )),
                )?;
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

    /// Replaces the code of a system contract in state.
    fn upgrade_system_contract(
        &mut self,
        address: Address,
        code: Bytecode,
        source: StateChangeSource,
    ) -> Result<(), BlockExecutionError> {
        let changes = {
            let mut info = self
                .evm
                .db_mut()
                .basic(address)
                .map_err(BlockExecutionError::other)?
                .unwrap_or_default();
            info.code_hash = code.hash_slow();
            info.code = Some(code);
            let mut account = RevmAccount::from(info);
            account.mark_touch();
            let mut changes: EvmState = Default::default();
            changes.insert(address, account);
            self.commit_state(changes.clone());
            changes
        };

        // The state root is computed incrementally from the state hook (sparse trie /
        // `StateRootTask`), so a bare `db.commit` is invisible to it: the account's new
        // `code_hash` never reaches the trie and the block commits a root describing the
        // un-upgraded contract. That is what split bsc-qanet at the Pasteur transition
        // (block 21323714) - geth computed the true root and rejected the block while every
        // reth node agreed on the stale one. Report the change like `commit_transaction` does.
        self.system_caller.on_state(source, &changes);
        Ok(())
    }

    pub(crate) fn apply_history_storage_account(
        &mut self,
        block_number: BlockNumber,
    ) -> Result<bool, BlockExecutionError> {
        info!(
            target: "bsc::executor::prague",
            block_number,
            address = ?HISTORY_STORAGE_ADDRESS,
            "Deploying HistoryStorageAddress contract (Prague transition)"
        );

        let db = self.evm.db_mut();
        let old_info = db.basic(HISTORY_STORAGE_ADDRESS).map_err(|err| {
            error!(
                target: "bsc::executor::prague",
                block_number,
                error = ?err,
                "Failed to load HistoryStorageAddress account",
            );
            BlockExecutionError::other(err)
        })?;
        debug!(
            target: "bsc::executor::prague",
            block_number,
            old_nonce = ?old_info.as_ref().map(|i| i.nonce),
            old_code_hash = ?old_info.as_ref().map(|i| i.code_hash),
            "HistoryStorageAddress account before deployment"
        );

        let mut new_info = old_info.unwrap_or_default();
        new_info.code_hash = keccak256(HISTORY_STORAGE_CODE.clone());
        new_info.code = Some(Bytecode::new_raw(Bytes::from_static(&HISTORY_STORAGE_CODE)));
        new_info.nonce = 1_u64;
        new_info.balance = U256::ZERO;
        let mut account = RevmAccount::from(new_info);
        account.mark_touch();
        let mut changes: EvmState = Default::default();
        changes.insert(HISTORY_STORAGE_ADDRESS, account);
        self.commit_state(changes.clone());

        // Same reasoning as `upgrade_system_contract`: the incremental state-root pipeline only
        // sees changes reported through the hook, so this deployment must be announced or the
        // Prague transition block commits a root without it.
        self.system_caller.on_state(
            StateChangeSource::PreBlock(StateChangePreBlockSource::Other(
                "bsc_history_storage_account",
            )),
            &changes,
        );

        info!(
            target: "bsc::executor::prague",
            block_number,
            "Successfully deployed HistoryStorageAddress contract"
        );
        Ok(true)
    }
    /// Commits state changes, and with them the two facts the payment lane rests on: the DB no
    /// longer holds the parent's post-state, and whether this block has changed `0x2007`.
    ///
    /// `is_touched` is the whole test: revm skips anything else ("not touched account are never
    /// changed"), so every change that reaches the DB — new code, storage, creation, destruction
    /// — arrives touched.
    pub(super) fn commit_state(&mut self, changes: EvmState) {
        self.db_at_parent_state = false;
        self.lane_contract_changed |= changes
            .get(&crate::consensus::payment_lane::PAYMENT_LANE_CONTRACT)
            .is_some_and(|account| account.is_touched());
        self.evm.db_mut().commit(changes);
    }

    /// Gas still available to user transactions under Parlia's reservation policy — the unit
    /// **every** producer-side lane decision is expressed in, and the one place it is computed.
    ///
    /// go-bsc gets this for free: `makeEnv` initialises `environment.gasPool` to
    /// `GasLimit - EstimateGasReservedForSystemTxs` and every call site just asks the pool.
    /// reth has no such object, so this is it — the block rule is *not* a substitute, because
    /// it counts the system transactions' actual gas and would hand the difference to user
    /// traffic that the reservation has already been withheld from.
    fn producer_shared_gas(&self) -> u64 {
        let reserved = self.parlia.estimate_gas_reserved_for_system_txs(
            self.inner_ctx.parent_header.as_ref().map(|p| p.timestamp),
            self.evm.block().number().to::<u64>(),
            self.evm.block().timestamp().to::<u64>(),
        );
        self.evm
            .block()
            .gas_limit()
            .saturating_sub(reserved)
            .saturating_sub(self.gas_used)
    }

    /// Which lane this transaction's gas is booked against.
    ///
    /// Thin on purpose: [`LaneState::classify`] owns the pre-Jenner short circuit and
    /// [`rules::classify`](crate::consensus::payment_lane::rules::classify) owns every gate, so
    /// nothing about the rule can be re-decided here.
    pub(crate) fn classify_lane(
        &mut self,
        is_system: bool,
        to: Option<Address>,
        tx_type: u8,
        value: U256,
    ) -> Result<Lane, BlockExecutionError> {
        // The lane holds an `Arc` to the listed set, so this clone is two words and a refcount;
        // it is what lets the classifier borrow the executor as the live state.
        let lane = self.lane.clone();
        lane.classify(self, is_system, to, tx_type, value).map_err(lane_reject)
    }

    /// The lane verdict for a finished block. `gas_used` is the header's total, so this may only
    /// run once the last system transaction has been executed.
    ///
    /// Logs everything the verdict was derived from: nothing about the reservation reaches the
    /// header, so a disagreement between two nodes can only be diagnosed from what each side
    /// read out of `0x2007` and how it booked the block's gas.
    pub(crate) fn verify_payment_lane(&self, gas_used: u64) -> Result<(), BlockExecutionError> {
        if !self.lane.on() {
            return Ok(());
        }
        let metrics = &crate::metrics::LANE_METRICS;

        if let Err(err) = self.lane.verify(gas_used) {
            if self.ctx.mode.finalizes() {
                metrics.produce_declined.increment(1);
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
                idle = self.lane.idle(),
                ratio = self.lane.ratio(),
                listed = self.lane.listed_len(),
                receipts = self.receipts.len(),
                system_txs = self.system_txs.len(),
                "payment lane violated"
            );
            return Err(lane_reject(err));
        }

        metrics.quota.set(self.lane.quota() as f64);
        metrics.payment_gas_used.set(self.lane.used() as f64);
        metrics.idle.set(self.lane.idle() as f64);

        // Hand the config to this block's children, unless this block is what changed it. No-op
        // while producing, where the block has no hash yet — it is cached when this node later
        // imports it.
        if !self.lane_contract_changed {
            if let Some(hash) = self.ctx.header_hash {
                self.lane.inherit_to(hash);
            }
        }
        Ok(())
    }
}

/// The live state, for [`LaneState::classify`] — the block as execution has reached it, which is
/// a different view from [`LaneParentState`] and must stay one.
impl<E, Spec, R> LaneLiveState for BscBlockExecutor<'_, E, Spec, R>
where
    E: Evm<DB: alloy_evm::block::StateDB>,
    Spec: EthChainSpec,
    R: ReceiptBuilder,
{
    fn lane_code_is_empty(&mut self, addr: Address) -> Result<bool, LaneError> {
        let block = self.evm.block().number().to::<u64>();
        match self.evm.db_mut().basic(addr) {
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

impl<'a, E, Spec, R> BlockExecutor for BscBlockExecutor<'a, E, Spec, R>
where
    E: Evm<
        DB: alloy_evm::block::StateDB,
        Tx: FromRecoveredTx<R::Transaction>
                + FromRecoveredTx<TransactionSigned>
                + FromTxWithEncoded<TransactionSigned>,
        BlockEnv = crate::evm::block_env::BscBlockEnv,
    >,
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + 'static,
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt>,
    <R as ReceiptBuilder>::Transaction: Unpin + From<TransactionSigned>,
    <E as alloy_evm::Evm>::Tx: FromTxWithEncoded<<R as ReceiptBuilder>::Transaction>,
    BscTxEnv: IntoTxEnv<<E as alloy_evm::Evm>::Tx>,
    R::Transaction: Into<TransactionSigned>,
{
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;
    type Evm = E;
    type Result = BscTxResult<E::HaltReason>;

    fn apply_pre_execution_changes(&mut self) -> Result<(), BlockExecutionError> {
        let block_env = self.evm.block().clone();
        trace!(
            target: "bsc::executor",
            block_id = %block_env.number(),
            mode = ?self.ctx.mode,
            "Start to apply_pre_execution_changes"
        );

        // Update current block height and header height metrics
        let block_number = block_env.number().to::<u64>();
        self.consensus_metrics.current_block_height.set(block_number as f64);

        // pre check and prepare some intermediate data for commit parlia snapshot in finish function.
        // `check_new_block` dereferences `ctx.header`, which only exists when importing.
        if self.ctx.mode.authors_block() {
            self.prepare_new_block(&block_env)?;
        } else {
            self.check_new_block(&block_env)?;
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
            self.system_caller
                .apply_blockhashes_contract_call(self.ctx.base.parent_hash, &mut self.evm)?;
        }

        Ok(())
    }

    fn execute_transaction_without_commit(
        &mut self,
        tx: impl ExecutableTx<Self>,
    ) -> Result<BscTxResult<E::HaltReason>, BlockExecutionError> {
        use alloy_evm::RecoveredTx as _;

        let (tx_env, recovered) = tx.into_parts();
        let signer = *recovered.signer();
        let tx_signed: TransactionSigned = recovered.tx().clone();
        let tx_type = tx_signed.tx_type();

        // Detect system transactions: skip EVM execution, accumulate for later.
        let is_system = is_system_transaction(&tx_signed, signer, self.evm.block().beneficiary());

        let lane =
            self.classify_lane(is_system, tx_signed.to(), tx_signed.ty(), tx_signed.value())?;

        if is_system {
            self.system_txs.push(tx_signed.clone());
            let dummy = ResultAndState {
                result: ExecutionResult::Success {
                    reason: SuccessReason::Stop,
                    gas: ResultGas::default(),
                    logs: vec![],
                    output: Output::Call(Bytes::new()),
                },
                state: Default::default(),
            };
            return Ok(BscTxResult {
                inner: dummy,
                blob_gas_used: 0,
                tx_type,
                tx: tx_signed,
                is_system: true,
                lane,
            });
        }

        // Apply hertz patch before tx (import only — it replays a historical state fix and
        // is meaningless for a block being authored).
        if !self.ctx.mode.authors_block() {
            self.hertz_patch_manager.patch_before_tx(&tx_signed, self.evm.db_mut())?;
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

        // BEP-703's admission gate. Only where this node picks the transactions itself: a bid
        // arrives with its set fixed and is ruled on whole, before finalization, instead.
        //
        // `InvalidTx` is the sentinel the producing loop already answers by dropping this
        // transaction and the sender's later nonces; a capacity error would abort the build.
        if self.ctx.mode.packs_from_pool() && self.lane.on() {
            let shared = self.producer_shared_gas();
            if !self.lane.admits(shared, lane, tx_gas_limit) {
                crate::metrics::LANE_METRICS.general_lane_yielded.increment(1);
                debug!(
                    target: "bsc::payment_lane",
                    block = block_number,
                    tx = %tx_hash,
                    ?lane,
                    tx_gas_limit,
                    shared,
                    quota = self.lane.quota(),
                    idle = self.lane.idle(),
                    "dropping a transaction that would eat into the reservation"
                );
                return Err(BlockValidationError::InvalidTx {
                    hash: tx_hash,
                    error: Box::new(InvalidTransaction::CallerGasLimitMoreThanBlock),
                }
                .into());
            }
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
            if BscHardforks::is_cancun_active_at_timestamp(&self.spec, block_number, timestamp) {
                tx_signed.blob_gas_used().unwrap_or_default()
            } else {
                0
            };

        let inner =
            self.evm.transact(tx_env).map_err(|err| BlockExecutionError::evm(err, tx_hash))?;

        Ok(BscTxResult { inner, blob_gas_used, tx_type, tx: tx_signed, is_system: false, lane })
    }

    fn commit_transaction(
        &mut self,
        output: BscTxResult<E::HaltReason>,
    ) -> GasOutput {
        if output.is_system {
            return GasOutput::new(0);
        }

        let ResultAndState { result, state } = output.inner;

        let mut temp_state = state.clone();
        temp_state.remove(&SYSTEM_ADDRESS);
        self.system_caller
            .on_state(StateChangeSource::Transaction(self.receipts.len()), &temp_state);

        let gas_used = result.tx_gas_used();
        self.gas_used += gas_used;
        // The same value added to `self.gas_used`, so the booked payment gas is a subset of the
        // block's by construction.
        self.lane.record_used(output.lane, gas_used);
        self.blob_gas_used = self.blob_gas_used.saturating_add(output.blob_gas_used);

        self.receipts.push(self.receipt_builder.build_receipt(ReceiptBuilderCtx {
            tx_type: output.tx_type,
            evm: &self.evm,
            result,
            state: &state,
            cumulative_gas_used: self.gas_used,
        }));

        self.commit_state(state);

        // Apply hertz patch after tx (import only — see `patch_before_tx` above).
        // commit_transaction cannot return errors in the new API, so defer any error to finish().
        if !self.ctx.mode.authors_block() {
            if let Err(e) = self.hertz_patch_manager.patch_after_tx(&output.tx, self.evm.db_mut()) {
                self.deferred_error = Some(e);
            }
        }

        GasOutput::new(gas_used)
    }

    fn finish(
        mut self,
    ) -> Result<(Self::Evm, BlockExecutionResult<R::Receipt>), BlockExecutionError> {
        if let Some(err) = self.deferred_error.take() {
            return Err(err);
        }
        let block_env = self.evm.block().clone();
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

        // The whole-set form of the gate above, for transactions the caller supplied: nothing
        // can be dropped, so the finished set is held to the same inequality once — go-bsc
        // `bidSimulator.simBid` -> `LaneState.VerifyPackedBid`. Before finalization, so
        // `producer_shared_gas` still means what it means on the packing side.
        if self.ctx.mode.packs_a_caller_supplied_set() {
            self.lane
                .verify_reservation_intact(self.producer_shared_gas())
                .map_err(lane_reject)?;
        }

        match self.ctx.mode {
            // Generates and signs system txs (rewards, slashing, validator-set updates).
            BscExecutionMode::Mining | BscExecutionMode::BidSimulation => {
                self.finalize_new_block(&self.evm.block().clone())?
            }
            // Verifies the system txs already present in the received block.
            BscExecutionMode::Import => self.post_check_new_block(&self.evm.block().clone())?,
            // Neither: return the executed block as-is, matching BSC geth's simulation path.
            BscExecutionMode::Simulation => {
                trace!(
                    target: "bsc::executor",
                    block_id = %block_env.number(),
                    "Skipping Parlia finalization for simulated block"
                );
            }
        }

        // The same verdict the importer reaches, as the producer's self-check. After the mode
        // match, so the system transactions issued above are already in `self.gas_used`. Failure
        // declines the block; there is no fallback that produces with the lane switched off.
        if self.ctx.mode.finalizes() {
            self.verify_payment_lane(self.gas_used)?;
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

        // Calculate block receive time difference
        // This is the difference between current block timestamp and parent block timestamp
        let current_timestamp = self.evm.block().timestamp().to::<u64>();
        if let Some(parent_header) = &self.inner_ctx.parent_header {
            let parent_timestamp = parent_header.timestamp;
            let time_diff = (current_timestamp as i64) - (parent_timestamp as i64);
            self.blockchain_metrics.block_receive_time_diff_seconds.set(time_diff as f64);
        }

        // Note: For gas-related metrics, use reth's ExecutorMetrics:
        // - sync.execution.gas_used_histogram
        // - sync.execution.gas_per_second (can be converted to MGas/s)
        // - sync.execution.execution_duration

        Ok((
            self.evm,
            BlockExecutionResult {
                receipts: self.receipts,
                requests: Requests::default(),
                gas_used: self.gas_used,
                blob_gas_used: self.blob_gas_used,
            },
        ))
    }

    fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.system_caller.with_state_hook(hook);
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
