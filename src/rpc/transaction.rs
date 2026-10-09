use crate::{evm::transaction::BscTxEnv, node::evm::config::BscEvmConfig};
use alloy_evm::rpc::{EthTxEnvError, TryIntoTxEnv};
use alloy_rpc_types_eth::TransactionRequest;
use reth_evm::EvmEnvFor;
use reth_rpc_convert::transaction::TxEnvConverter;

/// Uses the same fork environment for RPC transaction defaults and execution.
/// A blob request without `maxFeePerBlobGas` must see the blob
/// price of the overridden environment before that default becomes a value in
/// `TxEnv`. Explicit fees, including zero, remain handled by alloy's converter.
#[derive(Debug, Clone)]
pub struct BscTxEnvConverter(pub BscEvmConfig);

impl TxEnvConverter<TransactionRequest, BscEvmConfig> for BscTxEnvConverter {
    type Error = EthTxEnvError;

    fn convert_tx_env(
        &self,
        request: TransactionRequest,
        env: &EvmEnvFor<BscEvmConfig>,
    ) -> Result<BscTxEnv, Self::Error> {
        request.try_into_tx_env(&self.0.with_block_rules(env.clone()))
    }
}
