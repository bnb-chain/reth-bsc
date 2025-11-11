use alloy_primitives::{address, Address};
use std::{collections::HashSet, sync::LazyLock};

/// Global static blacklist for BSC Nano addresses that are not allowed to participate
pub static NANO_BLACKLIST: LazyLock<HashSet<Address>> = LazyLock::new(|| {
    let blacklisted_addresses = vec![
        address!("0x489A8756C18C0b8B24EC2a2b9FF3D4d447F79BEc"),
        address!("0xFd6042Df3D74ce9959922FeC559d7995F3933c55"),
        // Test Account
        address!("0xdb789Eb5BDb4E559beD199B8b82dED94e1d056C9"),
    ];
    blacklisted_addresses.into_iter().collect()
});

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