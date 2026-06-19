//! BEP-675 block-source tagging.
//!
//! Validators encode the winning MEV path and builder address into the block header's
//! `requests_hash`; locally-built blocks keep the empty requests hash, so callers can rely on
//! "untagged" meaning "local". Ported from bnb-chain/bsc `core/types/block_mev_info.go`.

use alloy_primitives::{Address, B256};

/// Offset of the version byte within the 32-byte tag.
///
/// Layout: `[0..11] = 0` (leading-zero sentinel), `[11] = version`, `[12..32] = builder` (20-byte
/// address). The offset is `B256::len() - Address::len() - 1 = 32 - 20 - 1`.
const BLOCK_MEV_INFO_VERSION_OFFSET: usize = 32 - 20 - 1;

/// Identifies which submission path produced a block. Stored in `header.requests_hash` at
/// [`BLOCK_MEV_INFO_VERSION_OFFSET`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMevInfoVersion {
    /// Block produced via the legacy `SendBid` path.
    Bid = 1,
    /// Block produced via the BEP-675 `SendBidBlock` path.
    BidBlock = 2,
}

/// Pack `(version, builder)` into a 32-byte tag suitable for `header.requests_hash`.
///
/// Local blocks must NOT use this encoding; they keep the default empty requests hash so callers
/// can rely on "untagged" == local.
pub fn encode_block_mev_info(version: BlockMevInfoVersion, builder: Address) -> B256 {
    let mut h = B256::ZERO;
    h[BLOCK_MEV_INFO_VERSION_OFFSET] = version as u8;
    h[BLOCK_MEV_INFO_VERSION_OFFSET + 1..].copy_from_slice(builder.as_slice());
    h
}

/// Recover the MEV source and builder from a tag.
///
/// Returns `None` when the block should be treated as local: a nonzero leading sentinel, an
/// unknown version, or a zero builder address.
pub fn decode_block_mev_info(h: B256) -> Option<(BlockMevInfoVersion, Address)> {
    if h[..BLOCK_MEV_INFO_VERSION_OFFSET].iter().any(|&b| b != 0) {
        return None;
    }

    let version = match h[BLOCK_MEV_INFO_VERSION_OFFSET] {
        1 => BlockMevInfoVersion::Bid,
        2 => BlockMevInfoVersion::BidBlock,
        _ => return None,
    };

    let builder = Address::from_slice(&h[BLOCK_MEV_INFO_VERSION_OFFSET + 1..]);
    if builder == Address::ZERO {
        return None;
    }

    Some((version, builder))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    const BUILDER: Address = address!("0xb32d0723583040f3a16d1380d1e6aa874cd1bdf7");

    #[test]
    fn round_trips_both_versions() {
        for version in [BlockMevInfoVersion::Bid, BlockMevInfoVersion::BidBlock] {
            let tag = encode_block_mev_info(version, BUILDER);
            assert_eq!(decode_block_mev_info(tag), Some((version, BUILDER)));
        }
    }

    #[test]
    fn encode_layout_matches_spec() {
        let tag = encode_block_mev_info(BlockMevInfoVersion::BidBlock, BUILDER);
        // Leading sentinel is all-zero.
        assert!(tag[..BLOCK_MEV_INFO_VERSION_OFFSET].iter().all(|&b| b == 0));
        // Version byte then the 20-byte builder address.
        assert_eq!(tag[BLOCK_MEV_INFO_VERSION_OFFSET], BlockMevInfoVersion::BidBlock as u8);
        assert_eq!(&tag[BLOCK_MEV_INFO_VERSION_OFFSET + 1..], BUILDER.as_slice());
    }

    #[test]
    fn untagged_hash_decodes_as_local() {
        // The empty requests hash (all zero) is how local blocks are left.
        assert_eq!(decode_block_mev_info(B256::ZERO), None);
    }

    #[test]
    fn rejects_nonzero_leading_sentinel() {
        let mut tag = encode_block_mev_info(BlockMevInfoVersion::Bid, BUILDER);
        tag[0] = 1;
        assert_eq!(decode_block_mev_info(tag), None);
    }

    #[test]
    fn rejects_unknown_version() {
        let mut tag = encode_block_mev_info(BlockMevInfoVersion::Bid, BUILDER);
        tag[BLOCK_MEV_INFO_VERSION_OFFSET] = 3;
        assert_eq!(decode_block_mev_info(tag), None);
    }

    #[test]
    fn rejects_zero_builder() {
        let tag = encode_block_mev_info(BlockMevInfoVersion::Bid, Address::ZERO);
        assert_eq!(decode_block_mev_info(tag), None);
    }
}
