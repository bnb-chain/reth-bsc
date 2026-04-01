use alloy_primitives::{Address, B256, U256};
use revm::database::BundleState;
use revm::state::{AccountInfo, Bytecode};
use revm::DatabaseRef;

/// Layers a previous block's `BundleState` on top of an inner database.
///
/// Lookups check the bundle first (in-memory, fast), then fall through to the
/// inner DB (MDBX, possibly one block stale). This is safe because:
/// - Modified accounts are in the bundle (correct, current values)
/// - Unmodified accounts are identical in MDBX (stale by one block is fine)
pub struct BundleStateOverlay<DB> {
    bundle: BundleState,
    inner: DB,
}

impl<DB> std::fmt::Debug for BundleStateOverlay<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BundleStateOverlay").finish_non_exhaustive()
    }
}

impl<DB> BundleStateOverlay<DB> {
    pub fn new(bundle: BundleState, inner: DB) -> Self {
        Self { bundle, inner }
    }
}

impl<DB: DatabaseRef> DatabaseRef for BundleStateOverlay<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(account) = self.bundle.state.get(&address) {
            return Ok(account.info.clone());
        }
        self.inner.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(bytecode) = self.bundle.contracts.get(&code_hash) {
            return Ok(bytecode.clone());
        }
        self.inner.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(account) = self.bundle.state.get(&address) {
            if let Some(slot) = account.storage.get(&index) {
                return Ok(slot.present_value);
            }
            if account.was_destroyed() {
                return Ok(U256::ZERO);
            }
        }
        self.inner.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

/// Either a plain database or one wrapped with a `BundleStateOverlay`.
///
/// Used by the pipelined commit path so block N+1 can start executing
/// immediately against block N's in-memory state without waiting for the
/// MDBX commit to complete.
pub enum MaybeOverlay<DB> {
    Plain(DB),
    Overlay(BundleStateOverlay<DB>),
}

impl<DB> std::fmt::Debug for MaybeOverlay<DB> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Plain(_) => f.write_str("MaybeOverlay::Plain"),
            Self::Overlay(_) => f.write_str("MaybeOverlay::Overlay"),
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for MaybeOverlay<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        match self {
            Self::Plain(db) => db.basic_ref(address),
            Self::Overlay(db) => db.basic_ref(address),
        }
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        match self {
            Self::Plain(db) => db.code_by_hash_ref(code_hash),
            Self::Overlay(db) => db.code_by_hash_ref(code_hash),
        }
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        match self {
            Self::Plain(db) => db.storage_ref(address, index),
            Self::Overlay(db) => db.storage_ref(address, index),
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        match self {
            Self::Plain(db) => db.block_hash_ref(number),
            Self::Overlay(db) => db.block_hash_ref(number),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BundleStateOverlay;
    use alloy_primitives::{Address, B256, U256};
    use revm::database::{
        states::{AccountStatus, BundleAccount, StorageSlot},
        BundleState,
    };
    use revm::state::{AccountInfo, Bytecode};
    use revm::DatabaseRef;
    use std::collections::HashMap;
    use std::convert::Infallible;

    #[derive(Clone, Default)]
    struct FakeDb {
        storage: HashMap<(Address, U256), U256>,
    }

    impl DatabaseRef for FakeDb {
        type Error = Infallible;

        fn basic_ref(&self, _address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            Ok(None)
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            Ok(Bytecode::default())
        }

        fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
            Ok(self.storage.get(&(address, index)).copied().unwrap_or(U256::ZERO))
        }

        fn block_hash_ref(&self, _number: u64) -> Result<B256, Self::Error> {
            Ok(B256::ZERO)
        }
    }

    fn example_address() -> Address {
        Address::with_last_byte(1)
    }

    fn bundle_with_slot(slot: U256, value: U256) -> BundleState {
        let mut storage = HashMap::default();
        storage.insert(slot, StorageSlot::new_changed(U256::ZERO, value));

        let mut bundle = BundleState::default();
        bundle.state.insert(
            example_address(),
            BundleAccount::new(
                Some(AccountInfo::default()),
                Some(AccountInfo::default()),
                storage,
                AccountStatus::Changed,
            ),
        );
        bundle
    }

    fn destroyed_bundle() -> BundleState {
        let mut bundle = BundleState::default();
        bundle.state.insert(
            example_address(),
            BundleAccount::new(
                Some(AccountInfo::default()),
                None,
                HashMap::default(),
                AccountStatus::Destroyed,
            ),
        );
        bundle
    }

    fn fake_db_with_slot(slot: U256, value: U256) -> FakeDb {
        let mut db = FakeDb::default();
        db.storage.insert((example_address(), slot), value);
        db
    }

    #[test]
    fn overlay_returns_present_storage_before_inner_db() {
        let overlay = BundleStateOverlay::new(
            bundle_with_slot(U256::from(7), U256::from(9)),
            FakeDb::default(),
        );
        assert_eq!(overlay.storage_ref(example_address(), U256::from(7)).unwrap(), U256::from(9));
    }

    #[test]
    fn destroyed_account_storage_falls_back_to_zero_not_inner_db() {
        let overlay = BundleStateOverlay::new(
            destroyed_bundle(),
            fake_db_with_slot(U256::from(7), U256::from(99)),
        );
        assert_eq!(overlay.storage_ref(example_address(), U256::from(7)).unwrap(), U256::ZERO);
    }
}
