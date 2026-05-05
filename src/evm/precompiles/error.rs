use std::borrow::Cow;
use revm::precompile::PrecompileHalt;

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

impl From<BscPrecompileError> for PrecompileHalt {
    fn from(error: BscPrecompileError) -> Self {
        match error {
            BscPrecompileError::InvalidInput => PrecompileHalt::Other(Cow::Borrowed("invalid input")),
            BscPrecompileError::CometBftApplyBlockFailed => {
                PrecompileHalt::Other(Cow::Borrowed("apply block failed"))
            }
            BscPrecompileError::CometBftEncodeConsensusStateFailed => {
                PrecompileHalt::Other(Cow::Borrowed("encode consensus state failed"))
            }
            BscPrecompileError::DoubleSignInvalidEvidence => {
                PrecompileHalt::Other(Cow::Borrowed("double sign invalid evidence"))
            }
        }
    }
}
