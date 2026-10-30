//! Parent post-state reads shared by canonical and unpublished-child construction.
use reth_revm::cached::CachedReads;
use revm::database::BundleState;

/// Seed only the parent's final changed values. Untouched slots and absent/deleted accounts
/// remain provider reads; never inherit an older parent's original values through a wipe.
pub(crate) fn cached_reads(state: &BundleState) -> CachedReads {
    let mut cached = CachedReads::default();
    for (address, account) in &state.state {
        if let Some(info) = account.info.clone() {
            let storage =
                account.storage.iter().map(|(slot, value)| (*slot, value.present_value)).collect();
            cached.insert_account(*address, info, storage);
        }
    }
    cached
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::{Address, U256};
    use revm::{state::AccountInfo, Database, DatabaseRef};
    use std::convert::Infallible;
    #[derive(Debug)]
    struct Parent;
    impl DatabaseRef for Parent {
        type Error = Infallible;
        fn basic_ref(&self, _: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }
        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Self::Error> {
            Ok(U256::from(99))
        }
        fn code_by_hash_ref(
            &self,
            _: alloy_primitives::B256,
        ) -> Result<revm::bytecode::Bytecode, Self::Error> {
            Ok(Default::default())
        }
        fn block_hash_ref(&self, _: u64) -> Result<alloy_primitives::B256, Self::Error> {
            Ok(Default::default())
        }
    }
    #[test]
    fn parent_post_state_overrides_original_values_without_inventing_untouched_slots() {
        let address = Address::with_last_byte(1);
        let slot = U256::from(7);
        let deleted = Address::with_last_byte(3);
        let state = BundleState::builder(1..=1)
            .state_original_account_info(deleted, AccountInfo { nonce: 99, ..Default::default() })
            .state_present_account_info(address, AccountInfo { nonce: 4, ..Default::default() })
            .state_storage(
                address,
                [
                    (slot, (U256::from(10), U256::from(20))),
                    (U256::from(9), (U256::from(123), U256::ZERO)),
                ]
                .into_iter()
                .collect(),
            )
            .build();
        let mut cached = cached_reads(&state);
        assert!(!cached.accounts.contains_key(&deleted));
        let mut db = cached.as_db_mut(Parent);
        assert_eq!(db.basic(address).unwrap().unwrap().nonce, 4);
        assert_eq!(db.storage(address, slot).unwrap(), U256::from(20));
        assert_eq!(db.storage(address, U256::from(8)).unwrap(), U256::from(99));
        assert_eq!(db.storage(address, U256::from(9)).unwrap(), U256::ZERO);
        assert!(db.basic(Address::with_last_byte(2)).unwrap().is_none());
        assert!(db.basic(deleted).unwrap().is_none());
    }
}
