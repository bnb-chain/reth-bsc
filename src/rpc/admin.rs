//! BSC-specific `admin` RPC extension (BEP-675 BidBlock builder permission override).
//!
//! Ported from bnb-chain/bsc `eth/api_admin.go`'s `AdminAPI.SetBidBlockPermission`, which lets an
//! operator manually allow or revoke a builder's `mev_sendBidBlock` permission without waiting for
//! an automatic revoke window to expire (or restarting the node).

use alloy_primitives::Address;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;

/// BSC-specific `admin` namespace additions.
#[rpc(server, namespace = "admin")]
pub trait BscAdminApi {
    /// Manually allow or revoke a builder's `mev_sendBidBlock` permission (go-bsc
    /// `AdminAPI.SetBidBlockPermission`). `allowed = true` clears any active revoke;
    /// `allowed = false` revokes the builder until an operator re-allows it.
    #[method(name = "setBidBlockPermission")]
    async fn set_bid_block_permission(&self, builder: Address, allowed: bool) -> RpcResult<()>;
}

/// Implementation of [`BscAdminApiServer`].
#[derive(Debug, Default, Clone, Copy)]
pub struct BscAdminApiImpl;

impl BscAdminApiImpl {
    /// Create a new BSC admin API instance.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl BscAdminApiServer for BscAdminApiImpl {
    async fn set_bid_block_permission(&self, builder: Address, allowed: bool) -> RpcResult<()> {
        crate::shared::get_bid_block_permission_manager().set_allowed(builder, allowed);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn set_bid_block_permission_revokes_and_reallows() {
        // Unique builder so this test doesn't collide with the process-global permission manager
        // shared by other tests in this binary.
        let builder = Address::repeat_byte(0x90);
        let pm = crate::shared::get_bid_block_permission_manager();
        assert!(pm.is_allowed(builder), "builder should start allowed");

        let api = BscAdminApiImpl::new();
        api.set_bid_block_permission(builder, false).await.unwrap();
        assert!(!pm.is_allowed(builder), "operator revoke must take effect immediately");

        api.set_bid_block_permission(builder, true).await.unwrap();
        assert!(pm.is_allowed(builder), "operator re-allow must clear the revoke");
    }
}
