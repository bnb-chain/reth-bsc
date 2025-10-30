use alloy_consensus::constants::ETH_TO_WEI;
use alloy_primitives::{Address, B256, address};
use reth_provider::ProviderError;

pub const SYSTEM_ADDRESS: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");
/// The reward percent to system
pub const SYSTEM_REWARD_PERCENT: usize = 4;
/// The max reward in system reward contract
pub const MAX_SYSTEM_REWARD: u128 = 100 * ETH_TO_WEI;

/// Errors that can occur in Parlia consensus
#[derive(Debug, thiserror::Error)]
pub enum ParliaConsensusErr {
    /// Error from the provider
    #[error(transparent)]
    Provider(#[from] ProviderError),
    /// Head block hash not found
    #[error("Head block hash not found")]
    HeadHashNotFound,
    /// Fork choice update error
    #[error("Fork choice update error: {0}")]
    ForkChoiceUpdateError(String),
    /// Unknown total difficulty
    #[error("Unknown total difficulty for block {0} at number {1}")]
    UnknownTotalDifficulty(B256, u64),
    /// Internal error
    #[error(transparent)]
    Internal(Box<dyn core::error::Error + Send + Sync>),
}

impl ParliaConsensusErr {
    /// Create a new internal error.
    pub fn internal<E: core::error::Error + Send + Sync + 'static>(e: E) -> Self {
        Self::Internal(Box::new(e))
    }
}

pub mod parlia;
