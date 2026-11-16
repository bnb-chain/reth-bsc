//! BSC's eth/70+ status packet retains total difficulty for Parlia synchronization.

use alloy_chains::Chain;
use alloy_primitives::{B256, U256};
use alloy_rlp::{Decodable, Encodable, RlpDecodable, RlpEncodable};
use bytes::Bytes;
use reth_eth_wire::{errors::EthHandshakeError, message::MessageError, UnifiedStatus};
use reth_eth_wire_types::{EthMessageID, EthVersion};
use reth_ethereum_forks::{ForkFilter, ForkId};
use reth_primitives_traits::GotExpected;

/// Field order matches go-bsc's `eth.StatusPacket`, including TD before genesis.
#[derive(Debug, Clone, PartialEq, Eq, RlpEncodable, RlpDecodable)]
pub(super) struct BscStatus {
    version: EthVersion,
    chain: Chain,
    total_difficulty: U256,
    genesis: B256,
    forkid: ForkId,
    earliest: u64,
    latest: u64,
    blockhash: B256,
}

impl BscStatus {
    pub(super) fn encode_message(&self) -> Bytes {
        let mut out = Vec::with_capacity(1 + self.length());
        EthMessageID::Status.encode(&mut out);
        self.encode(&mut out);
        out.into()
    }

    pub(super) fn decode_message(mut input: &[u8]) -> Result<Self, MessageError> {
        let message_id = EthMessageID::decode(&mut input)?;
        if message_id != EthMessageID::Status {
            return Err(MessageError::ExpectedStatusMessage(message_id));
        }
        let status = Self::decode(&mut input)?;
        if !input.is_empty() {
            return Err(alloy_rlp::Error::Custom("trailing data after BSC status").into());
        }
        Ok(status)
    }

    pub(super) fn validate(
        &self,
        local: &Self,
        fork_filter: &ForkFilter,
    ) -> Result<(), EthHandshakeError> {
        if self.genesis != local.genesis {
            return Err(EthHandshakeError::MismatchedGenesis(
                GotExpected { got: self.genesis, expected: local.genesis }.into(),
            ));
        }
        if self.version != local.version {
            return Err(EthHandshakeError::MismatchedProtocolVersion(GotExpected {
                got: self.version,
                expected: local.version,
            }));
        }
        if self.chain != local.chain {
            return Err(EthHandshakeError::MismatchedChain(GotExpected {
                got: self.chain,
                expected: local.chain,
            }));
        }
        // Match go-bsc's validateTD bound, including for the new status format.
        if self.total_difficulty.bit_len() > 100 {
            return Err(EthHandshakeError::TotalDifficultyBitLenTooLarge {
                got: self.total_difficulty.bit_len(),
                maximum: 100,
            });
        }
        fork_filter.validate(self.forkid)?;
        if self.earliest > self.latest {
            return Err(EthHandshakeError::EarliestBlockGreaterThanLatestBlock {
                got: self.earliest,
                latest: self.latest,
            });
        }
        if self.blockhash.is_zero() {
            return Err(EthHandshakeError::BlockhashZero);
        }
        Ok(())
    }
}

impl From<UnifiedStatus> for BscStatus {
    fn from(status: UnifiedStatus) -> Self {
        Self {
            version: status.version,
            chain: status.chain,
            total_difficulty: status.total_difficulty.unwrap_or_default(),
            genesis: status.genesis,
            forkid: status.forkid,
            earliest: status.earliest_block.unwrap_or_default(),
            latest: status.latest_block.unwrap_or_default(),
            blockhash: status.blockhash,
        }
    }
}

impl From<BscStatus> for UnifiedStatus {
    fn from(status: BscStatus) -> Self {
        Self {
            version: status.version,
            chain: status.chain,
            total_difficulty: Some(status.total_difficulty),
            genesis: status.genesis,
            forkid: status.forkid,
            earliest_block: Some(status.earliest),
            latest_block: Some(status.latest),
            blockhash: status.blockhash,
        }
    }
}
