use super::payload::BscPayloadTypes;
use crate::{chainspec::BscChainSpec, hardforks::BscHardforks, BscBlock, BscPrimitives};
use alloy_consensus::BlockHeader;
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::B256;
use alloy_rpc_types_engine::PayloadError;
use reth::{
    api::{FullNodeComponents, NodeTypes},
    builder::{
        rpc::{BasicEngineValidatorBuilder, PayloadValidatorBuilder},
        AddOnsContext,
    },
    consensus::ConsensusError,
};
use reth_engine_primitives::{ExecutionPayload, PayloadValidator};
use reth_payload_primitives::NewPayloadError;
use reth_primitives::{RecoveredBlock, SealedBlock};
use reth_trie_common::HashedPostState;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use reth_primitives_traits::Block;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BscExecutionData(pub BscBlock);

impl ExecutionPayload for BscExecutionData {
    fn parent_hash(&self) -> B256 {
        self.0.header.parent_hash()
    }

    fn block_hash(&self) -> B256 {
        self.0.header.hash_slow()
    }

    fn block_number(&self) -> u64 {
        self.0.header.number()
    }

    fn withdrawals(&self) -> Option<&Vec<Withdrawal>> {
        None
    }

    fn parent_beacon_block_root(&self) -> Option<B256> {
        None
    }

    fn timestamp(&self) -> u64 {
        self.0.header.timestamp()
    }

    fn gas_used(&self) -> u64 {
        self.0.header.gas_used()
    }
}

impl PayloadValidator<BscPayloadTypes> for BscEngineValidator {
    type Block = BscBlock;

    fn ensure_well_formed_payload(
        &self,
        payload: BscExecutionData,
    ) -> Result<RecoveredBlock<Self::Block>, NewPayloadError> {
        let sealed_block =
            self.inner.ensure_well_formed_payload(payload).map_err(NewPayloadError::other)?;
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
        let block = payload.0;
        let expected_hash = block.header.hash_slow();
        let sealed_block = block.seal_slow();

        // Ensure the hash included in the payload matches the block hash
        if expected_hash != sealed_block.hash() {
            return Err(PayloadError::BlockHash {
                execution: sealed_block.hash(),
                consensus: expected_hash,
            })?
        }

        Ok(sealed_block)
    }
}
