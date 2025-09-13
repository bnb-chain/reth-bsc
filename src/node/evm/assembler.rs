use crate::{
    node::evm::config::{BscBlockExecutorFactory, BscEvmConfig, BscBlockExecutionCtx},
    chainspec::BscChainSpec,
    consensus::parlia::{SnapshotProvider, Parlia},
    hardforks::BscHardforks,
    BscBlock, BscBlockBody,
};
use alloy_consensus::{Block, BlockBody, Header, EMPTY_OMMER_ROOT_HASH, proofs, Transaction, BlockHeader};
use alloy_eips::{eip7840::BlobParams, merge::BEACON_NONCE};
use alloy_primitives::Bytes;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth_ethereum_primitives::{Receipt, TransactionSigned};
use reth_evm::{
    block::{BlockExecutionError, BlockExecutorFactory},
    execute::{BlockAssembler, BlockAssemblerInput},
};
use reth_primitives_traits::logs_bloom;
use reth_provider::BlockExecutionResult;
use std::sync::Arc;

/// Block assembler for BSC, mainly for support BscBlockExecutionCtx.
#[derive(Clone)]
pub struct BscBlockAssembler<ChainSpec = BscChainSpec> {
    /// The chainspec.
    pub chain_spec: Arc<ChainSpec>,
    /// Extra data to use for the blocks.
    pub extra_data: Bytes,
    /// Snapshot provider for accessing Parlia validator snapshots.
    pub(super) snapshot_provider: Option<Arc<dyn SnapshotProvider + Send + Sync>>,
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
            snapshot_provider: crate::shared::get_snapshot_provider().cloned(),
            parlia: Arc::new(Parlia::new(chain_spec, 200)),
        }
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
    type Block = Block<TransactionSigned>;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, F>,
    ) -> Result<Block<TransactionSigned>, BlockExecutionError> {
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

        let withdrawals = self
            .chain_spec
            .is_shanghai_active_at_timestamp(timestamp)
            .then(|| eth_ctx.withdrawals.clone().map(|w| w.into_owned()).unwrap_or_default());

        let withdrawals_root =
            withdrawals.as_deref().map(|w| proofs::calculate_withdrawals_root(w));
        let requests_hash = self
            .chain_spec
            .is_prague_active_at_timestamp(timestamp)
            .then(|| requests.requests_hash());

        let mut excess_blob_gas = None;
        let mut blob_gas_used = None;

        let block_number = evm_env.block_env.number.saturating_to();
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
                Some(BlobParams::cancun().next_block_excess_blob_gas(0, 0))
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
                .get_header_by_number(header.number - 1)
                .ok_or(BlockExecutionError::msg("Failed to get header from global header reader"))?;
            let parent_snap = self
                .snapshot_provider
                .as_ref()
                .unwrap()
                .snapshot(header.number - 1)
                .ok_or(BlockExecutionError::msg("Failed to get snapshot from snapshot provider"))?;
            if let Err(e) = crate::node::miner::util::finalize_new_header(
                self.parlia.clone(), 
                &parent_snap, 
                &parent_header, 
                ctx.turn_length,
                &mut header
            ) {
                tracing::warn!("Failed to finalize header: {}", e);
            }
        }

        Ok(Block {
            header,
            body: BlockBody { transactions, ommers: Default::default(), withdrawals },
        })
    }
}

impl BlockAssembler<BscBlockExecutorFactory> for BscEvmConfig {
    type Block = BscBlock;

    fn assemble_block(
        &self,
        input: BlockAssemblerInput<'_, '_, BscBlockExecutorFactory, Header>,
    ) -> Result<Self::Block, BlockExecutionError> {
        let Block { header, body: inner } = self.block_assembler.assemble_block(input)?;
        Ok(BscBlock {
            header,
            body: BscBlockBody {
                inner,
                // HACK: we're setting sidecars to `None` here but ideally we should somehow get
                // them from the payload builder.
                //
                // Payload building is out of scope of reth-bsc for now, so this is not critical
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
            .field("snapshot_provider", &self.snapshot_provider.is_some())
            .field("parlia", &"Arc<Parlia<ChainSpec>>")
            .finish()
    }
}
