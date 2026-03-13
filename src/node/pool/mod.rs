use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use alloy_consensus::{BlockHeader, Transaction};
use alloy_eips::merge::EPOCH_SLOTS;
use reth::api::FullNodeTypes;
use reth::api::{NodePrimitives, NodeTypes};
use reth::builder::{
    components::{create_blob_store_with_cache, PoolBuilder, TxPoolBuilder},
    BuilderContext,
};
use reth_chainspec::{EthChainSpec, EthereumHardforks, ForkCondition, Hardforks};
use reth_ethereum_primitives::TransactionSigned as EthTxSigned;
use reth_payload_primitives::PayloadTypes;
use reth_primitives_traits::SignedTransaction;
use reth_primitives_traits::constants::MAX_TX_GAS_LIMIT_OSAKA;
use reth_transaction_pool::{
    blobstore::DiskFileBlobStore, error::InvalidPoolTransactionError, PoolTransaction,
    TransactionOrigin, TransactionValidationOutcome, TransactionValidationTaskExecutor,
    TransactionValidator,
};
use reth_transaction_pool::{
    CoinbaseTipOrdering, EthPooledTransaction, EthTransactionValidator, Pool,
};

use crate::evm::blacklist;
use crate::hardforks::bsc::BscHardfork;

/// Transaction pool blacklist error type: marked as "bad transaction" to punish source node
#[derive(thiserror::Error, Debug)]
#[error("sender or recipient is blacklisted")]
pub struct BlacklistedAddressError();

impl reth_transaction_pool::error::PoolTransactionError for BlacklistedAddressError {
    fn is_bad_transaction(&self) -> bool {
        true
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// BSC transaction validator: adds blacklist validation and Osaka gas limit check
/// to the default Ethereum transaction validator.
///
/// EthereumHardfork::Osaka is blocked in BscChainSpec to prevent EIP-7594 sidecar conversion,
/// so the upstream pool validator's Osaka gas limit check never fires. This validator
/// compensates by tracking BscHardfork::Osaka activation independently.
#[derive(Debug, Clone)]
pub struct BscTxValidator<V> {
    inner: Arc<V>,
    osaka_activated: Arc<AtomicBool>,
    osaka_timestamp: Option<u64>,
}

impl<V> BscTxValidator<V> {
    pub fn new(inner: V, osaka_activated: bool, osaka_timestamp: Option<u64>) -> Self {
        Self {
            inner: Arc::new(inner),
            osaka_activated: Arc::new(AtomicBool::new(osaka_activated)),
            osaka_timestamp,
        }
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
            return TransactionValidationOutcome::Invalid(
                transaction,
                InvalidPoolTransactionError::other(BlacklistedAddressError()),
            );
        }

        if self.osaka_activated.load(Ordering::Relaxed)
            && transaction.gas_limit() > MAX_TX_GAS_LIMIT_OSAKA
        {
            return TransactionValidationOutcome::Invalid(
                transaction,
                reth_primitives_traits::transaction::error::InvalidTransactionError::GasLimitTooHigh
                    .into(),
            );
        }

        // Delegate to internal validator
        self.inner.validate_transaction(origin, transaction).await
    }

    async fn validate_transactions(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<TransactionValidationOutcome<Self::Transaction>> {
        let outcomes = self.inner.validate_transactions(transactions).await;
        let mut mapped: Vec<TransactionValidationOutcome<Self::Transaction>> =
            Vec::with_capacity(outcomes.len());
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
                    if blacklist::check_tx_basic_blacklist(
                        transaction.transaction().sender(),
                        transaction.transaction().to(),
                    ) {
                        tracing::debug!(target: "bsc::txpool", "Blacklisted transaction: {:?}", transaction.hash());
                        TransactionValidationOutcome::Invalid(
                            transaction.into_transaction(),
                            InvalidPoolTransactionError::other(BlacklistedAddressError()),
                        )
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
        if let Some(osaka_ts) = self.osaka_timestamp {
            self.osaka_activated.store(
                new_tip_block.header().timestamp() >= osaka_ts,
                Ordering::Relaxed,
            );
        }
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
        ChainSpec: EthChainSpec + EthereumHardforks + Hardforks,
        Primitives: NodePrimitives<SignedTx = EthTxSigned>,
    >,
    <Types as NodeTypes>::Primitives: NodePrimitives<SignedTx: SignedTransaction>,
    <Types as NodeTypes>::Payload: PayloadTypes,
    EthPooledTransaction<EthTxSigned>: reth_transaction_pool::EthPoolTransaction,
    EthPooledTransaction<EthTxSigned>: PoolTransaction,
{
    type Pool = Pool<
        TransactionValidationTaskExecutor<
            BscTxValidator<EthTransactionValidator<Node::Provider, EthPooledTransaction>>,
        >,
        CoinbaseTipOrdering<EthPooledTransaction>,
        DiskFileBlobStore,
    >;

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
                .chain_spec()
                .blob_params_at_timestamp(current_timestamp)
                .unwrap_or_else(BlobParams::cancun);
            Some((blob_params.target_blob_count * EPOCH_SLOTS * 2) as u32)
        };

        let blob_store = create_blob_store_with_cache(ctx, blob_cache_size)?;

        // Build default Ethereum validator executor
        // BSC rejected EIP-7594 (PeerDAS), so we disable EIP-7594 sidecar support to always
        // use v0 (legacy) blob sidecars and reject v1 (EIP-7594) sidecars.
        let validator = TransactionValidationTaskExecutor::eth_builder(ctx.provider().clone())
            .with_head_timestamp(ctx.head().timestamp)
            .with_max_tx_input_bytes(ctx.config().txpool.max_tx_input_bytes)
            .kzg_settings(ctx.kzg_settings()?)
            .with_local_transactions_config(pool_config.local_transactions_config.clone())
            .set_tx_fee_cap(ctx.config().rpc.rpc_tx_fee_cap)
            .with_max_tx_gas_limit(ctx.config().txpool.max_tx_gas_limit)
            .with_minimum_priority_fee(ctx.config().txpool.minimum_priority_fee)
            .with_additional_tasks(ctx.config().txpool.additional_validation_tasks)
            .no_eip7594()
            .build_with_tasks(ctx.task_executor().clone(), blob_store.clone());

        // Determine BscHardfork::Osaka activation for pool-level gas limit check.
        // EthereumHardfork::Osaka is blocked in BscChainSpec to prevent EIP-7594
        // sidecar conversion, so the upstream validator's Osaka check never fires.
        let osaka_timestamp = match ctx.chain_spec().fork(BscHardfork::Osaka) {
            ForkCondition::Timestamp(ts) => Some(ts),
            _ => None,
        };
        let osaka_activated = osaka_timestamp
            .is_some_and(|ts| ctx.head().timestamp >= ts);

        let validator = validator.map(|v| BscTxValidator::new(v, osaka_activated, osaka_timestamp));

        // Build txpool and start maintenance task
        let transaction_pool = TxPoolBuilder::new(ctx)
            .with_validator(validator)
            .build_and_spawn_maintenance_task(blob_store, pool_config)?;

        tracing::info!(target: "bsc::txpool", "Transaction pool with blacklist validation initialized");
        Ok(transaction_pool)
    }
}
