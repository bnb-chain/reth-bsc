
use alloy_consensus::Header;
use alloy_primitives::{Address, U256};
use crate::node::engine::BscBuiltPayload;
use crate::node::evm::config::BscEvmConfig;
use reth_provider::StateProviderFactory;
use reth_revm::{database::StateProviderDatabase, db::State};
use reth_revm::cached::CachedReads;
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
use std::sync::Arc;

/// BSC payload builder, used to build payload for bsc miner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BscPayloadBuilder<Pool, Client, EvmConfig = BscEvmConfig> {
    /// Client providing access to node state.
    client: Client,
    /// Transaction pool.
    pool: Pool,
    /// The type responsible for creating the evm.
    evm_config: EvmConfig,
    // builder_config: EthereumBuilderConfig,
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
        //builder_config: EthereumBuilderConfig,
    ) -> Self {
        Self { client, pool, evm_config }
    }

    pub fn build_payload(&self, parent: &Header) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        // 1.prepare header field by parlia, such as timestamp, difficulty etc.
        // 2.apply change before execute, maybe need upgrade system contract.
        // 3.fetch tx-list from tx pool
        // 4.simulate tx execute
        // 5.assemble system txs by parlia
        // 6.seal block by parlia
        // 7.queue to engine-api for memory tree and broadcast it block_import channel(maybe in here)


        let state_provider = self.client.state_by_block_hash(parent.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut cached_reads = CachedReads::default();
        let mut db = State::builder().with_database(cached_reads.as_db_mut(state)).build();

        // Convert Header to SealedHeader
        // todo: remove it later.
        let sealed_parent = reth_primitives::SealedHeader::new(
            parent.clone(),
            parent.hash_slow(),
        );
        
        let mut builder = self.evm_config
        .builder_for_next_block(
            &mut db,
            &sealed_parent,
            NextBlockEnvAttributes {
                timestamp: parent.timestamp + 1,
                suggested_fee_recipient: Address::ZERO,
                prev_randao: Default::default(),
                gas_limit: 30_000_000,
                parent_beacon_block_root: None,
                withdrawals: None,
            },
        )
        .map_err(PayloadBuilderError::other)?;

        let mut best_tx_list = self.pool.best_transactions_with_attributes(BestTransactionsAttributes::new(0, None));

        let _cumulative_gas_used = 0;
        let _block_gas_limit: u64 = builder.evm_mut().block().gas_limit;
        let _base_fee = builder.evm_mut().block().basefee;
        let _total_fees = U256::ZERO;

        while let Some(pool_tx) = best_tx_list.next() {
            // todo: skip blob tx first.
            // convert tx to a signed transaction
            let tx = pool_tx.transaction.clone().into_consensus();
            let _gas_used = match builder.execute_transaction(tx.clone()) {
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
        }

        let BlockBuilderOutcome { execution_result, block, .. } = builder.finish(&state_provider)?;
        let sealed_block = Arc::new(block.sealed_block().clone().into());
        // todo: seal block by parlia.
        let payload = BscBuiltPayload {
            block: sealed_block,
            fees: U256::ZERO, // TODO: calculate fees from execution result
            requests: Some(execution_result.requests),
        };
        Ok(payload)
    }
}
