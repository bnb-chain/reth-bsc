//! BSC-specific `admin` RPC extensions: the BEP-675 BidBlock builder permission override, and
//! `admin_nodeInfo` with the `bsc` sub-protocol.
//!
//! Ported from bnb-chain/bsc `eth/api_admin.go`'s `AdminAPI.SetBidBlockPermission`, which lets an
//! operator manually allow or revoke a builder's `mev_sendBidBlock` permission without waiting for
//! an automatic revoke window to expire (or restarting the node).

use alloy_primitives::Address;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use reth::rpc::api::AdminApiServer;

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

/// `admin_nodeInfo` listing the `bsc` sub-protocol, which reth's version omits.
#[rpc(server, namespace = "admin")]
pub trait BscNodeInfoApi {
    /// go-bsc `AdminAPI.NodeInfo`: `protocols` also carries `bsc`, whose node info is empty.
    #[method(name = "nodeInfo")]
    async fn node_info(&self) -> RpcResult<serde_json::Value>;
}

/// Implementation of [`BscNodeInfoApiServer`] over reth's `admin` API.
#[derive(Debug, Clone)]
pub struct BscNodeInfoApiImpl<A>(pub A);

#[async_trait::async_trait]
impl<A: AdminApiServer> BscNodeInfoApiServer for BscNodeInfoApiImpl<A> {
    async fn node_info(&self) -> RpcResult<serde_json::Value> {
        let info = AdminApiServer::node_info(&self.0).await?;
        Ok(with_bsc_protocol(serde_json::to_value(info).expect("NodeInfo serializes")))
    }
}

fn with_bsc_protocol(mut info: serde_json::Value) -> serde_json::Value {
    info["protocols"]["bsc"] = serde_json::json!({});
    info
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

    #[test]
    fn node_info_lists_bsc_next_to_eth() {
        let info = with_bsc_protocol(serde_json::json!({ "protocols": { "eth": {} } }));
        assert_eq!(info["protocols"], serde_json::json!({ "eth": {}, "bsc": {} }));
    }
}
