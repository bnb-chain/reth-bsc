use alloy_primitives::U256;
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
use reth_provider::StateProviderFactory;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_evm::execute::BlockBuilder;
use alloy_evm::Evm;
use reth_payload_primitives::PayloadBuilderError;
use reth::transaction_pool::{TransactionPool, PoolTransaction};
use reth_primitives::TransactionSigned;
use reth::transaction_pool::BestTransactionsAttributes;
use tracing::trace;
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth_primitives::InvalidTransactionError;
use reth_evm::execute::BlockBuilderOutcome;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use std::sync::Arc;
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth::payload::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use alloy_consensus::Transaction;
use tracing::warn;

/// BSC payload builder, used to build payload for bsc miner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscPayloadBuilder<Pool, Client, EvmConfig = BscEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    /// Payload builder configuration, now reuse eth builder config.
    builder_config: EthereumBuilderConfig,
    // todo: aborted build task by new header.
}

impl<Pool, Client, EvmConfig> BscPayloadBuilder<Pool, Client, EvmConfig> 
where
    Client: StateProviderFactory,
    EvmConfig: ConfigureEvm<NextBlockEnvCtx = NextBlockEnvAttributes>,
    <EvmConfig as ConfigureEvm>::Primitives: reth_primitives_traits::NodePrimitives<BlockHeader = alloy_consensus::Header, SignedTx = alloy_consensus::EthereumTxEnvelope<alloy_consensus::TxEip4844>, Block = crate::node::primitives::BscBlock>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
{
    pub const fn new(
        client: Client,
        pool: Pool,
        evm_config: EvmConfig,
        builder_config: EthereumBuilderConfig,
    ) -> Self {
        Self { client, pool, evm_config, builder_config }
    }

    pub fn build_payload(self,args: BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let BuildArguments { mut cached_reads, config, cancel: _cancel, best_payload: _best_payload } = args;
        let PayloadConfig { parent_header, attributes } = config;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(cached_reads.as_db_mut(state)).build();
        
        let mut builder = self.evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                NextBlockEnvAttributes {
                    timestamp: attributes.timestamp(),
                    suggested_fee_recipient: attributes.suggested_fee_recipient(),
                    prev_randao: attributes.prev_randao(),
                    gas_limit: self.builder_config.gas_limit(parent_header.gas_limit),
                    parent_beacon_block_root: attributes.parent_beacon_block_root(),
                    withdrawals: Some(attributes.withdrawals().clone()),
                },
            )
            .map_err(PayloadBuilderError::other)?;

        // check: rewrite in here.
        builder.apply_pre_execution_changes().map_err(|err| {
            warn!(target: "payload_builder", %err, "failed to apply pre-execution changes");
            PayloadBuilderError::Internal(err.into())
        })?;

        let mut total_fees = U256::ZERO;
        let mut cumulative_gas_used = 0;
        let block_gas_limit: u64 = builder.evm_mut().block().gas_limit;

        let base_fee = builder.evm_mut().block().basefee;
        // check: now only filter out blob tx by none blob fee for simple test.
        let mut best_tx_list = self.pool.best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, None));
        while let Some(pool_tx) = best_tx_list.next() {
            // ensure we still have capacity for this transaction
            if cumulative_gas_used + pool_tx.gas_limit() > block_gas_limit {
                // we can't fit this transaction into the block, so we need to mark it as invalid
                // which also removes all dependent transaction from the iterator before we can
                // continue
                best_tx_list.mark_invalid(
                    &pool_tx,
                    InvalidPoolTransactionError::ExceedsGasLimit(pool_tx.gas_limit(), block_gas_limit),
                );
                continue
            }

            let tx = pool_tx.to_consensus();
            if let Some(_blob_tx) = tx.as_eip4844() {
                // todo: skip blob tx for simple test.
                continue
            }
            
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error, ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        trace!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        trace!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
                        best_tx_list.mark_invalid(
                            &pool_tx,
                            InvalidPoolTransactionError::Consensus(
                                InvalidTransactionError::TxTypeNotSupported,
                            ),
                        );
                    }
                    continue
                }
                // this is an error that we should treat as fatal for this attempt
                Err(err) => return Err(Box::new(PayloadBuilderError::evm(err))),
            };
            // update and add to total fees
            let miner_fee = tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
            total_fees += U256::from(miner_fee) * U256::from(gas_used);
            cumulative_gas_used += gas_used;
        }

        // add system txs to payload, need to rewrite finish.
        // check: rewrite in here.
        let BlockBuilderOutcome { execution_result, block, .. } = builder.finish(&state_provider)?;
        let sealed_block = Arc::new(block.sealed_block().clone());
        let payload = BscBuiltPayload {
            block: sealed_block,
            fees: total_fees,
            requests: Some(execution_result.requests),
        };
        Ok(payload)
    }
}
