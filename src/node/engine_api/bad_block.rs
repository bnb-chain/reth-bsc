//! Live-sync bad-block diagnostics. The header's builder tag is self-declared, not proof.

use crate::{
    metrics::BscBadBlockMetrics,
    node::miner::block_mev_info::{decode_block_mev_info, BlockMevInfoVersion},
    BscBlock,
};
use alloy_primitives::{Address, B256};
use lru::LruCache;
use parking_lot::Mutex;
use reth_engine_tree::tree::error::{InsertBlockError, InsertBlockErrorKind};
use std::{num::NonZeroUsize, sync::LazyLock};

static METRICS: LazyLock<BscBadBlockMetrics> = LazyLock::new(BscBadBlockMetrics::default);
// Separate from revocation evidence: diagnostics also include unauthenticated bad headers.
static COUNTED: LazyLock<Mutex<LruCache<B256, ()>>> =
    LazyLock::new(|| Mutex::new(LruCache::new(NonZeroUsize::new(1_000).unwrap())));

fn bid_block_builder(requests_hash: Option<B256>) -> Option<Address> {
    let (version, builder) = decode_block_mev_info(requests_hash?)?;
    (version == BlockMevInfoVersion::BidBlock).then_some(builder)
}

pub(super) fn report(error: &InsertBlockError<BscBlock>) {
    // Internal/provider errors do not establish that the block is bad.
    let invalid = match error.kind() {
        InsertBlockErrorKind::Consensus(_) => true,
        InsertBlockErrorKind::Execution(error) => error.as_validation().is_some(),
        _ => false,
    };
    if !invalid {
        return;
    }
    let block = error.block();
    let header = block.header();
    let builder = bid_block_builder(header.requests_hash);
    if builder.is_some() && COUNTED.lock().put(block.hash(), ()).is_none() {
        METRICS.bad_bid_blocks_total.increment(1);
    }
    tracing::warn!(
        target: "bsc::bad_block",
        invalid_hash = %block.hash(),
        invalid_number = header.number,
        miner = %header.beneficiary,
        requests_hash = ?header.requests_hash,
        is_bid_block = builder.is_some(),
        builder = ?builder,
        validation_err = %error,
        "Invalid block on live sync"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::node::miner::block_mev_info::encode_block_mev_info;

    #[test]
    fn bad_block_only_recognizes_valid_bidblock_tags() {
        let builder = Address::with_last_byte(1);
        let tag = encode_block_mev_info(BlockMevInfoVersion::BidBlock, builder);
        assert_eq!(bid_block_builder(Some(tag)), Some(builder));
        assert_eq!(bid_block_builder(None), None);
        assert_eq!(bid_block_builder(Some(B256::ZERO)), None);
        assert_eq!(
            bid_block_builder(Some(encode_block_mev_info(BlockMevInfoVersion::Bid, builder))),
            None
        );
        assert_eq!(
            bid_block_builder(Some(encode_block_mev_info(
                BlockMevInfoVersion::BidBlock,
                Address::ZERO
            ))),
            None
        );
        let mut malformed = tag;
        malformed[0] = 1;
        assert_eq!(bid_block_builder(Some(malformed)), None);
    }
}
