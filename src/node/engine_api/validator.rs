use super::payload::BscPayloadTypes;
use crate::{chainspec::BscChainSpec, hardforks::BscHardforks, BscBlock, BscPrimitives};
use alloy_consensus::BlockHeader;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_engine::PayloadError;
use reth::{
    api::{FullNodeComponents, NodeTypes, TreeConfig},
    builder::{
        invalid_block_hook::InvalidBlockHookExt,
        rpc::{
            BasicEngineValidator, ChangesetCache, EngineValidator, EngineValidatorBuilder,
            PayloadValidatorBuilder,
        },
        AddOnsContext,
    },
    consensus::ConsensusError,
};
use reth_chain_state::ExecutedBlock;
use reth_chainspec::EthChainSpec;
use reth_engine_primitives::{ExecutionPayload, InvalidBlockHooks, PayloadValidator};
use reth_engine_tree::tree::{
    error::{InsertBlockErrorKind, InsertPayloadError},
    payload_processor::multiproof::StateRootHandle,
    payload_validator::{TreeCtx, ValidationOutcome},
    CacheWaitDurations, EngineApiTreeState, SavedCache, WaitForCaches,
};
use reth_evm::{ConfigureEngineEvm, ConfigureEvm};
use reth_payload_primitives::{InvalidPayloadAttributesError, NewPayloadError, PayloadTypes};
use reth_primitives_traits::{Block, RecoveredBlock, SealedBlock};
use reth_provider::HeaderProvider;
use reth_trie_common::HashedPostState;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Default, Clone)]
#[non_exhaustive]
pub struct BscPayloadValidatorBuilder;

impl<Node, Types> PayloadValidatorBuilder<Node> for BscPayloadValidatorBuilder
where
    Types:
        NodeTypes<ChainSpec = BscChainSpec, Payload = BscPayloadTypes, Primitives = BscPrimitives>,
    Node: FullNodeComponents<Types = Types>,
{
    type Validator = BscEngineValidator;

    async fn build(self, ctx: &AddOnsContext<'_, Node>) -> eyre::Result<Self::Validator> {
        Ok(BscEngineValidator::new(Arc::new(ctx.config.chain.clone().as_ref().clone())))
    }
}

/// BSC engine validator builder that combines reth's configured invalid-block diagnostics with
/// the BEP-675 cross-validator bad-BidBlock evidence hook.
#[derive(Debug, Default, Clone)]
pub struct BscEngineValidatorBuilder;

impl<Node, Types> EngineValidatorBuilder<Node> for BscEngineValidatorBuilder
where
    Types:
        NodeTypes<ChainSpec = BscChainSpec, Payload = BscPayloadTypes, Primitives = BscPrimitives>,
    Node: FullNodeComponents<Types = Types, Evm: ConfigureEngineEvm<BscExecutionData>>,
{
    type EngineValidator = BscEngineValidatorWithEvidence<Node::Provider, Node::Evm>;

    async fn build_tree_validator(
        self,
        ctx: &AddOnsContext<'_, Node>,
        tree_config: TreeConfig,
        changeset_cache: ChangesetCache,
    ) -> eyre::Result<Self::EngineValidator> {
        let validator = BscPayloadValidatorBuilder.build(ctx).await?;
        let data_dir = ctx.config.datadir.clone().resolve_datadir(ctx.config.chain.chain());
        let configured_hook = ctx.create_invalid_block_hook(&data_dir).await?;
        let evidence_reporter =
            crate::node::miner::bid_block_evidence::BadBidBlockEvidenceReporter::spawn(
                ctx.node.provider().clone(),
                ctx.config.chain.clone(),
                ctx.node.task_executor(),
            );
        // Enqueue evidence first; the configured witness hook may synchronously re-execute the
        // invalid block and should not delay cross-validator propagation.
        let invalid_block_hook =
            InvalidBlockHooks(vec![Box::new(evidence_reporter.clone()), configured_hook]);

        let inner = BasicEngineValidator::new(
            ctx.node.provider().clone(),
            Arc::new(ctx.node.consensus().clone()),
            ctx.node.evm_config().clone(),
            validator,
            tree_config,
            Box::new(invalid_block_hook),
            changeset_cache,
            ctx.node.task_executor().clone(),
        );

        Ok(BscEngineValidatorWithEvidence {
            inner,
            provider: ctx.node.provider().clone(),
            evidence_reporter,
        })
    }
}

/// Adds evidence reporting for execution errors that occur before an execution output exists.
///
/// Reth's invalid-block hook covers state-root and post-execution failures. Transaction execution
/// errors return before that hook can run, so this wrapper observes the final validation outcome.
/// The inner validator checks the header and body; typed Parlia pre-execution errors are excluded
/// because they do not establish that the claimed sealer authenticated the block.
pub struct BscEngineValidatorWithEvidence<P, Evm: ConfigureEvm> {
    inner: BasicEngineValidator<P, Evm, BscEngineValidator>,
    provider: P,
    evidence_reporter: crate::node::miner::bid_block_evidence::BadBidBlockEvidenceReporter,
}

impl<P, Evm> BscEngineValidatorWithEvidence<P, Evm>
where
    P: HeaderProvider<Header = alloy_consensus::Header>,
    Evm: ConfigureEvm,
{
    fn report_execution_error(&self, outcome: &ValidationOutcome<BscPrimitives>) {
        let Err(InsertPayloadError::Block(error)) = outcome else { return };
        let InsertBlockErrorKind::Execution(execution_error) = error.kind() else { return };
        if !crate::node::evm::error::is_execution_evidence(execution_error) {
            return;
        }

        let block = error.block();
        match self.provider.sealed_header_by_hash(block.parent_hash()) {
            Ok(Some(parent)) => self.evidence_reporter.report(&parent, block),
            Ok(None) => tracing::debug!(
                target: "bsc::bid_block_evidence",
                parent_hash = %block.parent_hash(),
                block_hash = %block.hash(),
                "Parent header unavailable for bad BidBlock evidence"
            ),
            Err(err) => tracing::warn!(
                target: "bsc::bid_block_evidence",
                parent_hash = %block.parent_hash(),
                block_hash = %block.hash(),
                %err,
                "Failed to load parent header for bad BidBlock evidence"
            ),
        }
    }
}

impl<P, Evm> EngineValidator<BscPayloadTypes, BscPrimitives>
    for BscEngineValidatorWithEvidence<P, Evm>
where
    BasicEngineValidator<P, Evm, BscEngineValidator>:
        EngineValidator<BscPayloadTypes, BscPrimitives>,
    P: HeaderProvider<Header = alloy_consensus::Header> + Send + Sync + 'static,
    Evm: ConfigureEvm + Send + Sync + 'static,
{
    fn validate_payload_attributes_against_header(
        &self,
        attr: &<BscPayloadTypes as PayloadTypes>::PayloadAttributes,
        header: &alloy_consensus::Header,
    ) -> Result<(), InvalidPayloadAttributesError> {
        self.inner.validate_payload_attributes_against_header(attr, header)
    }

    fn convert_payload_to_block(
        &self,
        payload: BscExecutionData,
    ) -> Result<SealedBlock<BscBlock>, NewPayloadError> {
        self.inner.convert_payload_to_block(payload)
    }

    fn validate_payload(
        &mut self,
        payload: BscExecutionData,
        ctx: TreeCtx<'_, BscPrimitives>,
    ) -> ValidationOutcome<BscPrimitives> {
        let outcome = self.inner.validate_payload(payload, ctx);
        self.report_execution_error(&outcome);
        outcome
    }

    fn validate_block(
        &mut self,
        block: SealedBlock<BscBlock>,
        ctx: TreeCtx<'_, BscPrimitives>,
    ) -> ValidationOutcome<BscPrimitives> {
        let outcome = self.inner.validate_block(block, ctx);
        self.report_execution_error(&outcome);
        outcome
    }

    fn on_inserted_executed_block(&self, block: ExecutedBlock<BscPrimitives>) {
        self.inner.on_inserted_executed_block(block)
    }

    fn cache_for(&self, block_hash: B256) -> Option<SavedCache> {
        self.inner.cache_for(block_hash)
    }

    fn sparse_trie_handle_for(
        &self,
        parent_hash: B256,
        parent_state_root: B256,
        state: &EngineApiTreeState<BscPrimitives>,
    ) -> Option<StateRootHandle> {
        self.inner.sparse_trie_handle_for(parent_hash, parent_state_root, state)
    }
}

impl<P, Evm> WaitForCaches for BscEngineValidatorWithEvidence<P, Evm>
where
    BasicEngineValidator<P, Evm, BscEngineValidator>: WaitForCaches,
    Evm: ConfigureEvm,
{
    fn wait_for_caches(&self) -> CacheWaitDurations {
        self.inner.wait_for_caches()
    }
}

/// Validator for Optimism engine API.
#[derive(Debug, Clone)]
pub struct BscEngineValidator {
    inner: BscExecutionPayloadValidator<BscChainSpec>,
}

impl BscEngineValidator {
    /// Instantiates a new validator.
    pub fn new(chain_spec: Arc<BscChainSpec>) -> Self {
        Self { inner: BscExecutionPayloadValidator { inner: chain_spec } }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BscExecutionData {
    #[serde(flatten)]
    pub block: BscBlock,
    #[serde(skip, default)]
    hash: OnceLock<B256>,
}

impl BscExecutionData {
    pub fn new(block: BscBlock) -> Self {
        Self { block, hash: OnceLock::new() }
    }

    /// Seeds the hash cache from a trusted sealed-block source.
    pub(crate) fn new_with_hash(block: BscBlock, hash: B256) -> Self {
        let lock = OnceLock::new();
        let _ = lock.set(hash);
        Self { block, hash: lock }
    }

    pub fn block_hash_cached(&self) -> B256 {
        *self.hash.get_or_init(|| self.block.header.hash_slow())
    }

    pub(crate) fn cached_hash(&self) -> Option<B256> {
        self.hash.get().copied()
    }

    pub fn into_block(self) -> BscBlock {
        self.block
    }
}

impl From<BscBlock> for BscExecutionData {
    fn from(block: BscBlock) -> Self {
        Self::new(block)
    }
}

impl Default for BscExecutionData {
    fn default() -> Self {
        Self::new(BscBlock::default())
    }
}

impl Clone for BscExecutionData {
    fn clone(&self) -> Self {
        let hash = OnceLock::new();
        if let Some(value) = self.hash.get() {
            let _ = hash.set(*value);
        }
        Self { block: self.block.clone(), hash }
    }
}

impl ExecutionPayload for BscExecutionData {
    fn parent_hash(&self) -> B256 {
        self.block.header.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.block_hash_cached()
    }

    fn block_number(&self) -> u64 {
        self.block.header.number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        None
    }

    fn block_access_list(&self) -> Option<&Bytes> {
        None
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        None
    }

    fn timestamp(&self) -> u64 {
        self.block.header.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.block.header.gas_used()
    }

    fn gas_limit(&self) -> u64 {
        self.block.header.gas_limit()
    }

    fn slot_number(&self) -> Option<u64> {
        None
    }

    fn transaction_count(&self) -> usize {
        self.block.body.inner.transactions.len()
    }
}

impl PayloadValidator<BscPayloadTypes> for BscEngineValidator {
    type Block = BscBlock;

    fn convert_payload_to_block(
        &self,
        payload: BscExecutionData,
    ) -> Result<SealedBlock<Self::Block>, NewPayloadError> {
        self.inner.ensure_well_formed_payload(payload).map_err(NewPayloadError::other)
    }

    fn ensure_well_formed_payload(
        &self,
        payload: BscExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed_block = self.convert_payload_to_block(payload)?;
        sealed_block.try_recover().map_err(|e| NewPayloadError::Other(e.into()))
    }

    fn validate_block_post_execution_with_hashed_state(
        &self,
        _state_updates: &HashedPostState,
        _block: &RecoveredBlock<Self::Block>,
    ) -> Result<(), ConsensusError> {
        Ok(())
    }
}

/// Execution payload validator.
#[derive(Clone, Debug)]
pub struct BscExecutionPayloadValidator<ChainSpec> {
    /// Chain spec to validate against.
    #[allow(unused)]
    inner: Arc<ChainSpec>,
}

impl<ChainSpec> BscExecutionPayloadValidator<ChainSpec>
where
    ChainSpec: BscHardforks,
{
    pub fn ensure_well_formed_payload(
        &self,
        payload: BscExecutionData,
    ) -> Result<SealedBlock<BscBlock>, PayloadError> {
        let header_hash = if let Some(cached_hash) = payload.cached_hash() {
            // Cached hashes are seeded only from trusted internal sealed-block producers.
            // Keep a debug assertion here to catch any future misuse without paying the
            // recomputation cost on the hot path in release builds.
            #[cfg(debug_assertions)]
            {
                let computed_hash = payload.block.header.hash_slow();
                if cached_hash != computed_hash {
                    return Err(PayloadError::BlockHash {
                        execution: computed_hash,
                        consensus: cached_hash,
                    })?
                }
            }
            cached_hash
        } else {
            payload.block.header.hash_slow()
        };

        let block = payload.into_block();
        Ok(block.seal_unchecked(header_hash))
    }
}
