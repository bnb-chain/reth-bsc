use revm::precompile::PrecompileError;

/// BSC specific precompile errors.
#[derive(Debug, PartialEq)]
pub enum BscPrecompileError {
    /// The cometbft validation input is invalid.
    InvalidInput,
    /// The cometbft apply block failed.
    CometBftApplyBlockFailed,
    /// The cometbft consensus state encoding failed.
    CometBftEncodeConsensusStateFailed,
    /// The double sign invalid evidence.
    DoubleSignInvalidEvidence,
}

impl From<BscPrecompileError> for PrecompileError {
    fn from(error: BscPrecompileError) -> Self {
        match error {
            BscPrecompileError::InvalidInput => PrecompileError::Other("invalid input".into()),
            BscPrecompileError::CometBftApplyBlockFailed => {
                PrecompileError::Other("apply block failed".into())
            }
            BscPrecompileError::CometBftEncodeConsensusStateFailed => {
                PrecompileError::Other("encode consensus state failed".into())
            }
            BscPrecompileError::DoubleSignInvalidEvidence => {
                PrecompileError::Other("double sign invalid evidence".into())
            }
        }
    }
}
