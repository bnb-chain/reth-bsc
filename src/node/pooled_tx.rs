use alloy_consensus::transaction::Recovered;
use alloy_consensus::{
    EthereumTxEnvelope, SignableTransaction, TxEip1559, TxEip2930, TxEip4844, TxEip4844Variant,
    TxEip7702, TxEnvelope, TxLegacy,
};
use alloy_primitives::{Signature, B256, U256};
use reth::transaction_pool::{EthBlobTransactionSidecar, EthPooledTransaction};

#[allow(unused)]
fn mock_eth_pooled_transaction_new_legacy(
) -> EthPooledTransaction<EthereumTxEnvelope<TxEip4844Variant>> {
    // Create a legacy transaction with specific parameters
    let tx = TxEnvelope::Legacy(
        TxLegacy { gas_price: 10, gas_limit: 1000, value: U256::from(100), ..Default::default() }
            .into_signed(Signature::test_signature()),
    );
    let transaction = Recovered::new_unchecked(tx, Default::default());
    let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);

    pooled_tx
}

#[allow(unused)]
fn mock_eth_pooled_transaction_new_eip2930(
) -> EthPooledTransaction<EthereumTxEnvelope<TxEip4844Variant>> {
    // Create an EIP-2930 transaction with specific parameters
    let tx = TxEnvelope::Eip2930(
        TxEip2930 { gas_price: 10, gas_limit: 1000, value: U256::from(100), ..Default::default() }
            .into_signed(Signature::test_signature()),
    );
    let transaction = Recovered::new_unchecked(tx, Default::default());
    let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);
    pooled_tx
}

#[allow(unused)]
fn mock_eth_pooled_transaction_new_eip1559(
) -> EthPooledTransaction<EthereumTxEnvelope<TxEip4844Variant>> {
    // Create an EIP-1559 transaction with specific parameters
    let tx = TxEnvelope::Eip1559(
        TxEip1559 {
            max_fee_per_gas: 10,
            gas_limit: 1000,
            value: U256::from(100),
            ..Default::default()
        }
        .into_signed(Signature::test_signature()),
    );
    let transaction = Recovered::new_unchecked(tx, Default::default());
    let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);
    pooled_tx
}

#[allow(unused)]
fn mock_eth_pooled_transaction_new_eip4844() -> EthPooledTransaction<EthereumTxEnvelope<TxEip4844>>
{
    // Create an EIP-4844 transaction with specific parameters
    let tx = EthereumTxEnvelope::Eip4844(
        TxEip4844 {
            max_fee_per_gas: 10,
            gas_limit: 1000,
            value: U256::from(100),
            max_fee_per_blob_gas: 5,
            blob_versioned_hashes: vec![B256::default()],
            ..Default::default()
        }
        .into_signed(Signature::test_signature()),
    );
    let transaction = Recovered::new_unchecked(tx, Default::default());
    let pooled_tx = EthPooledTransaction::new(transaction.clone(), 300);

    pooled_tx
}

#[allow(unused)]
fn test_eth_pooled_transaction_new_eip7702() {
    // Init an EIP-7702 transaction with specific parameters
    let tx = EthereumTxEnvelope::<TxEip4844>::Eip7702(
        TxEip7702 {
            max_fee_per_gas: 10,
            gas_limit: 1000,
            value: U256::from(100),
            ..Default::default()
        }
        .into_signed(Signature::test_signature()),
    );
    let transaction = Recovered::new_unchecked(tx, Default::default());
    let pooled_tx = EthPooledTransaction::new(transaction.clone(), 200);

    // Check that the pooled transaction is created correctly
    assert_eq!(pooled_tx.transaction, transaction);
    assert_eq!(pooled_tx.encoded_length, 200);
    assert_eq!(pooled_tx.blob_sidecar, EthBlobTransactionSidecar::None);
    assert_eq!(pooled_tx.cost, U256::from(100) + U256::from(10 * 1000));
}
