use super::{
    assembler::BscBlockAssembler, builder::BscBlockBuilder, executor::BscBlockExecutor,
    factory::BscEvmFactory,
};
use crate::{
    chainspec::BscChainSpec,
    consensus::{
        eip4844::next_block_excess_blob_gas_with_mendel,
        parlia::{util::millisecond_remainder, VoteAddress},
    },
    hardforks::{bsc::BscHardfork, BscHardforks},
    node::engine_api::validator::BscExecutionData,
    system_contracts::{feynman_fork::ValidatorElectionInfo, SystemContract},
    BscPrimitives,
};
use alloy_consensus::{transaction::SignerRecoverable, BlockHeader, Header, TxReceipt};
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{Address, BlockHash, Log, U256};
use evm2::{env::BlockEnv, SpecId, Version};
use reth_bsc_evm::{env::BscEvmEnv, evm::BscEvm, types::BscEvmTypes};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{
    execute::BlockBuilder, BlockExecutorFactory, ConfigureEngineEvm, ConfigureEvm, Database,
    DynDatabase, EvmEnvFor, EvmFor, ExecutableTxIterator, ExecutionCtxFor, NextBlockEnvAttributes,
    ReceiptBuilder,
};
use reth_evm_ethereum::{EthBlockExecutionCtx, RethReceiptBuilder};
use reth_primitives_traits::{BlockTy, HeaderTy, SealedBlock, SealedHeader};
use reth_rpc_eth_api::helpers::pending_block::BuildPendingEnv;
use std::{
    borrow::Cow,
    cell::RefCell,
    convert::Infallible,
    rc::Rc,
    sync::{Arc, Mutex},
};

/// Shared sink type for transporting `(current_validators, vote_addresses)` from the builder to
/// the payload/bid layer so that VALIDATOR_CACHE can be written after the definitive block hash
/// is known.
pub type ValidatorCacheSink = Arc<Mutex<Option<(Vec<Address>, Vec<VoteAddress>)>>>;

/// Sink carrying the sparse-trie background task's precomputed
/// `(state_root, trie_updates)`, threaded from the payload layer to the builder's
/// MDBX branch so it can skip the blocking `state_root_with_updates` call.
pub type StateRootPrecomputedSink =
    Arc<Mutex<Option<(alloy_primitives::B256, reth_trie_common::updates::TrieUpdates)>>>;

/// Parent data owned by one unpublished mining computation. Never inserted in the
/// global header/snapshot caches, which may persist snapshots at checkpoints.
#[derive(Debug, Clone)]
pub struct PrivateMiningParent {
    pub header: SealedHeader<Header>,
    pub snapshot: Arc<crate::consensus::parlia::Snapshot>,
    pub ancestor: Option<Arc<PrivateMiningParent>>,
}
impl PrivateMiningParent {
    /// Keep only unpublished-window metadata. Accepted ancestors are resolved by
    /// the ordinary snapshot provider; their private snapshots must not grow forever.
    pub(crate) fn retained_from(&self, number: u64) -> Option<Arc<Self>> {
        (self.header.number >= number).then(|| {
            Arc::new(Self {
                header: self.header.clone(),
                snapshot: self.snapshot.clone(),
                ancestor: self.ancestor.as_ref().and_then(|parent| parent.retained_from(number)),
            })
        })
    }
    fn find(&self, hash: &alloy_primitives::B256) -> Option<&Self> {
        let mut current = Some(self);
        while let Some(parent) = current {
            if parent.header.hash() == *hash {
                return Some(parent)
            }
            current = parent.ancestor.as_deref();
        }
        None
    }
}

/// What the executor is doing with the block it is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BscExecutionMode {
    /// Verifying a block that already exists: the header is given, and system transactions are
    /// consumed from the block rather than generated.
    Import,
    /// Producing a block this validator picks the transactions for, then signs and broadcasts.
    Mining,
    /// Runs a fixed bid with Parlia finalization. Optional pool additions require explicit
    /// admission, separate from the builder's fixed transactions.
    BidSimulation,
    /// Answering a hypothetical (`eth_simulateV1`, local pending block): authors a header, but
    /// runs no Parlia finalization and signs nothing.
    Simulation,
}

/// BSC wrapper around [`NextBlockEnvAttributes`].
///
/// Extends the upstream attributes with sparse-trie sinks and validator/turn-length
/// transport sinks needed by the BSC miner. The struct still satisfies upstream RPC
/// trait bounds via a delegating [`BuildPendingEnv`] implementation, keeping reth's
/// base attributes unchanged.
#[derive(Debug, Clone)]
pub struct BscNextBlockEnvAttributes {
    pub inner: NextBlockEnvAttributes,
    /// [`BuildPendingEnv`] defaults to simulation. Producers must explicitly select
    /// [`BscExecutionMode::Mining`] or [`BscExecutionMode::BidSimulation`].
    pub mode: BscExecutionMode,
    /// Sink for transporting `current_validators` from builder to payload layer without writing
    /// to VALIDATOR_CACHE prematurely (hash not yet final at build time).
    pub validator_cache_sink: Option<ValidatorCacheSink>,
    /// Sink for transporting `turn_length` from builder to payload layer without writing to
    /// TURN_LENGTH_CACHE prematurely.
    pub turn_length_sink: Option<Arc<Mutex<Option<u8>>>>,
    /// Sink for precomputed `(state_root, trie_updates)` from a sparse-trie background
    /// task. Filled by the payload layer between exec and `finish` so the builder can
    /// skip the blocking `state_root_with_updates` call. See
    /// [`BscBlockExecutionCtx::state_root_precomputed_sink`] for full semantics.
    pub state_root_precomputed_sink: Option<StateRootPrecomputedSink>,
    /// Sparse-trie state-root handle, threaded through to `finish`.
    ///
    /// Stored here (in `Arc<Mutex<Option<_>>>` so `Clone` works for the type-erased
    /// builder path) so that `state_root()` can be called **after** `executor.finish()`
    /// runs BSC's post-execution system transactions (slash, fee distribution,
    /// validator-set updates). Those system txs change state via the same executor
    /// that has the `state_hook` installed; the hook is dropped naturally when the
    /// executor is consumed by `finish()`, which sends `FinishedStateUpdates` to the
    /// background task. Only after that drop is it safe to await `state_root()`.
    pub trie_handle: Option<Arc<Mutex<Option<reth_engine_tree::tree::StateRootHandle>>>>,
    /// Absolute wall-clock deadline (epoch ms) for bounding the sparse-trie
    /// `state_root()` wait in `finish`. Past it the builder stops waiting and falls
    /// back to synchronous `state_root_with_updates`, so an in-turn block never
    /// blocks unboundedly past its slot. `None` = legacy unbounded blocking wait
    /// (out-of-turn / bid-sim / import paths).
    pub state_root_deadline_ms: Option<u64>,
    /// Monotonic local-build budget and cancellation; absent for import and RPC.
    pub build_control: Option<crate::node::miner::build_control::BuildControl>,
    /// Miner-only bounded (attempt kind, consensus turn, private depth) labels for
    /// execution/finish diagnostics. None for import, RPC and bid simulation; never infer kind
    /// from transaction count.
    pub finish_diagnostics: Option<(&'static str, &'static str, usize)>,
    /// Exact unpublished parent context; absent for canonical mining, import and RPC.
    pub private_parent: Option<Arc<PrivateMiningParent>>,
    /// Sub-second millisecond remainder (BEP-520) of the block being built, consumed by
    /// [`BscBlockEnv`] and the BEP-706 precompile. The miner and bid simulator fill it
    /// with `block_timestamp_ms % 1000` (the planned millisecond timestamp that is
    /// later sealed into the header's `mix_hash`); the RPC pending/simulate paths leave
    /// it `0`, matching go-bsc's synthetic headers whose zero `MixDigest` yields
    /// `Time*1000`.
    pub milli_remainder: u64,
}

impl<H: BlockHeader> BuildPendingEnv<H> for BscNextBlockEnvAttributes {
    fn build_pending_env(
        parent: &SealedHeader<H>,
        block_overrides: Option<&alloy_rpc_types_eth::BlockOverrides>,
    ) -> Self {
        Self {
            inner: NextBlockEnvAttributes::build_pending_env(parent, block_overrides),
            // This is the RPC-side entry point (`eth_simulateV1`, local pending block), not
            // the miner. Simulation must not run Parlia finalization or sign system txs.
            mode: BscExecutionMode::Simulation,
            validator_cache_sink: None,
            turn_length_sink: None,
            state_root_precomputed_sink: None,
            trie_handle: None,
            state_root_deadline_ms: None,
            build_control: None,
            finish_diagnostics: None,
            private_parent: None,
            // RPC pending/simulate blocks have no millisecond source — second precision
            // (`Time*1000`), like go-bsc's zero-MixDigest synthetic headers.
            milli_remainder: 0,
        }
    }
}

/// Type alias for system transactions to reduce complexity
type SystemTxs =
    Vec<reth_primitives_traits::Recovered<reth_primitives_traits::TxTy<crate::BscPrimitives>>>;

#[derive(Debug, Clone, Default)]
pub struct BscExecutionSharedCtxInner {
    /// current validators for miner to produce block.
    pub current_validators: Option<(Vec<Address>, Vec<VoteAddress>)>,
    /// max elected validators for miner to produce block.
    pub max_elected_validators: Option<U256>,
    /// validators election info for miner to produce block.
    pub validators_election_info: Option<Vec<ValidatorElectionInfo>>,
    /// turn length for miner to produce block.
    pub turn_length: Option<u8>,
    /// assembled system txs.
    pub assembled_system_txs: SystemTxs,
}

#[derive(Debug, Clone)]
pub struct BscExecutionSharedCtx {
    pub inner: Rc<RefCell<BscExecutionSharedCtxInner>>,
}

impl Default for BscExecutionSharedCtx {
    fn default() -> Self {
        Self { inner: Rc::new(RefCell::new(BscExecutionSharedCtxInner::default())) }
    }
}

/// Context for BSC block execution.
/// Contains all the fields from EthBlockExecutionCtx plus additional header field.
#[derive(Debug, Clone)]
pub struct BscBlockExecutionCtx<'a> {
    /// Base Ethereum execution context.
    pub base: EthBlockExecutionCtx<'a>,
    /// Block header (optional for BSC-specific logic).
    pub header: Option<Header>,
    /// Block hash when known (sealed block), to avoid re-hashing.
    pub header_hash: Option<BlockHash>,
    /// What this execution is for: verifying, mining, or simulating.
    pub mode: BscExecutionMode,
    /// Sink for `current_validators` — written by builder in `finish()` and read by the
    /// payload layer after the builder is consumed. `None` for non-miner paths.
    pub validator_cache_sink: Option<ValidatorCacheSink>,
    /// Sink for `turn_length` — same lifecycle as `validator_cache_sink`.
    pub turn_length_sink: Option<Arc<Mutex<Option<u8>>>>,
    /// Sink for a precomputed `(state_root, trie_updates)` from a sparse-trie background
    /// task (reth 2.0 mechanism).
    ///
    /// Write direction is **reversed** vs the other sinks: the payload layer fills this
    /// **before** calling `finish`, and the builder reads it to skip the synchronous
    /// `state_root_with_updates` call. `None` in the bid simulator path and when the
    /// `--mining.use-sparse-trie-state-root` flag is off, triggering the legacy
    /// state-root path.
    pub state_root_precomputed_sink: Option<StateRootPrecomputedSink>,
    /// Sparse-trie state-root handle. The builder consumes this **after**
    /// `executor.finish()` runs BSC's post-execution system transactions (slash,
    /// fee distribution, validator-set updates), so those state changes are
    /// captured by the executor's `state_hook` before the hook is dropped (which
    /// signals the sparse-trie task to finalize). Calling `state_root()` before
    /// the executor is dropped would deadlock the task on `FinishedStateUpdates`.
    ///
    /// `Arc<Mutex<Option<_>>>` because `StateRootHandle` is `!Clone` (single-use
    /// receiver) and `BscBlockExecutionCtx` derives `Clone`.
    pub trie_handle: Option<Arc<Mutex<Option<reth_engine_tree::tree::StateRootHandle>>>>,
    /// See [`BscNextBlockEnvAttributes::state_root_deadline_ms`]. Bounds the
    /// sparse-trie `state_root()` wait in `finish`.
    pub state_root_deadline_ms: Option<u64>,
    /// Monotonic local-build budget and cancellation; absent for import and RPC.
    pub build_control: Option<crate::node::miner::build_control::BuildControl>,
    /// Miner-only bounded (attempt kind, consensus turn, private depth) labels for
    /// execution/finish diagnostics. None for import, RPC and bid simulation; never infer kind
    /// from transaction count.
    pub finish_diagnostics: Option<(&'static str, &'static str, usize)>,
    /// Exact unpublished parent context; absent for canonical mining, import and RPC.
    pub private_parent: Option<Arc<PrivateMiningParent>>,
}

impl<'a> BscBlockExecutionCtx<'a> {
    pub(crate) fn header_by_hash(&self, hash: &alloy_primitives::B256) -> Option<Header> {
        self.private_parent
            .as_ref()
            .and_then(|p| p.find(hash))
            .map(|p| p.header.header().clone())
            .or_else(|| super::util::get_header_by_hash_from_cache(hash))
    }
    pub(crate) fn private_snapshot(
        &self,
        hash: &alloy_primitives::B256,
    ) -> Option<crate::consensus::parlia::Snapshot> {
        self.private_parent.as_ref().and_then(|p| p.find(hash)).map(|p| p.snapshot.as_ref().clone())
    }

    /// Convert to EthBlockExecutionCtx for compatibility with existing BlockAssembler.
    pub fn as_eth_context(&self) -> &EthBlockExecutionCtx<'a> {
        &self.base
    }
}

/// Ethereum-related EVM configuration.
#[derive(Debug, Clone)]
pub struct BscEvmConfig {
    /// Inner [`BscBlockExecutorFactory`].
    pub executor_factory:
        BscBlockExecutorFactory<RethReceiptBuilder, Arc<BscChainSpec>, BscEvmFactory>,
    /// BSC block assembler.
    pub block_assembler: BscBlockAssembler<BscChainSpec>,
}

impl BscEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec.
    pub fn new(chain_spec: Arc<BscChainSpec>) -> Self {
        Self::bsc(chain_spec)
    }

    /// Creates a new Ethereum EVM configuration.
    pub fn bsc(chain_spec: Arc<BscChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, BscEvmFactory::default())
    }
}

impl BscEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(chain_spec: Arc<BscChainSpec>, evm_factory: BscEvmFactory) -> Self {
        Self {
            block_assembler: BscBlockAssembler::new(chain_spec.clone()),
            executor_factory: BscBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<BscChainSpec> {
        self.executor_factory.spec()
    }

    /// Reconciles inherited opcode and gas rules after a block-number or time override.
    pub(crate) fn with_block_rules(&self, mut env: BscEvmEnv) -> BscEvmEnv {
        let number = env.block.number.saturating_to();
        let timestamp = env.block.timestamp.saturating_to();
        let spec =
            revm_spec_by_timestamp_and_block_number(self.chain_spec().clone(), timestamp, number);
        if spec == env.spec {
            return env;
        }
        let old_defaults = Version::new(env.spec.into());
        let removed = old_defaults.features & !env.version.features;
        let added = env.version.features & !old_defaults.features;
        let mut version = Version::new(spec.into());
        version.chain_id = env.version.chain_id;
        version.features = (version.features & !removed) | added;
        if env.version.max_code_size != old_defaults.max_code_size {
            version.max_code_size = env.version.max_code_size;
        }
        if env.version.max_initcode_size != old_defaults.max_initcode_size {
            version.max_initcode_size = env.version.max_initcode_size;
        }
        version.initial_call_depth = env.version.initial_call_depth;
        version.memory_limit = env.version.memory_limit;
        if env.version.tx_gas_limit_cap != old_defaults.tx_gas_limit_cap ||
            !env.version.feature(evm2::EvmFeatures::BLOCK_GAS_LIMIT_CHECK)
        {
            version.tx_gas_limit_cap = env.version.tx_gas_limit_cap;
        }
        if let Some(params) = self.chain_spec().blob_params_at_timestamp(timestamp) {
            version.max_blobs_per_tx = params.max_blobs_per_tx as usize;
            if SpecId::from(env.spec) < SpecId::CANCUN &&
                SpecId::from(spec) >= SpecId::CANCUN &&
                !env.blob_base_fee_overridden
            {
                env.block.blob_basefee = U256::from(params.calc_blob_fee(0));
            }
        }
        if env.version.max_blobs_per_tx != old_defaults.max_blobs_per_tx {
            version.max_blobs_per_tx = env.version.max_blobs_per_tx;
        }
        env.spec = spec;
        env.version = version;
        env
    }
}

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct BscBlockExecutorFactory<
    R = RethReceiptBuilder,
    Spec = Arc<BscChainSpec>,
    EvmFactory = BscEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> BscBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`BscBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`ReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }
}

impl<R, Spec> BlockExecutorFactory for BscBlockExecutorFactory<R, Spec, BscEvmFactory>
where
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt<Log = Log>>
        + Clone
        + 'static,
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + Clone + 'static,
{
    type EvmFactory = BscEvmFactory;
    type EvmTypes = BscEvmTypes;
    type Evm<'a> = BscEvm<'a>;
    type EvmEnv = BscEvmEnv;
    type ExecutionCtx<'a> = BscBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;
    type Executor<'a> = BscBlockExecutor<'a, Spec, R>;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn evm_with_env<'a, DB: DynDatabase + 'a>(&self, db: DB, env: BscEvmEnv) -> BscEvm<'a> {
        self.evm_factory.create_evm(db, env)
    }
    fn create_executor<'a>(
        &'a self,
        evm: BscEvm<'a>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> Self::Executor<'a>
    where
        Self: 'a,
    {
        BscBlockExecutor::new(
            evm,
            ctx,
            BscExecutionSharedCtx::default(),
            self.spec().clone(),
            self.receipt_builder().clone(),
            SystemContract::new(self.spec().clone()),
        )
    }
}

pub(crate) const EIP1559_INITIAL_BASE_FEE: u64 = 0;

/// Resolves opcode-visible fields from the header without losing BSC's fork identity.
pub(crate) fn evm_env_for_header<Spec>(spec: &Spec, header: &Header) -> BscEvmEnv
where
    Spec: BscHardforks + EthChainSpec + Clone,
{
    let hardfork =
        revm_spec_by_timestamp_and_block_number(spec.clone(), header.timestamp, header.number);
    let blob_params =
        BscHardforks::is_cancun_active_at_timestamp(spec, header.number, header.timestamp)
            .then(|| spec.blob_params_at_timestamp(header.timestamp))
            .flatten();
    let blob_basefee = header
        .excess_blob_gas
        .zip(blob_params)
        .map(|(excess, params)| U256::from(params.calc_blob_fee(excess)))
        .unwrap_or_default();
    let block = BlockEnv::<BscEvmTypes> {
        number: U256::from(header.number),
        beneficiary: header.beneficiary,
        timestamp: U256::from(header.timestamp),
        difficulty: header.difficulty,
        prevrandao: header.difficulty,
        gas_limit: U256::from(header.gas_limit),
        basefee: U256::from(header.base_fee_per_gas.unwrap_or_default()),
        blob_basefee,
        ..Default::default()
    };
    let mut env = BscEvmEnv::new(hardfork, block, spec.chain().id(), millisecond_remainder(header));
    env.header_mix_hash = header.mix_hash;
    if let Some(params) = blob_params {
        env.version.max_blobs_per_tx = params.max_blobs_per_tx as usize;
    }
    env
}

impl ConfigureEvm for BscEvmConfig {
    type Primitives = BscPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = BscNextBlockEnvAttributes;
    type BlockExecutorFactory = BscBlockExecutorFactory;
    type BlockAssembler = BscBlockAssembler<BscChainSpec>;
    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }
    fn block_assembler(&self) -> &Self::BlockAssembler {
        &self.block_assembler
    }
    fn evm_with_env<'a, DB: Database + 'a>(
        &self,
        db: DB,
        env: EvmEnvFor<Self>,
    ) -> EvmFor<'a, Self> {
        self.executor_factory.evm_with_database(db, self.with_block_rules(env))
    }

    fn evm_env(&self, header: &Header) -> Result<BscEvmEnv, Self::Error> {
        Ok(evm_env_for_header(self.chain_spec(), header))
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<BscEvmEnv, Self::Error> {
        let spec = revm_spec_by_timestamp_and_block_number(
            self.chain_spec().clone(),
            attributes.inner.timestamp,
            parent.number + 1,
        );
        let params = self.chain_spec().blob_params_at_timestamp(attributes.inner.timestamp);
        let blob_basefee = next_block_excess_blob_gas_with_mendel(
            self.chain_spec(),
            parent.number + 1,
            attributes.inner.timestamp,
            parent,
            params,
        )
        .map(|excess| U256::from(params.unwrap_or_else(BlobParams::cancun).calc_blob_fee(excess)))
        .unwrap_or_default();
        let block = BlockEnv::<BscEvmTypes> {
            number: U256::from(parent.number + 1),
            beneficiary: attributes.inner.suggested_fee_recipient,
            timestamp: U256::from(attributes.inner.timestamp),
            prevrandao: U256::from_be_bytes(attributes.inner.prev_randao.0),
            difficulty: U256::from_be_bytes(attributes.inner.prev_randao.0),
            gas_limit: U256::from(attributes.inner.gas_limit),
            basefee: U256::from(EIP1559_INITIAL_BASE_FEE),
            blob_basefee,
            ..Default::default()
        };
        let mut env =
            BscEvmEnv::new(spec, block, self.chain_spec().chain().id(), attributes.milli_remainder);
        env.header_difficulty = U256::ZERO;
        if let Some(params) = params {
            env.version.max_blobs_per_tx = params.max_blobs_per_tx as usize;
        }
        Ok(env)
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> Result<ExecutionCtxFor<'a, Self>, Self::Error>
    where
        Self: 'a,
    {
        Ok(BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                tx_count_hint: Some(block.transaction_count()),
                parent_hash: block.header().parent_hash,
                parent_beacon_block_root: block.header().parent_beacon_block_root,
                ommers: &block.body().ommers,
                withdrawals: block.body().withdrawals.as_ref().map(|w| Cow::Borrowed(w.as_slice())),
                extra_data: block.header().extra_data.clone(),
                slot_number: None,
            },
            header: Some(block.header().clone()),
            header_hash: Some(block.hash()),
            mode: BscExecutionMode::Import,
            validator_cache_sink: None,
            turn_length_sink: None,
            state_root_precomputed_sink: None,
            trie_handle: None,
            state_root_deadline_ms: None,
            build_control: None,
            finish_diagnostics: None,
            private_parent: None,
        })
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<HeaderTy<Self::Primitives>>,
        attributes: Self::NextBlockEnvCtx,
    ) -> Result<ExecutionCtxFor<'_, Self>, Self::Error> {
        tracing::trace!(
            "Try to create next block ctx, next_block_numer={}, parent_hash={}, mode={:?}",
            parent.number + 1,
            parent.hash(),
            attributes.mode
        );
        Ok(BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                tx_count_hint: None,
                parent_hash: parent.hash(),
                parent_beacon_block_root: attributes.inner.parent_beacon_block_root,
                ommers: &[],
                withdrawals: attributes.inner.withdrawals.map(|w| Cow::Owned(w.into_inner())),
                extra_data: attributes.inner.extra_data,
                slot_number: attributes.inner.slot_number,
            },
            header: None, // No header available for next block context
            header_hash: None,
            // Carried from the attributes rather than hard-coded: this hook is shared by
            // the miner, `eth_simulateV1` and the local pending-block path.
            mode: attributes.mode,
            validator_cache_sink: attributes.validator_cache_sink,
            turn_length_sink: attributes.turn_length_sink,
            state_root_precomputed_sink: attributes.state_root_precomputed_sink,
            trie_handle: attributes.trie_handle,
            state_root_deadline_ms: attributes.state_root_deadline_ms,
            build_control: attributes.build_control,
            finish_diagnostics: attributes.finish_diagnostics,
            private_parent: attributes.private_parent,
        })
    }

    // payload builder use this method to create BscBlockBuilder.
    fn create_block_builder<'a>(
        &'a self,
        evm: EvmFor<'a, Self>,
        evm_env: BscEvmEnv,
        parent: &'a SealedHeader<HeaderTy<Self::Primitives>>,
        ctx: ExecutionCtxFor<'a, Self>,
    ) -> impl BlockBuilder<Primitives = Self::Primitives, Executor = reth_evm::BlockExecutorFor<'a, Self>>
    where
        Self: 'a,
    {
        // just init a default custom ctx for mining block.
        let shared_ctx = BscExecutionSharedCtx::default();
        let bsc_executor = BscBlockExecutor::new(
            evm,
            ctx.clone(),
            shared_ctx.clone(),
            self.executor_factory.spec().clone(),
            *self.executor_factory.receipt_builder(),
            SystemContract::new(self.executor_factory.spec().clone()),
        );

        BscBlockBuilder::new(bsc_executor, evm_env, ctx, shared_ctx, &self.block_assembler, parent)
    }
}

impl ConfigureEngineEvm<BscExecutionData> for BscEvmConfig
where
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(&self, payload: &BscExecutionData) -> Result<BscEvmEnv, Self::Error> {
        self.evm_env(&payload.block.header)
    }

    fn context_for_payload<'a>(
        &self,
        payload: &'a BscExecutionData,
    ) -> Result<BscBlockExecutionCtx<'a>, Self::Error> {
        let block = &payload.block;
        Ok(BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                tx_count_hint: Some(block.body.inner.transactions.len()),
                parent_hash: block.header.parent_hash(),
                parent_beacon_block_root: block.header.parent_beacon_block_root,
                ommers: &block.body.inner.ommers,
                withdrawals: block
                    .body
                    .inner
                    .withdrawals
                    .as_ref()
                    .map(|w| Cow::Borrowed(w.as_slice())),
                extra_data: block.header.extra_data.clone(),
                slot_number: None,
            },
            header: Some(block.header.clone()),
            header_hash: Some(payload.block_hash_cached()),
            mode: BscExecutionMode::Import,
            validator_cache_sink: None,
            turn_length_sink: None,
            state_root_precomputed_sink: None,
            trie_handle: None,
            state_root_deadline_ms: None,
            build_control: None,
            finish_diagnostics: None,
            private_parent: None,
        })
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &BscExecutionData,
    ) -> Result<impl ExecutableTxIterator<Self>, Self::Error> {
        let txs = payload.block.body.inner.transactions.clone();
        Ok((txs, |tx: TransactionSigned| tx.try_into_recovered()))
    }
}

/// Map the latest active hardfork at the given timestamp or block number to a [`BscHardfork`].
pub fn revm_spec_by_timestamp_and_block_number(
    chain_spec: impl BscHardforks,
    timestamp: u64,
    block_number: u64,
) -> BscHardfork {
    if chain_spec.is_jenner_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Jenner
    } else if chain_spec.is_pasteur_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Pasteur
    } else if chain_spec.is_mendel_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Mendel
    } else if BscHardforks::is_osaka_active_at_timestamp(&chain_spec, block_number, timestamp) {
        BscHardfork::Osaka
    } else if chain_spec.is_fermi_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Fermi
    } else if chain_spec.is_maxwell_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Maxwell
    } else if chain_spec.is_lorentz_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Lorentz
    } else if chain_spec.is_pascal_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Pascal
    } else if chain_spec.is_bohr_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Bohr
    } else if chain_spec.is_haber_fix_active_at_timestamp(block_number, timestamp) {
        BscHardfork::HaberFix
    } else if chain_spec.is_haber_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Haber
    } else if BscHardforks::is_cancun_active_at_timestamp(&chain_spec, block_number, timestamp) {
        BscHardfork::Cancun
    } else if chain_spec.is_feynman_fix_active_at_timestamp(block_number, timestamp) {
        BscHardfork::FeynmanFix
    } else if chain_spec.is_feynman_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Feynman
    } else if chain_spec.is_kepler_active_at_timestamp(block_number, timestamp) {
        BscHardfork::Kepler
    } else if chain_spec.is_hertz_fix_active_at_block(block_number) {
        BscHardfork::HertzFix
    } else if chain_spec.is_hertz_active_at_block(block_number) {
        BscHardfork::Hertz
    } else if chain_spec.is_plato_active_at_block(block_number) {
        BscHardfork::Plato
    } else if chain_spec.is_luban_active_at_block(block_number) {
        BscHardfork::Luban
    } else if chain_spec.is_planck_active_at_block(block_number) {
        BscHardfork::Planck
    } else {
        // Dynamically determine the order for Moran, Nano, Gibbs for the current chain
        fn get_activation_block(fc: &reth_chainspec::ForkCondition) -> Option<u64> {
            match fc {
                reth_chainspec::ForkCondition::Block(b) => Some(*b),
                _ => None,
            }
        }
        let gibbs_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Gibbs));
        let moran_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Moran));
        let nano_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Nano));
        // Sort by activation block descending (newest first)
        let mut forks = vec![
            (gibbs_block, BscHardfork::Gibbs),
            (moran_block, BscHardfork::Moran),
            (nano_block, BscHardfork::Nano),
        ];
        #[allow(clippy::unnecessary_sort_by)]
        forks.sort_by(|a, b| b.0.cmp(&a.0));
        for &(_, fork) in &forks {
            if chain_spec.bsc_fork_activation(fork).active_at_block(block_number) {
                return fork;
            }
        }
        if chain_spec.is_euler_active_at_block(block_number) {
            BscHardfork::Euler
        } else if chain_spec.is_bruno_active_at_block(block_number) {
            BscHardfork::Bruno
        } else if chain_spec.is_mirror_sync_active_at_block(block_number) {
            BscHardfork::MirrorSync
        } else if chain_spec.is_niels_active_at_block(block_number) {
            BscHardfork::Niels
        } else if chain_spec.is_ramanujan_active_at_block(block_number) {
            BscHardfork::Ramanujan
        } else {
            BscHardfork::Frontier
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use reth_primitives_traits::SealedHeader;

    /// `evm_env(header)` must decode the BEP-520 millisecond remainder out of the
    /// header's `mix_hash` tail into [`BscBlockEnv`] — the counterpart of go-bsc
    /// filling `BlockContext.MilliTimestamp` from `header.MilliTimestamp()` in
    /// `NewEVMBlockContext` (the only fill point on the header-derived paths).
    #[test]
    fn evm_env_fills_millisecond_remainder_from_header() {
        let chain_spec = crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet());
        let evm_config = BscEvmConfig::bsc(std::sync::Arc::new(chain_spec));

        // Post-Lorentz header: seconds in `timestamp`, remainder in the mix_hash tail.
        let mut header = Header {
            number: 50_000_000,
            timestamp: 1_780_000_000,
            gas_limit: 30_000_000,
            ..Default::default()
        };
        crate::consensus::parlia::util::set_millisecond_part_of_timestamp(
            1_780_000_000_750,
            &mut header,
        );
        let env = evm_config.evm_env(&header).unwrap();
        assert_eq!(env.milli_remainder, 750);
        assert_eq!(env.milli_timestamp(), 1_780_000_000_750);
        assert_eq!(
            env.milli_timestamp(),
            crate::consensus::parlia::util::calculate_millisecond_timestamp(&header),
            "env and header millisecond sources must agree"
        );

        // Pre-Lorentz-shaped header (zero mix_hash): remainder 0, value Time*1000.
        header.mix_hash = B256::ZERO;
        let env = evm_config.evm_env(&header).unwrap();
        assert_eq!(env.milli_remainder, 0);
        assert_eq!(env.milli_timestamp(), 1_780_000_000_000);

        // Stale-immunity across the generic `inner_mut()` mutation path (block
        // overrides, traceCallMany's bump, callBundle): the live value follows the
        // mutated seconds and keeps the original remainder.
        crate::consensus::parlia::util::set_millisecond_part_of_timestamp(
            1_780_000_000_750,
            &mut header,
        );
        let mut env = evm_config.evm_env(&header).unwrap();

        env.block.timestamp = U256::from(1_790_000_000u64);
        assert_eq!(env.milli_timestamp(), 1_790_000_000_750);
    }

    /// `next_evm_env` (miner / pending / simulate paths) carries a zero remainder until
    /// the miner threads its planned millisecond timestamp through the attributes (R4).
    #[test]
    fn next_evm_env_defaults_to_zero_remainder() {
        let chain_spec = crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet());
        let evm_config = BscEvmConfig::bsc(std::sync::Arc::new(chain_spec));

        let parent = SealedHeader::seal_slow(Header {
            number: 50_000_000,
            timestamp: 1_780_000_000,
            gas_limit: 30_000_000,
            ..Default::default()
        });
        let attrs = <BscNextBlockEnvAttributes as BuildPendingEnv<Header>>::build_pending_env(
            &parent, None,
        );
        let env = evm_config.next_evm_env(&parent, &attrs).unwrap();
        assert_eq!(env.milli_remainder, 0);
        assert_eq!(env.milli_timestamp(), env.block.timestamp.saturating_to::<u64>() * 1000);
    }

    /// The miner threads `block_timestamp_ms % 1000` through the attributes; the env's
    /// live millisecond value must equal both the planned value and what the sealed
    /// header will report via `calculate_millisecond_timestamp` once the same remainder
    /// is written into its `mix_hash` (the two millisecond sources must agree).
    #[test]
    fn next_evm_env_carries_the_miner_millisecond_remainder() {
        let chain_spec = crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet());
        let evm_config = BscEvmConfig::bsc(std::sync::Arc::new(chain_spec));

        let parent = SealedHeader::seal_slow(Header {
            number: 50_000_000,
            timestamp: 1_780_000_000,
            gas_limit: 30_000_000,
            ..Default::default()
        });
        // The planned millisecond timestamp a miner would compute for the next slot.
        let block_timestamp_ms: u64 = 1_780_000_000_750;
        let mut attrs = <BscNextBlockEnvAttributes as BuildPendingEnv<Header>>::build_pending_env(
            &parent, None,
        );
        attrs.inner.timestamp = block_timestamp_ms / 1000;
        attrs.milli_remainder = block_timestamp_ms % 1000;

        let env = evm_config.next_evm_env(&parent, &attrs).unwrap();
        assert_eq!(env.milli_remainder, 750);
        assert_eq!(env.milli_timestamp(), block_timestamp_ms);

        // Sealing the same value into a header (what finalize does via
        // `set_millisecond_part_of_timestamp`) reports the identical milliseconds.
        let mut sealed = Header { timestamp: block_timestamp_ms / 1000, ..Default::default() };
        crate::consensus::parlia::util::set_millisecond_part_of_timestamp(
            block_timestamp_ms,
            &mut sealed,
        );
        assert_eq!(
            env.milli_timestamp(),
            crate::consensus::parlia::util::calculate_millisecond_timestamp(&sealed),
        );
    }

    /// Regression guard for <https://github.com/bnb-chain/reth-bsc/issues/451>.
    ///
    /// `build_pending_env` feeds `eth_simulateV1` and the local pending block, so it must not
    /// select a finalizing mode: that makes a read-only RPC try to sign system transactions —
    /// failing without a validator key, and silently injecting a reward tx with one.
    #[test]
    fn rpc_pending_env_does_not_select_a_finalizing_mode() {
        let parent = SealedHeader::seal_slow(Header::default());
        let attrs = <BscNextBlockEnvAttributes as BuildPendingEnv<Header>>::build_pending_env(
            &parent, None,
        );

        assert_eq!(
            attrs.mode,
            BscExecutionMode::Simulation,
            "a read-only pending env must not select a mode that signs system transactions"
        );
    }

    /// Regression guard for <https://github.com/bnb-chain/reth-bsc/issues/464>.
    ///
    /// `context_for_next_block` must carry the mode from the attributes, never re-decide it:
    /// forcing finalization made `eth_getBlockByNumber("pending")` return `null` on any node
    /// without a validator key.
    #[test]
    fn next_block_ctx_carries_the_mode_from_the_attributes() {
        let chain_spec = Arc::new(BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet()));
        let evm_config = BscEvmConfig::new(chain_spec);
        let parent = SealedHeader::seal_slow(Header::default());

        // The RPC pending-block / `eth_simulateV1` entry point.
        let pending_attrs =
            <BscNextBlockEnvAttributes as BuildPendingEnv<Header>>::build_pending_env(
                &parent, None,
            );
        let ctx = evm_config.context_for_next_block(&parent, pending_attrs).unwrap();
        assert_eq!(
            ctx.mode,
            BscExecutionMode::Simulation,
            "pending-block ctx must not require a validator key to finalize"
        );

        // The miner passes its attributes through the same hook and must keep finalizing.
        let mut mining_attrs =
            <BscNextBlockEnvAttributes as BuildPendingEnv<Header>>::build_pending_env(
                &parent, None,
            );
        mining_attrs.mode = BscExecutionMode::Mining;
        let ctx = evm_config.context_for_next_block(&parent, mining_attrs).unwrap();
        assert_eq!(ctx.mode, BscExecutionMode::Mining, "mining must still run Parlia finalization");
    }
}

#[cfg(test)]
mod sliding_parent_tests {
    use super::*;

    #[test]
    fn private_snapshot_window_releases_old_history() {
        let snapshot = Arc::new(crate::consensus::parlia::Snapshot::default());
        let mut previous: Option<Arc<PrivateMiningParent>> = None;
        for number in 1..=40 {
            previous = Some(Arc::new(PrivateMiningParent {
                header: SealedHeader::new(
                    Header { number, ..Default::default() },
                    alloy_primitives::B256::with_last_byte(number as u8),
                ),
                snapshot: snapshot.clone(),
                ancestor: previous
                    .and_then(|parent| parent.retained_from(number.saturating_sub(2))),
            }));
            let mut current = previous.as_deref();
            let mut count = 0;
            while let Some(parent) = current {
                count += 1;
                current = parent.ancestor.as_deref();
            }
            assert_eq!(count, number.min(3));
        }
        assert!(previous.unwrap().find(&alloy_primitives::B256::with_last_byte(1)).is_none());
    }
}
