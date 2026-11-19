//! Read adapter for legacy test fixtures. Writes remain in the native EVM overlay.
use alloy_primitives::{Address, B256, U256};
use evm2::{
    bytecode::Bytecode,
    evm::{AccountInfo, Database},
};

pub(crate) struct LegacyDb<D>(pub D);
impl<D: revm::Database<Error: std::error::Error + Send + Sync + 'static>> Database for LegacyDb<D> {
    type Error = D::Error;
    fn get_account(&mut self, address: &Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.basic(*address).map(|info| info.map(account))
    }
    fn get_code_by_hash(&mut self, hash: &B256) -> Result<Bytecode, Self::Error> {
        self.0.code_by_hash(*hash).map(|code| Bytecode::new_legacy(code.original_bytes()))
    }
    fn get_storage(&mut self, address: &Address, key: &U256) -> Result<U256, Self::Error> {
        self.0.storage(*address, *key)
    }
    fn get_block_hash(&mut self, number: &U256) -> Result<B256, Self::Error> {
        self.0.block_hash(number.saturating_to())
    }
}

pub(crate) fn account(info: revm::state::AccountInfo) -> AccountInfo {
    AccountInfo {
        balance: info.balance,
        nonce: info.nonce,
        code_hash: info.code_hash,
        code: info.code.map(|code| Bytecode::new_legacy(code.original_bytes())),
        ..Default::default()
    }
}

pub(crate) fn pending(state: revm::state::EvmState) -> evm2::evm::PendingState {
    let mut pending = evm2::evm::PendingState::default();
    for (address, change) in state {
        let original = Some(account(change.original_info()));
        let current = (!change.is_selfdestructed()).then(|| account(change.info.clone()));
        pending.insert_account(address, original, current);
        for (key, slot) in change.storage {
            pending.insert_storage(address, key, slot.original_value(), slot.present_value());
        }
    }
    pending
}

pub(crate) fn update(state: revm::state::EvmState) -> reth_execution_types::StateUpdate {
    let mut block = reth_execution_types::BlockState::default();
    let mut update = reth_execution_types::StateUpdate::default();
    for change in pending(state).account_changes() {
        block.commit_account_changes(change, Some(&mut update));
    }
    update
}
