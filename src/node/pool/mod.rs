use std::sync::Arc;

use alloy_consensus::Transaction;
use alloy_eips::merge::EPOCH_SLOTS;
use reth_chainspec::{EthChainSpec, EthereumHardforks};
use reth::api::{NodePrimitives, NodeTypes};
use reth::builder::{
	components::{create_blob_store_with_cache, PoolBuilder, TxPoolBuilder},
	BuilderContext,
};
use reth::api::FullNodeTypes;
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::SignedTransaction;
use reth_ethereum_primitives::TransactionSigned as EthTxSigned;
use reth_transaction_pool::{CoinbaseTipOrdering, EthPooledTransaction, EthTransactionValidator, Pool};
use reth_transaction_pool::{
	blobstore::DiskFileBlobStore, error::InvalidPoolTransactionError, TransactionOrigin,
	TransactionValidationOutcome, TransactionValidationTaskExecutor, TransactionValidator,
	PoolTransaction,
};

use crate::evm::blacklist;

/// Transaction pool blacklist error type: marked as "bad transaction" to punish source node
#[derive(thiserror::Error, Debug)]
#[error("sender or recipient is blacklisted")]
pub struct BlacklistedAddressError ();

impl reth_transaction_pool::error::PoolTransactionError for BlacklistedAddressError {
	fn is_bad_transaction(&self) -> bool {
		true
	}
	fn as_any(&self) -> &dyn std::any::Any {
		self
	}
}

/// BSC transaction validator: add blacklist validation to the default Ethereum transaction validator.
#[derive(Debug, Clone)]
pub struct BscTxValidator<V> {
	inner: Arc<V>,
}

impl<V> BscTxValidator<V> {
	pub fn new(inner: V) -> Self {
		Self { inner: Arc::new(inner) }
	}
}

impl<V> TransactionValidator for BscTxValidator<V>
where
	V: TransactionValidator + Send + Sync + 'static,
{
	type Transaction = <V as TransactionValidator>::Transaction;

	async fn validate_transaction(
		&self,
		origin: TransactionOrigin,
		transaction: Self::Transaction,
	) -> TransactionValidationOutcome<Self::Transaction> {
		if blacklist::check_tx_basic_blacklist(transaction.sender(), transaction.to()) {
                  tracing::debug!(target: "bsc::txpool", "Blacklisted transaction: {:?}", transaction.hash());
			return TransactionValidationOutcome::Invalid(transaction, InvalidPoolTransactionError::other(BlacklistedAddressError()));
		}

		// Delegate to internal validator
            self.inner.validate_transaction(origin, transaction).await
	}

	async fn validate_transactions(
		&self,
		transactions: Vec<(TransactionOrigin, Self::Transaction)>,
	) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
		let outcomes = self.inner.validate_transactions(transactions).await;
		let mut mapped: Vec<TransactionValidationOutcome<Self::Transaction>> = Vec::with_capacity(outcomes.len());
		for outcome in outcomes {
			let new_outcome: TransactionValidationOutcome<Self::Transaction> = match outcome {
				TransactionValidationOutcome::Valid {
					balance,
					state_nonce,
					bytecode_hash,
					transaction,
					propagate,
					authorities,
				} => {
					if blacklist::check_tx_basic_blacklist(transaction.transaction().sender(), transaction.transaction().to()) {
                                    tracing::debug!(target: "bsc::txpool", "Blacklisted transaction: {:?}", transaction.hash());
						TransactionValidationOutcome::Invalid(transaction.into_transaction(), InvalidPoolTransactionError::other(BlacklistedAddressError()))
					} else {
						TransactionValidationOutcome::Valid {
							balance,
							state_nonce,
							bytecode_hash,
							transaction,
							propagate,
							authorities,
						}
					}
				}
				other => other,
			};
			mapped.push(new_outcome);
		}
		mapped
	}

	fn on_new_head_block<B>(&self, new_tip_block: &reth_primitives_traits::SealedBlock<B>)
	where
		B: reth_primitives_traits::Block,
	{
		self.inner.on_new_head_block(new_tip_block)
	}
}

/// BSC custom transaction pool builder: add blacklist validation to the default Ethereum pool builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscPoolBuilder;

impl<Types, Node> PoolBuilder<Node> for BscPoolBuilder
where
      Node: FullNodeTypes<Types = Types>,
	Types: NodeTypes<
		ChainSpec: EthChainSpec + EthereumHardforks,
		Primitives: NodePrimitives<SignedTx = EthTxSigned>,
	>,
	<Types as NodeTypes>::Primitives: NodePrimitives<SignedTx: SignedTransaction>,
	<Types as NodeTypes>::Payload: PayloadTypes,
    EthPooledTransaction<EthTxSigned>: reth_transaction_pool::EthPoolTransaction,
    EthPooledTransaction<EthTxSigned>: PoolTransaction,
{
	type Pool = Pool<
            TransactionValidationTaskExecutor<
                BscTxValidator<
                    EthTransactionValidator<Node::Provider, EthPooledTransaction>
                >
            >,
            CoinbaseTipOrdering<EthPooledTransaction>,
            DiskFileBlobStore>;

	async fn build_pool(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Pool> {
		let pool_config = ctx.pool_config();

		// Same as upstream: derive blob cache size based on time
		let blob_cache_size = if let Some(blob_cache_size) = pool_config.blob_cache_size {
			Some(blob_cache_size)
		} else {
			use alloy_eips::eip7840::BlobParams;
			use std::time::SystemTime;

			let current_timestamp =
				SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)?.as_secs();
			let blob_params = ctx
				.chain_spec().blob_params_at_timestamp(current_timestamp)
				.unwrap_or_else(BlobParams::cancun);
			Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
		};

		let blob_store = create_blob_store_with_cache(ctx, blob_cache_size)?;

		// Build default Ethereum validator executor
		let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
			.with_head_timestamp(ctx.head().timestamp)
			.with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
			.kzg_settings(ctx.kzg_settings()?)
			.with_local_transactions_config(pool_config.local_transactions_config.clone())
			.set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
			.with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
			.with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
			.with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
			.build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

		// Inject blacklist wrapper
		let validator = validator.map(BscTxValidator::new);

		// Build txpool and start maintenance task
		let transaction_pool = TxPoolBuilder::new(ctx)
			.with_validator(validator)
			.build_and_spawn_maintenance_task(blob_store, pool_config)?;

		reth_tracing::tracing::info!(target: "bsc::txpool", "Transaction pool with blacklist validation initialized");
		Ok(transaction_pool)
	}
}


