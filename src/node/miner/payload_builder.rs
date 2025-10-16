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
use tracing::debug;
use reth_evm::block::{BlockExecutionError, BlockValidationError};
use reth::transaction_pool::error::InvalidPoolTransactionError;
use reth_primitives::InvalidTransactionError;
use reth_evm::execute::BlockBuilderOutcome;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use std::sync::Arc;
use reth_basic_payload_builder::{BuildArguments, PayloadConfig};
use reth::payload::EthPayloadBuilderAttributes;
use reth_payload_primitives::PayloadBuilderAttributes;
use alloy_consensus::{Transaction, BlockHeader};
use reth_primitives_traits::SignerRecoverable;
use tracing::warn;
use crate::chainspec::{BscChainSpec};
use reth::transaction_pool::error::Eip4844PoolTransactionError;
use crate::node::primitives::BscBlobTransactionSidecar;
use std::collections::HashMap;
use reth_chainspec::EthChainSpec;
use reth_chainspec::EthereumHardforks;

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

    chain_spec: Arc<BscChainSpec>,
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
        chain_spec: Arc<BscChainSpec>,
    ) -> Self {
        Self { client, pool, evm_config, builder_config, chain_spec }
    }

    // todo: check more and refine it later.
    pub fn build_payload(self, args: BuildArguments<EthPayloadBuilderAttributes, BscBuiltPayload>) -> Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>> {
        let BuildArguments { mut cached_reads, config, cancel: _cancel, best_payload: _best_payload } = args;
        let PayloadConfig { parent_header, attributes } = config;

        let state_provider = self.client.state_by_block_hash(parent_header.hash_slow())?;
        let state = StateProviderDatabase::new(&state_provider);
        let mut db = State::builder().with_database(cached_reads.as_db_mut(state)).with_bundle_update().build();
        
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
        
        let mut sidecars_map = HashMap::new();
        let mut block_blob_count = 0;
        // todo: calc blob fee.

        let blob_params = self.chain_spec.blob_params_at_timestamp(attributes.timestamp());
        let max_blob_count = blob_params.as_ref().map(|params| params.max_blob_count).unwrap_or_default();
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
            let mut blob_tx_sidecar = None;
            debug!("debug payload_builder, tx: {:?} is blob tx: {:?} tx type: {:?}", tx.hash(), tx.is_eip4844(), tx.tx_type());
            if let Some(blob_tx) = tx.as_eip4844() {
                let tx_blob_count = blob_tx.tx().blob_versioned_hashes.len() as u64;

                if block_blob_count + tx_blob_count > max_blob_count {
                    // we can't fit this _blob_ transaction into the block, so we mark it as
                    // invalid, which removes its dependent transactions from
                    // the iterator. This is similar to the gas limit condition
                    // for regular transactions above.
                    debug!(target: "payload_builder", tx=?tx.hash(), ?block_blob_count, "skipping blob transaction because it would exceed the max blob count per block");
                    best_tx_list.mark_invalid(
                        &pool_tx,
                        InvalidPoolTransactionError::Eip4844(
                            Eip4844PoolTransactionError::TooManyEip4844Blobs {
                                have: block_blob_count + tx_blob_count,
                                permitted: max_blob_count,
                            },
                        ),
                    );
                    continue
                }

                let blob_sidecar_result = 'sidecar: {
                    let Some(sidecar) =
                        self.pool.get_blob(*tx.hash()).map_err(PayloadBuilderError::other)?
                    else {
                        break 'sidecar Err(Eip4844PoolTransactionError::MissingEip4844BlobSidecar)
                    };

                    if self.chain_spec.is_osaka_active_at_timestamp(attributes.timestamp()) {
                        if sidecar.is_eip7594() {
                            Ok(sidecar)
                        } else {
                            Err(Eip4844PoolTransactionError::UnexpectedEip4844SidecarAfterOsaka)
                        }
                    } else if sidecar.is_eip4844() {
                        Ok(sidecar)
                    } else {
                        Err(Eip4844PoolTransactionError::UnexpectedEip7594SidecarBeforeOsaka)
                    }
                };
                debug!("debug payload_builder,tx hash: {:?}, blob_sidecar_result: {:?}", tx.hash(), blob_sidecar_result);

                blob_tx_sidecar = match blob_sidecar_result {
                    Ok(sidecar) => Some(sidecar),
                    Err(error) => {
                        best_tx_list.mark_invalid(&pool_tx, InvalidPoolTransactionError::Eip4844(error));
                        continue
                    }
                };
                debug!("debug payload_builder, tx hash: {:?}, blob_tx_sidecar: {:?}", tx.hash(), blob_tx_sidecar);
            }
            
            let gas_used = match builder.execute_transaction(tx.clone()) {
                Ok(gas_used) => gas_used,
                Err(BlockExecutionError::Validation(BlockValidationError::InvalidTx {
                    error, ..
                })) => {
                    if error.is_nonce_too_low() {
                        // if the nonce is too low, we can skip this transaction
                        debug!(target: "payload_builder", %error, ?tx, "skipping nonce too low transaction");
                    } else {
                        // if the transaction is invalid, we can skip it and all of its
                        // descendants
                        debug!(target: "payload_builder", %error, ?tx, "skipping invalid transaction and its descendants");
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

             // add to the total blob gas used if the transaction successfully executed
            if let Some(blob_tx) = tx.as_eip4844() {
                block_blob_count += blob_tx.tx().blob_versioned_hashes.len() as u64;

                // if we've reached the max blob count, we can skip blob txs entirely
                if block_blob_count == max_blob_count {
                    best_tx_list.skip_blobs();
                }
            }
            // update and add to total fees
            let miner_fee = tx.effective_tip_per_gas(base_fee).expect("fee is always valid; execution succeeded");
            total_fees += U256::from(miner_fee) * U256::from(gas_used);
            cumulative_gas_used += gas_used;

            // Add blob tx sidecar to the payload.
            if let Some(sidecar) = blob_tx_sidecar {
                sidecars_map.insert(*tx.hash(), sidecar);
            }
        }

        // add system txs to payload, need to rewrite finish.
        // check: rewrite in here.
        let BlockBuilderOutcome { execution_result, block, .. } = builder.finish(&state_provider)?;
        let mut sealed_block = Arc::new(block.sealed_block().clone());
        
        // set sidecars to seal block
        let mut blob_sidecars:Vec<BscBlobTransactionSidecar>= Vec::new();
        let transactions = &sealed_block.body().inner.transactions;
        debug!("debug payload_builder, block_number: {}, block_hash: {:?}, contains {} transactions:", sealed_block.number(), sealed_block.hash(), transactions.len());
        for (index, tx) in transactions.iter().enumerate() {
            debug!("debug payload_builder, transaction {}: hash={:?}, from={:?}, to={:?}, value={:?}, gas_limit={}, gas_price={:?}, nonce={}", 
                index + 1,
                tx.hash(),
                tx.recover_signer().ok(),
                tx.to(),
                tx.value(),
                tx.gas_limit(),
                tx.gas_price(),
                tx.nonce()
            );
            if tx.is_eip4844() {
                let sidecar = sidecars_map.get(tx.hash()).unwrap();
                let bsc_blob_tx_sidecar = BscBlobTransactionSidecar {
                    inner: sidecar.as_eip4844().unwrap().clone(),
                    block_number: sealed_block.header().number(),
                    block_hash: sealed_block.hash(),
                    tx_index: index as u64,
                    tx_hash: *tx.hash(),
                };
                blob_sidecars.push(bsc_blob_tx_sidecar);
            }
        }

        let mut plain = sealed_block.clone_block();
        plain.body.sidecars = Some(blob_sidecars);
        sealed_block = Arc::new(plain.into());
    
        debug!("debug payload_builder, sealed_block: {:?}", sealed_block);
        let payload = BscBuiltPayload {
            block: sealed_block,
            fees: total_fees,
            requests: Some(execution_result.requests),
        };
        Ok(payload)
    }
}