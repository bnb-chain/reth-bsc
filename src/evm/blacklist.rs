use alloy_primitives::Address;
use std::{collections::HashSet, sync::LazyLock};

/// Global static blacklist for BSC Nano addresses that are not allowed to participate
pub static NANO_BLACKLIST: LazyLock<HashSet<Address>> =
    LazyLock::new(|| reth_bsc_evm::NANO_BLACKLIST.into_iter().collect());

/// Error for a transaction whose sender or recipient is in [`NANO_BLACKLIST`].
#[derive(thiserror::Error, Debug)]
#[error("sender or recipient is blacklisted")]
pub struct BlacklistedAddressError();

impl alloy_evm::InvalidTxError for BlacklistedAddressError {
    fn as_invalid_tx_err(&self) -> Option<&revm::context::result::InvalidTransaction> {
        None
    }
}

/// Checks if an address is blacklisted
pub fn is_blacklisted(address: &Address) -> bool {
    NANO_BLACKLIST.contains(address)
}

#[inline]
pub fn check_tx_basic_blacklist(from: Address, to: Option<Address>) -> bool {
    if is_blacklisted(&from) {
        return true;
    }
    if let Some(to) = to {
        if is_blacklisted(&to) {
            return true;
        }
    }
    false
}

impl reth_evm::InvalidTxError for BlacklistedAddressError {
    fn as_any(&self) -> &(dyn std::any::Any + 'static) {
        self
    }
}
