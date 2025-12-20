use super::upgrade_status::{UpgradeStatus, UpgradeStatusExtension};
use alloy_rlp::Decodable;
use futures::SinkExt;
use reth_eth_wire::{
    errors::{EthHandshakeError, EthStreamError},
    handshake::{EthRlpxHandshake, EthereumEthHandshake, UnauthEth},
    UnifiedStatus,
};
use reth_eth_wire_types::{DisconnectReason, EthVersion};
use reth_ethereum_forks::ForkFilter;
use std::{future::Future, pin::Pin};
use tokio::time::{timeout, Duration};
use tokio_stream::StreamExt;
use tracing::debug;

#[derive(Debug, Default)]
/// The Binance Smart Chain (BSC) P2P handshake.
#[non_exhaustive]
pub struct BscHandshake;

impl BscHandshake {
    /// Negotiate the upgrade status message.
    pub async fn upgrade_status(
        unauth: &mut dyn UnauthEth,
        negotiated_status: UnifiedStatus,
    ) -> Result<UnifiedStatus, EthStreamError> {
        if negotiated_status.version > EthVersion::Eth66 {
            // Send upgrade status message. When EVN is enabled, we ask peers
            // to NOT broadcast transactions to us (disable peer tx broadcast).
            // This mirrors the BSC EVN behavior where validator/sentry nodes
            // avoid mempool flooding between EVN peers.
            let evn_enabled = crate::node::network::evn::is_evn_ready();
            let disable_tx_broadcast_forbidden = crate::node::network::evn::get_global_evn_config().map(|cfg| cfg.disable_tx_broadcast_forbidden).unwrap_or(false);
            let upgrade_msg = UpgradeStatus {
                extension: UpgradeStatusExtension { disable_peer_tx_broadcast: evn_enabled && !disable_tx_broadcast_forbidden },
            };
            tracing::debug!(target: "bsc_handshake", "Sending upgrade status message, EVN enabled: {}, disable tx broadcast forbidden: {}", evn_enabled, disable_tx_broadcast_forbidden);
            unauth.start_send_unpin(upgrade_msg.into_rlpx())?;

            // Receive peer's upgrade status response
            let their_msg = match unauth.next().await {
                Some(Ok(msg)) => msg,
                Some(Err(e)) => return Err(EthStreamError::from(e)),
                None => {
                    unauth.disconnect(DisconnectReason::DisconnectRequested).await?;
                    return Err(EthStreamError::EthHandshakeError(EthHandshakeError::NoResponse));
                }
            };

            // Decode their response
            match UpgradeStatus::decode(&mut their_msg.as_ref()).map_err(|e| {
                debug!("Decode error in BSC handshake: msg={their_msg:x}");
                EthStreamError::InvalidMessage(e.into())
            }) {
                Ok(their_status) => {
                    tracing::trace!(target: "bsc_handshake", "bsc handshake: upgrade status: {:?}", their_status);
                    // Successful handshake; log remote's EVN preference
                    // TODO: cannot get peer id here, need to add it to the upgrade status message.
                    if their_status.extension.disable_peer_tx_broadcast {
                        debug!(target: "bsc_handshake", "Peer requests: disable TX broadcast towards them (EVN)");
                    }
                    return Ok(negotiated_status);
                }
                Err(e) => {
                    tracing::trace!(target: "bsc_handshake", "bsc handshake: upgrade failed: {:?}", e);
                    unauth.disconnect(DisconnectReason::ProtocolBreach).await?;
                    return Err(EthStreamError::EthHandshakeError(
                        EthHandshakeError::NonStatusMessageInHandshake,
                    ));
                }
            }
        }

        Ok(negotiated_status)
    }
}

impl EthRlpxHandshake for BscHandshake {
    fn handshake<'a>(
        &'a self,
        unauth: &'a mut dyn UnauthEth,
        status: UnifiedStatus,
        fork_filter: ForkFilter,
        timeout_limit: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<UnifiedStatus, EthStreamError>> + 'a + Send>> {
        Box::pin(async move {
            let fut = async {
                let negotiated_status =
                    EthereumEthHandshake(unauth).eth_handshake(status, fork_filter).await?;
                Self::upgrade_status(unauth, negotiated_status).await
            };
            timeout(timeout_limit, fut).await.map_err(|_| EthStreamError::StreamTimeout)?
        })
    }
}
