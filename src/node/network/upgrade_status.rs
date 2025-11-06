//! Implement BSC upgrade message which is required during handshake with other BSC clients, e.g.,
//! geth.
use alloy_rlp::{Decodable, Encodable};
use bytes::{Buf, BufMut, Bytes, BytesMut};

/// The message id for the upgrade status message, used in the BSC handshake.
const UPGRADE_STATUS_MESSAGE_ID: u8 = 0x0b;

/// UpdateStatus packet introduced in BSC to notify peers whether to broadcast transaction or not.
/// It is used during the p2p handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UpgradeStatus {
    /// Extension for support customized features for BSC.
    pub extension: UpgradeStatusExtension,
}

impl Encodable for UpgradeStatus {
    fn encode(&self, out: &mut dyn BufMut) {
        UPGRADE_STATUS_MESSAGE_ID.encode(out);
        vec![&self.extension].encode(out);
    }
}

impl Decodable for UpgradeStatus {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        let message_id = u8::decode(buf)?;
        if message_id != UPGRADE_STATUS_MESSAGE_ID {
            return Err(alloy_rlp::Error::Custom("Invalid message ID"));
        }
        
        // BSC sends: 0x0b (message id) followed by [[disable_peer_tx_broadcast]]
        // The remaining bytes should be the extension wrapped in an extra list
        let extension: Vec<UpgradeStatusExtension> = Decodable::decode(buf)?;
        if extension.len() != 1 {
            return Err(alloy_rlp::Error::Custom("Invalid extension length"));
        }
        Ok(Self { extension: extension[0] })
    }
}

impl UpgradeStatus {
    /// Encode the upgrade status message into RLPx bytes.
    pub fn into_rlpx(self) -> Bytes {
        let mut out = BytesMut::new();
        self.encode(&mut out);
        out.freeze()
    }
}

/// The extension to define whether to enable or disable the flag.
/// This flag currently is ignored, and will be supported later.
#[derive(Debug, Clone, PartialEq, Eq, Copy)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UpgradeStatusExtension {
    // TODO: support disable_peer_tx_broadcast flag
    /// To notify a peer to disable the broadcast of transactions or not.
    pub disable_peer_tx_broadcast: bool,
}

impl Encodable for UpgradeStatusExtension {
    fn encode(&self, out: &mut dyn BufMut) {
        // Encode as a list containing the boolean
        vec![self.disable_peer_tx_broadcast].encode(out);
    }
}

impl Decodable for UpgradeStatusExtension {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // if got empty extension, return false
        if buf[0] == 0x80 {
            buf.advance(1);
            return Ok(Self { disable_peer_tx_broadcast: false });
        }
        // First try `[bool]` format
        let vals = <Vec<bool>>::decode(buf)?;
        if vals.len() != 1 {
            return Err(alloy_rlp::Error::Custom("Invalid bool length"));
        }
        Ok(Self { disable_peer_tx_broadcast: vals[0] })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::hex;
    
    #[test]
    fn test_decode_bsc_upgrade_status() {
        // Raw wire message captured from a BSC peer.
        let cases = vec![
            ("0bc180", UpgradeStatus { extension: UpgradeStatusExtension { disable_peer_tx_broadcast: false } }),
            ("0bc2c180", UpgradeStatus { extension: UpgradeStatusExtension { disable_peer_tx_broadcast: false } }),
            ("0bc2c101", UpgradeStatus { extension: UpgradeStatusExtension { disable_peer_tx_broadcast: true } }),
        ];
        for (raw, expected) in cases {
            let raw = hex::decode(raw).unwrap();
            let mut slice = raw.as_slice();
            let decoded = UpgradeStatus::decode(&mut slice).expect("should decode");
            println!("decoded: {:?}", decoded);
            assert_eq!(expected, decoded);
            let mut enc = BytesMut::new();
            UpgradeStatus { extension: UpgradeStatusExtension { disable_peer_tx_broadcast: false } }.encode(&mut enc);
            println!("enc: {:x?}", enc.freeze());
        }
    }
}
