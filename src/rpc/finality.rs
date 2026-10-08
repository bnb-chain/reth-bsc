use alloy_eips::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, types::ErrorObject};
use reth_provider::{BlockIdReader, ProviderError};

/// Resolve a canonical finality marker without silently substituting the current head.
pub(super) fn resolve_finality_block(
    provider: &impl BlockIdReader,
    tag: BlockNumberOrTag,
) -> RpcResult<u64> {
    match tag {
        BlockNumberOrTag::Finalized => provider
            .finalized_block_number()
            .and_then(|number| number.ok_or(ProviderError::FinalizedBlockNotFound)),
        BlockNumberOrTag::Safe => provider
            .safe_block_number()
            .and_then(|number| number.ok_or(ProviderError::SafeBlockNotFound)),
        _ => {
            return Err(ErrorObject::owned(
                -32602,
                "Expected safe or finalized block tag",
                None::<()>,
            ));
        }
    }
    .map_err(|error| {
        ErrorObject::owned(-32603, format!("Failed to resolve {tag} block: {error}"), None::<()>)
    })
}
