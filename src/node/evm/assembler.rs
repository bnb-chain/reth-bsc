use crate::{
    chainspec::BscChainSpec, 
    consensus::parlia::{Parlia, EMPTY_REQUESTS_HASH, EMPTY_WITHDRAWALS_HASH}, 
    hardforks::BscHardforks, 
    node::{
        evm::config::{BscBlockExecutionCtx, BscBlockExecutorFactory},
        miner::util::finalize_new_header,
        primitives::{BscBlock, BscBlockBody},
    }
};
use alloy_consensus::{BlockBody, Header, EMPTY_OMMER_ROOT_HASH, proofs, Transaction, BlockHeader};
use alloy_primitives::{keccak256, B256};
use alloy_eips::{eip7840::BlobParams, merge::BEACON_NONCE};
use alloy_primitives::Bytes;
use alloy_rpc_types::Withdrawals;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput},
    EvmEnv,
};
use reth_primitives_traits::{logs_bloom, SealedHeader};
use reth_provider::{BlockExecutionResult, StateProvider};
use revm::database::BundleState;
use std::sync::Arc;


/// BSC block assembler input that mirrors BlockAssemblerInput but is not #[non_exhaustive]
/// 
/// This allows us to construct the input in external crates without being limited by
/// the #[non_exhaustive] attribute on the original BlockAssemblerInput.
pub struct BscBlockAssemblerInput<'a, 'b, F: BlockExecutorFactory, H = Header> {
    /// Configuration of EVM used when executing the block.
    pub evm_env: EvmEnv<<F::EvmFactory as reth_evm::EvmFactory>::Spec>,
    /// BlockExecutorFactory::ExecutionCtx used to execute the block.
    pub execution_ctx: F::ExecutionCtx<'a>,
    /// Parent block header.
    pub parent: &'a SealedHeader<H>,
    /// Transactions that were executed in this block.
    pub transactions: Vec<F::Transaction>,
    /// Output of block execution.
    pub output: &'b BlockExecutionResult<F::Receipt>,
    /// BundleState after the block execution.
    pub bundle_state: &'a BundleState,
    /// Provider with access to state.
    pub state_provider: &'b dyn StateProvider,
    /// State root for this block.
    pub state_root: alloy_primitives::B256,
}

/// Block assembler for BSC, mainly for support BscBlockExecutionCtx.
#[derive(Clone)]
pub struct BscBlockAssembler<ChainSpec = BscChainSpec> {
    /// The chainspec.
    pub chain_spec: Arc<ChainSpec>,
    /// Extra data to use for the blocks.
    pub extra_data: Bytes,
    /// Parlia consensus instance.
    pub(crate) parlia: Arc<Parlia<ChainSpec>>,
}

impl<ChainSpec> BscBlockAssembler<ChainSpec> 
where
    ChainSpec: EthChainSpec + BscHardforks + 'static,
{
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { 
            chain_spec: chain_spec.clone(), 
            extra_data: Default::default(),  
            parlia: Arc::new(Parlia::new(chain_spec, 200)),
        }
    }

    /// BSC-specific assemble_block method that accepts BscBlockAssemblerInput.
    /// This method is completely aligned with the standard assemble_block implementation.
    pub fn assemble_block_bsc(&self, input: BscBlockAssemblerInput<'_, '_, BscBlockExecutorFactory>) -> 
        Result<crate::node::primitives::BscBlock, BlockExecutionError>
    {
        // Get snapshot provider, return error if not available
        let snapshot_provider = crate::shared::get_snapshot_provider()
            .cloned()
            .ok_or_else(|| BlockExecutionError::msg("Snapshot provider not available"))?;

        let BscBlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            parent,
            transactions,
            output: BlockExecutionResult { receipts, requests: _, gas_used },
            state_root,
            ..
        } = input;

        // Use the base EthBlockExecutionCtx for compatibility
        let eth_ctx = ctx.as_eth_context();
        let timestamp = evm_env.block_env.timestamp.saturating_to();
        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = Receipt::calculate_receipt_root_no_memo(receipts);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| &r.logs));
        let block_number = evm_env.block_env.number.saturating_to();

        // parlia override header un-used fields.
        let mut withdrawals_root = None;
        let mut parent_beacon_block_root = None;
        let mut requests_hash = None;
        if BscHardforks::is_cancun_active_at_timestamp(self.chain_spec.as_ref(), block_number, timestamp) {
            withdrawals_root = Some(EMPTY_WITHDRAWALS_HASH);
            if self.chain_spec.is_bohr_active_at_timestamp(block_number, timestamp) {
                parent_beacon_block_root = Some(B256::default());
            }
            if self.chain_spec.is_prague_active_at_block_and_timestamp(block_number, timestamp) {
                requests_hash = Some(EMPTY_REQUESTS_HASH);
            }
        }

        let mut excess_blob_gas = None;
        let mut blob_gas_used = None;

        if BscHardforks::is_cancun_active_at_timestamp(self.chain_spec.as_ref(), block_number, timestamp) {
            blob_gas_used =
                Some(transactions.iter().map(|tx| tx.blob_gas_used().unwrap_or_default()).sum());
            excess_blob_gas = if BscHardforks::is_cancun_active_at_timestamp(self.chain_spec.as_ref(), parent.number, parent.timestamp) {
                parent.maybe_next_block_excess_blob_gas(
                    self.chain_spec.blob_params_at_timestamp(timestamp),
                )
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(BlobParams::cancun().next_block_excess_blob_gas_osaka(0, 0, 0))
            };
        }

        let mut header = Header {
            parent_hash: eth_ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block_env.prevrandao.unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee),
            number: evm_env.block_env.number.saturating_to(),
            gas_limit: evm_env.block_env.gas_limit,
            difficulty: evm_env.block_env.difficulty,
            gas_used: *gas_used,
            extra_data: self.extra_data.clone(),
            parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
        };
        
        {   // finalize_new_header
            let parent_header = crate::node::evm::util::HEADER_CACHE_READER
                .lock()
                .unwrap()
                .get_header_by_hash(&header.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get header from global header reader"))?;
            let parent_snap = snapshot_provider
                .snapshot_by_hash(&header.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
            finalize_new_header(
                self.parlia.clone(), 
                &parent_snap, 
                &parent_header, 
                &mut header
            ).map_err(|e| BlockExecutionError::msg(format!("Failed to finalize header: {}", e)))?;

            let header_hash = keccak256(alloy_rlp::encode(&header));
            tracing::debug!("Succeed to finalize header, block_number={}, hash=0x{:x}, parent_hash=0x{:x}, txs={}", 
                header.number, header_hash, header.parent_hash, transactions.len())
        }

        Ok(BscBlock {
            header,
            body: BscBlockBody {
                inner: BlockBody { transactions, ommers: Default::default(), withdrawals: Some(Withdrawals::new(vec![])) },
                sidecars: None, // BscSidecars is added to the block body in the payload builder.
            },
        })
    }

}

impl<F, ChainSpec> BlockAssembler<F> for BscBlockAssembler<ChainSpec>
where
    F: for<'a> BlockExecutorFactory<
        ExecutionCtx<'a> = BscBlockExecutionCtx<'a>,
        Transaction = TransactionSigned,
        Receipt = Receipt,
    >,
    ChainSpec: EthChainSpec + EthereumHardforks + crate::hardforks::BscHardforks + 'static,
{
    type Block = crate::node::primitives::BscBlock;

    // note that assemble_block is unused, BscBlockBuiler use assemble_block_bsc instead.
    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<crate::node::primitives::BscBlock, BlockExecutionError> {
        // Get snapshot provider, return error if not available
        let snapshot_provider = crate::shared::get_snapshot_provider()
            .cloned()
            .ok_or_else(|| BlockExecutionError::msg("Snapshot provider not available"))?;

        let BlockAssemblerInput {
            evm_env,
            execution_ctx: ctx,
            parent,
            transactions,
            output: BlockExecutionResult { receipts, requests, gas_used },
            state_root,
            ..
        } = input;

        // Use the base EthBlockExecutionCtx for compatibility
        let eth_ctx = ctx.as_eth_context();
        let timestamp = evm_env.block_env.timestamp.saturating_to();
        let transactions_root = proofs::calculate_transaction_root(&transactions);
        let receipts_root = Receipt::calculate_receipt_root_no_memo(receipts);
        let logs_bloom = logs_bloom(receipts.iter().flat_map(|r| &r.logs));
        let block_number = evm_env.block_env.number.saturating_to();

        let withdrawals = self
            .chain_spec
            .is_shanghai_active_at_timestamp(timestamp)
            .then(|| eth_ctx.withdrawals.clone().map(|w| w.into_owned()).unwrap_or_default());

        let withdrawals_root =
            withdrawals.as_deref().map(|w| proofs::calculate_withdrawals_root(w));
        let requests_hash = self.chain_spec.is_prague_active_at_block_and_timestamp(block_number, timestamp)
            .then(|| requests.requests_hash());

        let mut excess_blob_gas = None;
        let mut blob_gas_used = None;

        
        if BscHardforks::is_cancun_active_at_timestamp(&*self.chain_spec, block_number, timestamp) {
            blob_gas_used =
                Some(transactions.iter().map(|tx| tx.blob_gas_used().unwrap_or_default()).sum());
            excess_blob_gas = if BscHardforks::is_cancun_active_at_timestamp(&*self.chain_spec, parent.number, parent.timestamp) {
                parent.maybe_next_block_excess_blob_gas(
                    self.chain_spec.blob_params_at_timestamp(timestamp),
                )
            } else {
                // for the first post-fork block, both parent.blob_gas_used and
                // parent.excess_blob_gas are evaluated as 0
                Some(BlobParams::cancun().next_block_excess_blob_gas_osaka(0, 0, 0))
            };
        }

        let mut header = Header {
            parent_hash: eth_ctx.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: evm_env.block_env.beneficiary,
            state_root,
            transactions_root,
            receipts_root,
            withdrawals_root,
            logs_bloom,
            timestamp,
            mix_hash: evm_env.block_env.prevrandao.unwrap_or_default(),
            nonce: BEACON_NONCE.into(),
            base_fee_per_gas: Some(evm_env.block_env.basefee),
            number: evm_env.block_env.number.saturating_to(),
            gas_limit: evm_env.block_env.gas_limit,
            difficulty: evm_env.block_env.difficulty,
            gas_used: *gas_used,
            extra_data: self.extra_data.clone(),
            parent_beacon_block_root: eth_ctx.parent_beacon_block_root,
            blob_gas_used,
            excess_blob_gas,
            requests_hash,
        };
        
        {   // finalize_new_header
            let parent_header = crate::node::evm::util::HEADER_CACHE_READER
                .lock()
                .unwrap()
                .get_header_by_hash(&header.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get header from global header reader"))?;
            let parent_snap = snapshot_provider
                .snapshot_by_hash(&header.parent_hash)
                .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
            finalize_new_header(
                self.parlia.clone(), 
                &parent_snap, 
                &parent_header, 
                &mut header
            ).map_err(|e| BlockExecutionError::msg(format!("Failed to finalize header: {}", e)))?;

            let header_hash = keccak256(alloy_rlp::encode(&header));
            tracing::debug!("Succeed to finalize header, block_number={}, hash=0x{:x}, parent_hash=0x{:x}, txs={}", 
                header.number, header_hash, header.parent_hash, transactions.len())
        }

        Ok(BscBlock {
            header,
            body: BscBlockBody {
                inner: BlockBody { transactions, ommers: Default::default(), withdrawals },
                sidecars: None,
            },
        })
    }
}

impl<ChainSpec> std::fmt::Debug for BscBlockAssembler<ChainSpec> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BscBlockAssembler")
            .field("chain_spec", &"Arc<ChainSpec>")
            .field("extra_data", &self.extra_data)
            .field("parlia", &"Arc<Parlia<ChainSpec>>")
            .finish()
    }
}
