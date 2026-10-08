use alloy_eips::BlockNumberOrTag;
use jsonrpsee::{core::RpcResult, types::ErrorObject};
use reth_provider::BlockIdReader;

/// Resolve a canonical finality marker without silently substituting the current head.
pub(super) fn resolve_finality_block(
    provider: &impl BlockIdReader,
    tag: BlockNumberOrTag,
) -> RpcResult<u64> {
    provider
        .convert_block_number(tag)
        .map_err(|error| {
            ErrorObject::owned(
                -32603,
                format!("Failed to resolve {tag} block: {error}"),
                None::<()>,
            )
        })?
        .ok_or_else(|| {
            ErrorObject::owned(-32603, format!("{tag} block is not available"), None::<()>)
        })
}
