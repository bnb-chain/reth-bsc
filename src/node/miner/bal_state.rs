//! Immutable BAL-derived parent state. This does not establish execution validity.

use alloy_consensus::Header;
use alloy_eips::eip7928::{bal::Bal, BlockAccessList};
use alloy_primitives::{
    keccak256,
    map::{AddressMap, B256Map, U256Map},
    Address, B256, U256,
};
use reth_primitives_traits::Account;
use reth_provider::{
    EvmStateProviderAdapter, ProviderError, ProviderResult, StateProviderBox, StateProviderFactory,
};
use reth_revm::database::StateProviderDatabase;
use reth_trie_common::{HashedPostState, HashedStorage};
use revm::{bytecode::Bytecode, state::AccountInfo, DatabaseRef};

/// Final BAL writes layered over a hash-pinned state provider. Child writes belong in a
/// separate revm State; this view never commits to the underlying provider.
#[derive(Debug)]
pub(crate) struct BalParentState {
    accounts: AddressMap<Option<AccountInfo>>,
    storage: AddressMap<U256Map<U256>>,
    contracts: B256Map<Bytecode>,
    number: u64,
    hash: B256,
}

impl BalParentState {
    fn derive<DB: DatabaseRef<Error = ProviderError>>(
        base: &DB,
        header: &Header,
        bal: &Bal,
    ) -> ProviderResult<(Self, HashedPostState)> {
        let mut state = Self {
            accounts: Default::default(),
            storage: Default::default(),
            contracts: Default::default(),
            number: header.number,
            hash: header.hash_slow(),
        };
        let mut hashed = HashedPostState::default();
        for changes in bal.iter() {
            let fields = changes.account_info();
            if !fields.changes_state_root(changes) {
                continue;
            }
            let address = changes.address();
            let mut info = base.basic_ref(address)?.unwrap_or_default();
            // Provider-specific storage IDs cannot refer to this overlaid account.
            info.account_id = None;
            if let Some(balance) = fields.balance {
                info.balance = balance;
            }
            if let Some(nonce) = fields.nonce {
                info.nonce = nonce;
            }
            if let Some((hash, code)) = changes.code_post_state_with_hash() {
                let code = Bytecode::new_raw_checked(code.clone()).map_err(ProviderError::other)?;
                info.code_hash = hash;
                info.code = Some(code.clone());
                state.contracts.insert(hash, code);
            }
            let account = (!info.is_empty()).then_some(info);
            let hashed_address = keccak256(address);
            hashed.accounts.insert(
                hashed_address,
                account.as_ref().map(|info| Account {
                    nonce: info.nonce,
                    balance: info.balance,
                    bytecode_hash: (!info.is_empty_code_hash()).then_some(info.code_hash),
                }),
            );
            if account.is_some() && changes.has_storage_changes() {
                let storage = changes.storage_post_states().collect::<U256Map<_>>();
                hashed
                    .storages
                    .insert(hashed_address, HashedStorage::from_plain_storage(storage.iter()));
                state.storage.insert(address, storage);
            }
            state.accounts.insert(address, account);
        }
        Ok((state, hashed))
    }
}

/// Read adapter shared by normal and BAL-derived builds. An absent overlay delegates all reads.
#[derive(Debug)]
pub(crate) struct BalParentDatabase<'a, DB> {
    pub(crate) base: DB,
    pub(crate) overlay: Option<&'a BalParentState>,
}

impl<DB: DatabaseRef> DatabaseRef for BalParentDatabase<'_, DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(info) = self.overlay.and_then(|state| state.accounts.get(&address)) {
            return Ok(info.clone());
        }
        self.base.basic_ref(address)
    }

    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(code) = self.overlay.and_then(|state| state.contracts.get(&hash)) {
            return Ok(code.clone());
        }
        self.base.code_by_hash_ref(hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        if let Some(state) = self.overlay {
            if matches!(state.accounts.get(&address), Some(None)) {
                return Ok(U256::ZERO);
            }
            if let Some(value) = state.storage.get(&address).and_then(|slots| slots.get(&index)) {
                return Ok(*value);
            }
        }
        self.base.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        if let Some(state) = self.overlay {
            if number == state.number {
                return Ok(state.hash);
            }
            if number > state.number {
                return Ok(B256::ZERO);
            }
        }
        self.base.block_hash_ref(number)
    }
}

/// Optional rollout path: build from N−1 plus N's BAL only after matching N's state root.
/// The caller still waits for imported N and uses its provider to finalize child roots.
pub(crate) fn prepare(
    client: &impl StateProviderFactory,
    parent: &Header,
) -> ProviderResult<Option<(StateProviderBox, BalParentState)>> {
    if !matches!(std::env::var("BSC_BEP732_CANDIDATE_STATE").as_deref(), Ok("1" | "true")) ||
        parent.number == 0
    {
        return Ok(None);
    }
    let Some(raw) = crate::node::network::consensus_bal::get(parent.hash_slow()) else {
        metrics::counter!("bsc_bal_parent_state_total", "result" => "missing").increment(1);
        return Ok(None);
    };
    if parent.block_access_list_hash != Some(keccak256(&raw)) {
        return Err(ProviderError::other(std::io::Error::other("BAL parent commitment mismatch")));
    }
    // Retention already checks ordering and transaction indices against the full block.
    let bal =
        Bal::from(alloy_rlp::decode_exact::<BlockAccessList>(&raw).map_err(ProviderError::other)?);
    bal.validate_gas_limit(parent.gas_limit).map_err(ProviderError::other)?;
    let base = client.state_by_block_hash(parent.parent_hash)?;
    let db = StateProviderDatabase::new(EvmStateProviderAdapter(&base));
    let (overlay, hashed) = BalParentState::derive(&db, parent, &bal)?;
    let root = base.state_root(hashed)?;
    if root != parent.state_root {
        return Err(ProviderError::other(std::io::Error::other(
            "BAL-derived parent state root mismatch",
        )));
    }
    metrics::counter!("bsc_bal_parent_state_total", "result" => "derived").increment(1);
    Ok(Some((base, overlay)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_eips::eip7928::{
        AccountChanges, BalanceChange, BlockAccessIndex, CodeChange, NonceChange, SlotChanges,
        StorageChange,
    };
    use alloy_primitives::{bytes, Bytes};
    use revm::{database::CacheDB, Database};

    const ADDRESS: Address = Address::repeat_byte(1);

    #[derive(Default)]
    struct Base {
        accounts: AddressMap<AccountInfo>,
        storage: U256Map<U256>,
    }

    impl DatabaseRef for Base {
        type Error = ProviderError;
        fn basic_ref(&self, address: Address) -> ProviderResult<Option<AccountInfo>> {
            Ok(self.accounts.get(&address).cloned())
        }
        fn storage_ref(&self, _: Address, index: U256) -> ProviderResult<U256> {
            Ok(self.storage.get(&index).copied().unwrap_or_default())
        }
        fn code_by_hash_ref(&self, _: B256) -> ProviderResult<Bytecode> {
            Ok(Bytecode::default())
        }
        fn block_hash_ref(&self, _: u64) -> ProviderResult<B256> {
            Ok(B256::repeat_byte(9))
        }
    }

    fn base() -> Base {
        Base {
            accounts: [(
                ADDRESS,
                AccountInfo { balance: U256::from(10), nonce: 4, ..Default::default() },
            )]
            .into_iter()
            .collect(),
            storage: [(U256::from(1), U256::from(100)), (U256::from(2), U256::from(200))]
                .into_iter()
                .collect(),
        }
    }

    fn slot(key: u64, value: u64) -> SlotChanges {
        SlotChanges::new(
            U256::from(key),
            vec![StorageChange::new(BlockAccessIndex::new(1), U256::from(value))],
        )
    }

    #[test]
    fn sparse_final_writes_preserve_unchanged_fields_and_slots() {
        let base = base();
        let bal = Bal::from(vec![AccountChanges::new(ADDRESS)
            .with_balance_change(BalanceChange::new(BlockAccessIndex::new(1), U256::from(11)))
            .with_balance_change(BalanceChange::new(BlockAccessIndex::new(2), U256::from(12)))
            .with_storage_change(slot(1, 0))]);
        let (overlay, hashed) = BalParentState::derive(&base, &Header::default(), &bal).unwrap();
        let db = BalParentDatabase { base: &base, overlay: Some(&overlay) };
        let info = db.basic_ref(ADDRESS).unwrap().unwrap();
        assert_eq!(info.balance, U256::from(12));
        assert_eq!(info.nonce, 4);
        assert_eq!(db.storage_ref(ADDRESS, U256::from(1)).unwrap(), U256::ZERO);
        assert_eq!(db.storage_ref(ADDRESS, U256::from(2)).unwrap(), U256::from(200));
        assert_eq!(hashed.accounts[&keccak256(ADDRESS)].unwrap().balance, U256::from(12));
        assert_eq!(
            hashed.storages[&keccak256(ADDRESS)].storage[&keccak256(B256::from(U256::from(1)))],
            U256::ZERO
        );
    }

    #[test]
    fn read_only_does_not_create_or_delete_accounts() {
        let mut base = Base::default();
        base.accounts.insert(ADDRESS, AccountInfo::default());
        let absent = Address::repeat_byte(2);
        let bal = Bal::from(vec![
            AccountChanges::new(ADDRESS).with_storage_read(U256::ZERO),
            AccountChanges::new(absent),
        ]);
        let (overlay, hashed) = BalParentState::derive(&base, &Header::default(), &bal).unwrap();
        let db = BalParentDatabase { base, overlay: Some(&overlay) };
        assert_eq!(db.basic_ref(ADDRESS).unwrap(), Some(AccountInfo::default()));
        assert_eq!(db.basic_ref(absent).unwrap(), None);
        assert!(hashed.accounts.is_empty());
        assert!(hashed.storages.is_empty());
    }

    #[test]
    fn deleted_account_never_reads_old_storage() {
        let base = base();
        let bal = Bal::from(vec![AccountChanges::new(ADDRESS)
            .with_balance_change(BalanceChange::new(BlockAccessIndex::new(1), U256::ZERO))
            .with_nonce_change(NonceChange::new(BlockAccessIndex::new(1), 0))
            .with_code_change(CodeChange::new(BlockAccessIndex::new(1), Bytes::new()))]);
        let (overlay, hashed) = BalParentState::derive(&base, &Header::default(), &bal).unwrap();
        let db = BalParentDatabase { base, overlay: Some(&overlay) };
        assert_eq!(db.basic_ref(ADDRESS).unwrap(), None);
        assert_eq!(db.storage_ref(ADDRESS, U256::from(2)).unwrap(), U256::ZERO);
        assert_eq!(hashed.accounts[&keccak256(ADDRESS)], None);
    }

    #[test]
    fn storage_only_and_new_code_are_visible() {
        let base = base();
        let created = Address::repeat_byte(2);
        let code = bytes!("6001600055");
        let bal = Bal::from(vec![
            AccountChanges::new(ADDRESS).with_storage_change(slot(1, 50)),
            AccountChanges::new(created)
                .with_nonce_change(NonceChange::new(BlockAccessIndex::new(1), 1))
                .with_code_change(CodeChange::new(BlockAccessIndex::new(1), code.clone())),
        ]);
        let (overlay, _) = BalParentState::derive(&base, &Header::default(), &bal).unwrap();
        let db = BalParentDatabase { base: &base, overlay: Some(&overlay) };
        assert_eq!(db.basic_ref(ADDRESS).unwrap(), base.basic_ref(ADDRESS).unwrap());
        assert_eq!(db.storage_ref(ADDRESS, U256::from(1)).unwrap(), U256::from(50));
        let info = db.basic_ref(created).unwrap().unwrap();
        assert_eq!(info.nonce, 1);
        assert_eq!(info.code_hash, keccak256(&code));
        assert_eq!(db.code_by_hash_ref(info.code_hash).unwrap().original_bytes(), code);
    }

    #[test]
    fn blockhash_uses_candidate_branch_and_hides_future() {
        let header = Header { number: 10, ..Default::default() };
        let (overlay, _) =
            BalParentState::derive(&Base::default(), &header, &Bal::default()).unwrap();
        let db = BalParentDatabase { base: Base::default(), overlay: Some(&overlay) };
        assert_eq!(db.block_hash_ref(10).unwrap(), header.hash_slow());
        assert_eq!(db.block_hash_ref(9).unwrap(), B256::repeat_byte(9));
        assert_eq!(db.block_hash_ref(11).unwrap(), B256::ZERO);
    }

    #[test]
    fn child_writes_do_not_modify_parent_or_sibling() {
        let base = base();
        let bal = Bal::from(vec![AccountChanges::new(ADDRESS).with_storage_change(slot(1, 50))]);
        let (overlay, _) = BalParentState::derive(&base, &Header::default(), &bal).unwrap();
        let db = BalParentDatabase { base: &base, overlay: Some(&overlay) };
        let mut child = CacheDB::new(&db);
        let mut sibling = CacheDB::new(&db);
        child.insert_account_storage(ADDRESS, U256::from(1), U256::from(99)).unwrap();
        child.insert_account_info(ADDRESS, AccountInfo { nonce: 100, ..Default::default() });
        assert_eq!(child.storage(ADDRESS, U256::from(1)).unwrap(), U256::from(99));
        assert_eq!(sibling.storage(ADDRESS, U256::from(1)).unwrap(), U256::from(50));
        assert_eq!(db.storage_ref(ADDRESS, U256::from(1)).unwrap(), U256::from(50));
        assert_eq!(base.storage_ref(ADDRESS, U256::from(1)).unwrap(), U256::from(100));
        assert_eq!(sibling.basic(ADDRESS).unwrap().unwrap().nonce, 4);
    }

    #[test]
    fn disabled_overlay_delegates_reads() {
        let db = BalParentDatabase { base: base(), overlay: None };
        assert_eq!(db.basic_ref(ADDRESS).unwrap().unwrap().nonce, 4);
        assert_eq!(db.storage_ref(ADDRESS, U256::from(1)).unwrap(), U256::from(100));
        assert_eq!(db.code_by_hash_ref(B256::ZERO).unwrap(), Bytecode::default());
        assert_eq!(db.block_hash_ref(100).unwrap(), B256::repeat_byte(9));
    }
}
