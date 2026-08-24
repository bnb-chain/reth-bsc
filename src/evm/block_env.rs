use alloy_evm::env::BlockEnvironment;
use alloy_primitives::{Address, B256, U256};
use revm::context::{Block, BlockEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use std::ops::{Deref, DerefMut};

/// BSC block environment: revm's [`BlockEnv`] plus the sub-second millisecond
/// remainder of the block timestamp (BEP-520, decoded from the header's
/// `mix_hash` tail).
///
/// Only the *remainder* is stored — never an absolute millisecond value. The
/// millisecond timestamp consumed by the BEP-706 precompile (`0x70`, Jenner) is
/// always computed live as `timestamp * 1000 + milli_remainder`, so code paths
/// that mutate the inner `timestamp` directly (block overrides,
/// `debug_traceCallMany`'s per-bundle bump, `eth_callBundle`) can never leave a
/// stale millisecond value behind.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BscBlockEnv {
    /// The standard revm block environment.
    pub inner: BlockEnv,
    /// Millisecond remainder of the block timestamp (`0..1000` on any header
    /// that passed consensus validation; `0` for pre-Lorentz headers and for
    /// constructors that have no millisecond source).
    pub milli_remainder: u64,
}

impl BscBlockEnv {
    /// Creates a new [`BscBlockEnv`] from the standard env and the millisecond
    /// remainder.
    pub const fn new(inner: BlockEnv, milli_remainder: u64) -> Self {
        Self { inner, milli_remainder }
    }

    /// The block's millisecond timestamp (BEP-520): computed live from the
    /// *current* seconds value so direct `timestamp` mutations are reflected.
    ///
    /// Saturates instead of overflowing for out-of-domain timestamps
    /// (`timestamp > u64::MAX / 1000` cannot appear on a real header).
    pub fn milli_timestamp(&self) -> u64 {
        self.inner
            .timestamp
            .saturating_to::<u64>()
            .saturating_mul(1000)
            .saturating_add(self.milli_remainder)
    }
}

impl From<BlockEnv> for BscBlockEnv {
    fn from(inner: BlockEnv) -> Self {
        Self { inner, milli_remainder: 0 }
    }
}

impl Deref for BscBlockEnv {
    type Target = BlockEnv;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for BscBlockEnv {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Block for BscBlockEnv {
    fn number(&self) -> U256 {
        self.inner.number()
    }

    fn beneficiary(&self) -> Address {
        self.inner.beneficiary()
    }

    fn timestamp(&self) -> U256 {
        self.inner.timestamp()
    }

    fn gas_limit(&self) -> u64 {
        self.inner.gas_limit()
    }

    fn basefee(&self) -> u64 {
        self.inner.basefee()
    }

    fn difficulty(&self) -> U256 {
        self.inner.difficulty()
    }

    fn prevrandao(&self) -> Option<B256> {
        self.inner.prevrandao()
    }

    fn blob_excess_gas_and_price(&self) -> Option<BlobExcessGasAndPrice> {
        self.inner.blob_excess_gas_and_price()
    }

    fn slot_num(&self) -> u64 {
        self.inner.slot_num()
    }
}

impl BlockEnvironment for BscBlockEnv {
    fn inner_mut(&mut self) -> &mut BlockEnv {
        &mut self.inner
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(secs: u64, remainder: u64) -> BscBlockEnv {
        BscBlockEnv::new(BlockEnv { timestamp: U256::from(secs), ..Default::default() }, remainder)
    }

    #[test]
    fn test_milli_timestamp_is_computed_live() {
        let mut e = env(1_790_000_000, 750);
        assert_eq!(e.milli_timestamp(), 1_790_000_000_750);

        // Stale-immunity: mutating the inner timestamp directly (the generic
        // `inner_mut()` path used by block overrides and the traceCallMany /
        // callBundle bumps) is reflected without touching the remainder.
        e.inner_mut().timestamp = U256::from(1_800_000_000u64);
        assert_eq!(e.milli_timestamp(), 1_800_000_000_750);
    }

    #[test]
    fn test_zero_remainder_defaults_to_second_precision() {
        // Structural equivalent of go-bsc's `ts == 0 -> Time*1000` fallback:
        // an unfilled remainder yields the second-precision value, never a
        // near-1970 garbage number.
        assert_eq!(env(1_790_000_000, 0).milli_timestamp(), 1_790_000_000_000);
        assert_eq!(BscBlockEnv::from(BlockEnv::default()).milli_remainder, 0);
    }

    #[test]
    fn test_block_trait_delegates_to_inner() {
        let e = env(1_790_000_000, 1);
        assert_eq!(Block::timestamp(&e), U256::from(1_790_000_000u64));
        assert_eq!(Block::gas_limit(&e), e.inner.gas_limit);
        assert_eq!(Block::prevrandao(&e), e.inner.prevrandao);
    }
}
