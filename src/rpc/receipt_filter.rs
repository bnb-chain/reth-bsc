//! BSC Receipt Filter for excluding system transaction logs from RPC responses.
//!
//! BSC system transactions (validator rewards, cross-chain operations, etc.) are
//! consensus-internal transactions injected by the Parlia engine. Their receipts
//! are included in the receipt trie root (for block validation), but their logs
//! should be excluded from `eth_getLogs`, `eth_getFilterChanges`, and
//! `eth_subscribe("logs")` RPC responses to match BSC geth behavior.

use alloy_eips::BlockNumHash;
use alloy_primitives::Address;
use reth_rpc_eth_types::logs_utils::ReceiptFilter;

use crate::system_contracts::SYSTEM_CONTRACTS_SET;

/// BSC receipt filter that excludes system transaction logs from RPC responses.
///
/// A system transaction is identified by three criteria:
/// 1. The transaction signer equals the block's beneficiary (coinbase/validator)
/// 2. The transaction recipient is a BSC system contract
/// 3. The transaction's max_fee_per_gas is 0
///
/// This filter is applied during log retrieval to match BSC geth's behavior of
/// not returning system transaction logs in `eth_getLogs` and subscriptions.
#[derive(Debug, Clone, Default)]
pub struct BscReceiptFilter;

impl ReceiptFilter for BscReceiptFilter {
    fn should_include(
        &self,
        _block_num_hash: BlockNumHash,
        _receipt_idx: usize,
        beneficiary: Address,
        tx_signer: Address,
        tx_to: Option<Address>,
        tx_max_fee_per_gas: u128,
    ) -> bool {
        // Check if this is a system transaction:
        // signer == coinbase && to is system contract && max_fee_per_gas == 0
        let is_system_tx = tx_signer == beneficiary
            && tx_to.map_or(false, |to| SYSTEM_CONTRACTS_SET.contains(&to))
            && tx_max_fee_per_gas == 0;

        // Include the receipt only if it's NOT a system transaction
        !is_system_tx
    }
}
