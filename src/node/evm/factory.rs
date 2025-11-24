use crate::{
    evm::{
        api::{BscContext, BscEvm},
        transaction::BscTxEnv,
    },
    hardforks::bsc::BscHardfork,
};
use reth_evm::{precompiles::PrecompilesMap, Database, EvmEnv, EvmFactory};
use revm::context::result::{EVMError, HaltReason};
use revm::inspector::NoOpInspector;
use reth_revm::Inspector;

/// Factory producing [`BscEvm`].
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscEvmFactory;

impl EvmFactory for BscEvmFactory {
    type Evm<DB: Database, I: Inspector<BscContext<DB>>> = BscEvm<DB, I>;
    type Context<DB: Database> = BscContext<DB>;
    type Tx = BscTxEnv;
    type Error<DBError: core::error::Error + Send + Sync + 'static> = EVMError<DBError>;
    type HaltReason = HaltReason;
    type Spec = BscHardfork;
    type Precompiles = PrecompilesMap;

    fn create_evm<DB: Database>(
        &self,
        db: DB,
        input: EvmEnv<BscHardfork>,
    ) -> Self::Evm<DB, NoOpInspector> {
        // Check if we're in a trace/debug context by examining the database type
        // CacheDB is used in trace scenarios where we need to replay transactions
        let type_name = std::any::type_name::<DB>();
        let is_trace = type_name.contains("CacheDB");
        
        BscEvm::new(input, db, NoOpInspector {}, false, is_trace)
    }

    fn create_evm_with_inspector<DB: Database, I: Inspector<Self::Context<DB>>>(
        &self,
        db: DB,
        input: EvmEnv<BscHardfork>,
        inspector: I,
    ) -> Self::Evm<DB, I> {
        BscEvm::new(input, db, inspector, true, true)
    }
}
