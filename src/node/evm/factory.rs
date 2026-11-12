use crate::{
    evm::precompiles::native::BscNativePrecompiles, system_contracts::is_invoke_system_contract,
};
use reth_bsc_evm::{env::BscEvmEnv, evm::BscEvm};
use reth_evm::DynDatabase;

/// Factory producing the native BSC EVM with its complete fork-specific precompile set.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscEvmFactory;

impl BscEvmFactory {
    pub fn create_evm<'a>(&self, db: impl DynDatabase + 'a, env: BscEvmEnv) -> BscEvm<'a> {
        let precompiles = BscNativePrecompiles::new(&env);
        BscEvm::new(env, db, precompiles, is_invoke_system_contract)
    }
}
