//! Test case result types

use reth_ethereum_primitives::Block;
use reth_primitives_traits::RecoveredBlock;
use std::path::PathBuf;

/// Blockchain test execution witness (placeholder for BSC).
#[derive(Debug, Clone, Default)]
pub struct ExecutionWitness {
    /// Parent block headers needed for BLOCKHASH.
    pub headers: Vec<alloy_primitives::Bytes>,
}

/// A single test case result.
#[derive(Debug)]
pub struct CaseResult {
    /// The path to the test case.
    pub path: PathBuf,
    /// The test name.
    pub name: String,
    /// The result of the test.
    pub result: Result<(), Error>,
}

/// Test error types.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An IO error occurred.
    #[error("IO error at {path}: {error}")]
    Io {
        /// The path to the file.
        path: PathBuf,
        /// The error.
        error: std::io::Error,
    },
    /// A deserialization error occurred.
    #[error("Could not deserialize {path}: {error}")]
    CouldNotDeserialize {
        /// The path to the file.
        path: PathBuf,
        /// The error.
        error: serde_json::Error,
    },
    /// An assertion error occurred.
    #[error("Assertion failed: {0}")]
    Assertion(String),
    /// The test was skipped.
    #[error("Test skipped")]
    Skipped,
    /// Block processing failed at a specific block number.
    #[error("Block processing failed at block {block_number}: {err}")]
    BlockProcessingFailed {
        /// The block number at which processing failed.
        block_number: u64,
        /// Partial execution witnesses collected before failure.
        partial_program_inputs: Vec<(RecoveredBlock<Block>, ExecutionWitness)>,
        /// The underlying error.
        #[source]
        err: Box<dyn std::error::Error + Send + Sync>,
    },
    /// Database error.
    #[error("Database error: {0}")]
    Database(#[from] reth_db_api::DatabaseError),
    /// Provider error.
    #[error("Provider error: {0}")]
    Provider(#[from] reth_provider::ProviderError),
}

impl Error {
    /// Create a block processing failure error.
    pub fn block_failed<E>(
        block_number: u64,
        partial_inputs: Vec<(RecoveredBlock<Block>, ExecutionWitness)>,
        err: E,
    ) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::BlockProcessingFailed {
            block_number,
            partial_program_inputs: partial_inputs,
            err: Box::new(err),
        }
    }
}
