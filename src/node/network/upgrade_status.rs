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
        // IMPORTANT: `eth` messages are framed as:
        //   <1 byte message-id><RLP payload>
        //
        // For BSC's upgrade-status, the message-id is 0x0b (not part of the RLP payload).
        // The RLP payload follows geth-bsc's `UpgradeStatusPacket` format:
        //   RLP([ <raw RLP bytes of UpgradeStatusExtension> ])
        //
        // This ensures compatibility with geth-bsc which reads msg.Code == 0x0b and then RLP-decodes
        // the remaining payload into `UpgradeStatusPacket`.
        out.put_u8(UPGRADE_STATUS_MESSAGE_ID);

        // Encode extension as geth does: struct{ DisablePeerTxBroadcast bool } => RLP([bool])
        let ext_rlp = alloy_rlp::encode(&self.extension);

        // Encode `UpgradeStatusPacket` payload: a list containing the raw extension RLP as its
        // single element. This produces bytes like `c2 c1 80` or `c2 c1 01`.
        alloy_rlp::Header { list: true, payload_length: ext_rlp.len() }.encode(out);
        out.put_slice(&ext_rlp);
    }
}

impl Decodable for UpgradeStatus {
    fn decode(buf: &mut &[u8]) -> alloy_rlp::Result<Self> {
        // The message id is a raw single byte (NOT RLP encoded).
        let Some(&message_id) = buf.first() else {
            return Err(alloy_rlp::Error::InputTooShort);
        };
        buf.advance(1);
        if message_id != UPGRADE_STATUS_MESSAGE_ID {
            return Err(alloy_rlp::Error::Custom("Invalid message ID"));
        }

        // We accept two wire formats seen in the wild:
        //
        // 1) geth-bsc `UpgradeStatusPacket` payload: RLP([ <extension-rlp> ])
        //    full msg bytes examples: 0b c2 c1 80 / 0b c2 c1 01
        //
        // 2) legacy/raw extension payload: RLP(<extension>)
        //    full msg bytes examples: 0b c1 80 / 0b c1 01
        //
        // Try decoding raw extension first; if that fails, fall back to packet wrapper.
        {
            let mut tmp = *buf;
            if let Ok(ext) = UpgradeStatusExtension::decode(&mut tmp) {
                // If it fully consumed the payload, accept.
                if tmp.is_empty() {
                    *buf = tmp;
                    return Ok(Self { extension: ext });
                }
            }
        }

        // Fallback: decode packet wrapper and then decode the single extension item.
        let header = alloy_rlp::Header::decode(buf)?;
        if !header.list {
            return Err(alloy_rlp::Error::UnexpectedString);
        }
        if header.payload_length > buf.len() {
            return Err(alloy_rlp::Error::InputTooShort);
        }
        let mut payload = &buf[..header.payload_length];
        let extension = UpgradeStatusExtension::decode(&mut payload)?;
        if !payload.is_empty() {
            return Err(alloy_rlp::Error::Custom("Invalid upgrade status payload (trailing bytes)"));
        }
        buf.advance(header.payload_length);
        Ok(Self { extension })
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
