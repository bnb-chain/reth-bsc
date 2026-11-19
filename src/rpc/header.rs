//! BSC fields added to the standard Ethereum RPC header.

use alloy_consensus::Header;
use alloy_network::Ethereum;
use alloy_primitives::U256;
use alloy_rpc_types::serde_helpers::WithOtherFields;
use reth_primitives_traits::SealedHeader;
use reth_rpc_convert::{transaction::HeaderConverter, RpcTypes};
use std::convert::Infallible;

/// RPC response types for BSC, including its additional header fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct BscRpcTypes;

impl RpcTypes for BscRpcTypes {
    type Header = WithOtherFields<alloy_rpc_types_eth::Header>;
    type Receipt = <Ethereum as RpcTypes>::Receipt;
    type Log = <Ethereum as RpcTypes>::Log;
    type TransactionResponse = <Ethereum as RpcTypes>::TransactionResponse;
    type TransactionRequest = <Ethereum as RpcTypes>::TransactionRequest;
}

/// Adds the millisecond timestamp used by Go-BSC block and header RPC responses.
#[derive(Debug, Clone, Copy, Default)]
pub struct BscHeaderConverter;

impl HeaderConverter<Header, <BscRpcTypes as RpcTypes>::Header> for BscHeaderConverter {
    type Err = Infallible;

    fn convert_header(
        &self,
        header: SealedHeader<Header>,
        block_size: Option<usize>,
        total_difficulty: Option<U256>,
    ) -> Result<<BscRpcTypes as RpcTypes>::Header, Self::Err> {
        let milli_timestamp =
            crate::consensus::parlia::util::calculate_millisecond_timestamp(&header);
        let mut rpc_header = WithOtherFields::new(alloy_rpc_types_eth::Header::from_consensus(
            header.into(),
            total_difficulty,
            block_size.map(U256::from),
        ));
        rpc_header
            .other
            .insert("milliTimestamp".to_owned(), format!("0x{milli_timestamp:x}").into());
        Ok(rpc_header)
    }
}

#[cfg(test)]
mod tests;
