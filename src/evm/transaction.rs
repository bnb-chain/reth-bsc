use alloy_evm::{rpc::TryIntoTxEnv, EvmEnv};
use alloy_rpc_types_eth::{AccessList, TransactionRequest};
use reth_evm::{FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, TransactionEnv};
use reth_primitives::TransactionSigned;
use revm::{
    context::TxEnv,
    context_interface::transaction::Transaction,
    handler::SystemCallTx,
    primitives::{Address, Bytes, TxKind, B256, U256},
};

#[derive(Clone, Debug, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BscTxEnv {
    pub base: TxEnv,
    pub is_system_transaction: bool,
}

impl BscTxEnv {
    pub fn new(base: TxEnv) -> Self {
        Self { base, is_system_transaction: false }
    }
}

impl Transaction for BscTxEnv {
    type AccessListItem<'a> = <TxEnv as Transaction>::AccessListItem<'a>;
    type Authorization<'a> = <TxEnv as Transaction>::Authorization<'a>;

    fn tx_type(&self) -> u8 {
        self.base.tx_type()
    }

    fn caller(&self) -> Address {
        self.base.caller()
    }

    fn gas_limit(&self) -> u64 {
        self.base.gas_limit()
    }

    fn value(&self) -> U256 {
        self.base.value()
    }

    fn input(&self) -> &Bytes {
        self.base.input()
    }

    fn nonce(&self) -> u64 {
        Transaction::nonce(&self.base)
    }

    fn kind(&self) -> TxKind {
        self.base.kind()
    }

    fn chain_id(&self) -> Option<u64> {
        self.base.chain_id()
    }

    fn gas_price(&self) -> u128 {
        self.base.gas_price()
    }

    fn access_list(&self) -> Option<impl Iterator<Item = Self::AccessListItem<'_>>> {
        self.base.access_list()
    }

    fn blob_versioned_hashes(&self) -> &[B256] {
        self.base.blob_versioned_hashes()
    }

    fn max_fee_per_blob_gas(&self) -> u128 {
        self.base.max_fee_per_blob_gas()
    }

    fn authorization_list_len(&self) -> usize {
        self.base.authorization_list_len()
    }

    fn authorization_list(&self) -> impl Iterator<Item = Self::Authorization<'_>> {
        self.base.authorization_list()
    }

    fn max_fee_per_gas(&self) -> u128 {
        self.base.max_fee_per_gas()
    }

    fn max_priority_fee_per_gas(&self) -> Option<u128> {
        self.base.max_priority_fee_per_gas()
    }

    fn effective_gas_price(&self, base_fee: u128) -> u128 {
        self.base.effective_gas_price(base_fee)
    }
}

impl IntoTxEnv<Self> for BscTxEnv {
    fn into_tx_env(self) -> Self {
        self
    }
}

impl FromRecoveredTx<TransactionSigned> for BscTxEnv {
    fn from_recovered_tx(tx: &TransactionSigned, sender: Address) -> Self {
        Self::new(TxEnv::from_recovered_tx(tx, sender))
    }
}

impl FromTxWithEncoded<TransactionSigned> for BscTxEnv {
    fn from_encoded_tx(tx: &TransactionSigned, sender: Address, _encoded: Bytes) -> Self {
        let base = match tx.clone().into_typed_transaction() {
            reth_primitives::Transaction::Legacy(tx) => TxEnv::from_recovered_tx(&tx, sender),
            reth_primitives::Transaction::Eip2930(tx) => TxEnv::from_recovered_tx(&tx, sender),
            reth_primitives::Transaction::Eip1559(tx) => TxEnv::from_recovered_tx(&tx, sender),
            reth_primitives::Transaction::Eip4844(tx) => TxEnv::from_recovered_tx(&tx, sender),
            reth_primitives::Transaction::Eip7702(tx) => TxEnv::from_recovered_tx(&tx, sender),
        };

        Self { base, is_system_transaction: false }
    }
}

impl TransactionEnv for BscTxEnv {
    fn set_gas_limit(&mut self, gas_limit: u64) {
        self.base.set_gas_limit(gas_limit);
    }

    fn nonce(&self) -> u64 {
        TransactionEnv::nonce(&self.base)
    }

    fn set_nonce(&mut self, nonce: u64) {
        self.base.set_nonce(nonce);
    }

    fn set_access_list(&mut self, access_list: AccessList) {
        self.base.set_access_list(access_list);
    }
}

impl SystemCallTx for BscTxEnv {
    fn new_system_tx_with_caller(
        caller: Address,
        system_contract_address: Address,
        data: Bytes,
    ) -> Self {
        let base = TxEnv::builder()
            .caller(caller)
            .data(data)
            .kind(TxKind::Call(system_contract_address))
            .gas_limit(30_000_000) // Use BSC's gas limit for system calls
            .build()
            .unwrap();

        Self { base, is_system_transaction: true }
    }
}

impl TryIntoTxEnv<BscTxEnv> for TransactionRequest {
    type Err = <TransactionRequest as TryIntoTxEnv<TxEnv>>::Err;

    fn try_into_tx_env<Spec>(
        self,
        evm_env: &EvmEnv<Spec>,
    ) -> Result<BscTxEnv, <TransactionRequest as TryIntoTxEnv<TxEnv>>::Err> {
        Ok(BscTxEnv {
            base: <TransactionRequest as TryIntoTxEnv<TxEnv>>::try_into_tx_env(self, evm_env)?,
            is_system_transaction: false,
        })
    }
}

#[cfg(test)]
mod tests {

    use super::*;
    use revm::primitives::Address;

    #[test]
    fn test_bsc_transaction_fields() {
        let bsc_tx = BscTxEnv {
            base: TxEnv {
                tx_type: 0,
                gas_limit: 10,
                gas_price: 100,
                gas_priority_fee: Some(5),
                ..Default::default()
            },
            is_system_transaction: false,
        };

        assert_eq!(bsc_tx.tx_type(), 0);
        assert_eq!(bsc_tx.gas_limit(), 10);
        assert_eq!(bsc_tx.kind(), revm::primitives::TxKind::Call(Address::ZERO));
    }
}
