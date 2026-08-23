use super::payload::BscPayloadTypes;
use crate::{chainspec::BscChainSpec, hardforks::BscHardforks, BscBlock, BscPrimitives};
use alloy_consensus::BlockHeader;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Bytes, B256};
use alloy_rpc_types_engine::PayloadError;
use reth::{
    api::{FullNodeComponents, NodeTypes},
    builder::{
        rpc::{BasicEngineValidatorBuilder, PayloadValidatorBuilder},
        AddOnsContext,
    },
};
use reth_engine_primitives::{ExecutionPayload, PayloadValidator};
use reth_payload_primitives::NewPayloadError;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_primitives_traits::Block;
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

/// BSC engine validator builder that wraps the payload validator
pub type BscEngineValidatorBuilder = BasicEngineValidatorBuilder<BscPayloadValidatorBuilder>;

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

impl From<crate::node::engine::BscBuiltPayload> for BscExecutionData {
    fn from(payload: crate::node::engine::BscBuiltPayload) -> Self {
        let hash = payload.block.hash();
        Self::new_with_hash(payload.block.clone_block(), hash)
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
