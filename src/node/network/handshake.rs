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
use tracing::{info, warn, error};

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
        info!("BSC handshake: Starting upgrade_status negotiation, eth_version: {:?}", negotiated_status.version);
        
        if negotiated_status.version > EthVersion::Eth66 {
            info!("BSC handshake: Eth version > Eth66, sending UpgradeStatus message");
            // Send upgrade status message allowing peer to broadcast transactions
            let upgrade_msg = UpgradeStatus {
                extension: UpgradeStatusExtension { disable_peer_tx_broadcast: false },
            };
            info!("BSC handshake: Sending UpgradeStatus message: {:?}", upgrade_msg);
            unauth.start_send_unpin(upgrade_msg.into_rlpx())?;
            info!("BSC handshake: UpgradeStatus message sent successfully");

            // Receive peer's upgrade status response
            info!("BSC handshake: Waiting for peer's UpgradeStatus response...");
            let their_msg = match unauth.next().await {
                Some(Ok(msg)) => {
                    info!("BSC handshake: Received message from peer, length: {}, first 20 bytes: {:02x?}", 
                          msg.len(), &msg[..msg.len().min(20)]);
                    msg
                },
                Some(Err(e)) => {
                    error!("BSC handshake: Error receiving peer response: {:?}", e);
                    return Err(EthStreamError::from(e));
                },
                None => {
                    warn!("BSC handshake: No response received from peer, disconnecting");
                    unauth.disconnect(DisconnectReason::DisconnectRequested).await?;
                    return Err(EthStreamError::EthHandshakeError(EthHandshakeError::NoResponse));
                }
            };

            // Decode their response
            info!("BSC handshake: Attempting to decode peer's UpgradeStatus response");
            match UpgradeStatus::decode(&mut their_msg.as_ref()).map_err(|e| {
                error!("BSC handshake: Decode error - msg_hex={}, error={:?}", hex::encode(&their_msg), e);
                EthStreamError::InvalidMessage(e.into())
            }) {
                Ok(upgrade_status) => {
                    info!("BSC handshake: Successfully decoded UpgradeStatus: {:?}", upgrade_status);
                    info!("BSC handshake: Handshake completed successfully");
                    return Ok(negotiated_status);
                }
                Err(e) => {
                    error!("BSC handshake: Failed to decode UpgradeStatus message, disconnecting with ProtocolBreach. Error: {:?}", e);
                    unauth.disconnect(DisconnectReason::ProtocolBreach).await?;
                    return Err(EthStreamError::EthHandshakeError(
                        EthHandshakeError::NonStatusMessageInHandshake,
                    ));
                }
            }
        } else {
            info!("BSC handshake: Eth version <= Eth66, skipping UpgradeStatus exchange");
        }

        info!("BSC handshake: upgrade_status negotiation completed successfully");
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
                info!("BSC handshake: Starting full BSC handshake process");
                info!("BSC handshake: Initiating standard Ethereum handshake first");
                let negotiated_status =
                    EthereumEthHandshake(unauth).eth_handshake(status, fork_filter).await?;
                info!("BSC handshake: Standard Ethereum handshake completed, negotiated_status: {:?}", negotiated_status);
                info!("BSC handshake: Starting BSC-specific upgrade_status phase");
                Self::upgrade_status(unauth, negotiated_status).await
            };
            match timeout(timeout_limit, fut).await {
                Ok(result) => {
                    info!("BSC handshake: Full handshake process completed within timeout");
                    result
                },
                Err(_) => {
                    error!("BSC handshake: Handshake timed out after {:?}", timeout_limit);
                    Err(EthStreamError::StreamTimeout)
                }
            }
        })
    }
}
