use crate::evm::precompiles::cas20::is_cas20_precompile;
use alloy_evm::env::BlockEnvironment;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::state::StateOverride;
use revm::context::{Block, BlockEnv};
use revm::context_interface::block::BlobExcessGasAndPrice;
use std::ops::{Deref, DerefMut};

/// BSC block environment: revm's [`BlockEnv`] plus the sub-second millisecond
/// remainder of the block timestamp (BEP-520, decoded from the header's
/// `mix_hash` tail).
///
/// The BEP-706 precompile (`0x70`) derives milliseconds from the current seconds
/// and remainder, so direct timestamp mutations cannot leave a stale value.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BscBlockEnv {
    /// The standard revm block environment.
    pub inner: BlockEnv,
    /// Millisecond remainder of the block timestamp (`0..1000` on any header
    /// that passed consensus validation; `0` for pre-Lorentz headers and for
    /// constructors that have no millisecond source).
    pub milli_remainder: u64,
    /// RPC-only code overrides; never populated from a canonical block header.
    pub disabled_cas20: std::collections::BTreeSet<Address>,
    /// RPC-only logical value for opcode 0x44, independent of revm's fork-specific
    /// DIFFICULTY/PREVRANDAO representation. Kept separate from `inner` so EVM
    /// normalization cannot rewrite the fields of a simulated block header.
    pub opcode_44_override: Option<U256>,
}

impl BscBlockEnv {
    /// Creates a new [`BscBlockEnv`] from the standard env and the millisecond
    /// remainder.
    pub const fn new(inner: BlockEnv, milli_remainder: u64) -> Self {
        Self {
            inner,
            milli_remainder,
            disabled_cas20: std::collections::BTreeSet::new(),
            opcode_44_override: None,
        }
    }

    /// The block's millisecond timestamp (BEP-520): computed live from the
    /// *current* seconds value so direct `timestamp` mutations are reflected.
    ///
    /// Seconds saturate to `u64`; multiplication and addition wrap on overflow.
    pub fn milli_timestamp(&self) -> u64 {
        self.inner
            .timestamp
            .saturating_to::<u64>()
            .wrapping_mul(1000)
            .wrapping_add(self.milli_remainder)
    }
}

impl From<BlockEnv> for BscBlockEnv {
    fn from(inner: BlockEnv) -> Self {
        Self::new(inner, 0)
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
        self.opcode_44_override.unwrap_or_else(|| self.inner.difficulty())
    }

    fn prevrandao(&self) -> Option<B256> {
        self.opcode_44_override.map(Into::into).or_else(|| self.inner.prevrandao())
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

/// Upper bound (exclusive) for the value accepted as the `prevRandao` block
/// override: on BSC the underlying header field (`mixHash`) carries the
/// sub-second millisecond remainder of the block timestamp (BEP-520) instead of
/// a randomness beacon. Consensus enforces the same bound on real headers
/// (`MilliTimestamp() / 1000` must equal `Time`). Mirrors go-bsc's
/// `MaxBSCMilliRemainder`.
pub const MAX_BSC_MILLI_REMAINDER: u64 = 1000;

/// Interprets a `prevRandao` block override the BSC way: the 32-byte value is
/// the millisecond remainder and must be below [`MAX_BSC_MILLI_REMAINDER`].
/// The full 256-bit value is checked before narrowing, so non-zero high bytes
/// cannot be silently discarded.
pub fn bsc_milli_remainder(prev_randao: &B256) -> Result<u64, String> {
    let v = U256::from_be_bytes(prev_randao.0);
    if v >= U256::from(MAX_BSC_MILLI_REMAINDER) {
        return Err(format!(
            "block override \"prevRandao\" on BSC carries the millisecond remainder of the \
             block timestamp (BEP-520/BEP-706) and must be less than {MAX_BSC_MILLI_REMAINDER}, \
             got {v}"
        ));
    }
    Ok(v.to::<u64>())
}

/// Applies BSC semantics after alloy has written the standard block fields.
///
/// `time` resets the millisecond remainder; `prevRandao` replaces it after
/// validation, regardless of fork activation. Explicit `prevRandao` or
/// `difficulty` also sets the fork-independent opcode-0x44 view, with
/// `prevRandao` taking precedence. Raw header fields remain unchanged here.
impl reth_rpc_eth_types::BlockOverridesExt for BscBlockEnv {
    fn apply_block_overrides_ext(
        &mut self,
        overrides: &alloy_rpc_types_eth::BlockOverrides,
    ) -> Result<(), String> {
        if overrides.time.is_some() {
            self.milli_remainder = 0;
        }
        if let Some(prev_randao) = &overrides.random {
            self.milli_remainder = bsc_milli_remainder(prev_randao)?;
        }
        // Keep explicit zero distinct from an absent override during fork conversion.
        if let Some(value) =
            overrides.random.map(|v| U256::from_be_bytes(v.0)).or(overrides.difficulty)
        {
            self.opcode_44_override = Some(value);
        }
        // alloy-evm 0.34's `apply_block_overrides` drops `blobBaseFee`.
        if let Some(blob_base_fee) = overrides.blob_base_fee {
            let excess_blob_gas =
                self.inner.blob_excess_gas_and_price.map(|b| b.excess_blob_gas).unwrap_or_default();
            self.inner.blob_excess_gas_and_price = Some(BlobExcessGasAndPrice {
                excess_blob_gas,
                blob_gasprice: blob_base_fee.saturating_to(),
            });
        }
        Ok(())
    }

    fn apply_state_overrides_ext(&mut self, overrides: &StateOverride) -> Result<(), String> {
        self.disabled_cas20.extend(overrides.iter().filter_map(|(address, account)| {
            (account.code.is_some() && is_cas20_precompile(*address)).then_some(*address)
        }));
        Ok(())
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

        // Match the direct timestamp mutation used by RPC helpers.
        e.inner_mut().timestamp = U256::from(1_800_000_000u64);
        assert_eq!(e.milli_timestamp(), 1_800_000_000_750);
    }

    #[test]
    fn test_zero_remainder_defaults_to_second_precision() {
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

    use alloy_rpc_types_eth::BlockOverrides;
    use reth_rpc_eth_types::BlockOverridesExt;

    /// `apply_block_overrides` only needs `OverrideBlockHashes` from the db.
    struct NoopHashes;
    impl alloy_evm::overrides::OverrideBlockHashes for NoopHashes {
        fn override_block_hashes(
            &mut self,
            _hashes: std::collections::BTreeMap<u64, B256>,
        ) {
        }
    }

    /// Runs the standard apply + BSC hook, exactly in reth's call order.
    fn apply(e: &mut BscBlockEnv, overrides: BlockOverrides) -> Result<(), String> {
        alloy_evm::overrides::apply_block_overrides(
            overrides.clone(),
            &mut NoopHashes,
            e.inner_mut(),
        );
        e.apply_block_overrides_ext(&overrides)
    }

    const SECS: u64 = 1_790_000_000;
    const REMAINDER: u64 = 555;

    fn randao(ms: u64) -> B256 {
        B256::from(U256::from(ms))
    }

    #[test]
    fn test_time_override_resets_remainder() {
        let mut e = env(SECS, REMAINDER);
        let prevrandao_before = e.inner.prevrandao;
        apply(&mut e, BlockOverrides { time: Some(SECS + 1000), ..Default::default() }).unwrap();
        assert_eq!(e.milli_timestamp(), (SECS + 1000) * 1000);
        assert_eq!(e.milli_remainder, 0);
        assert_eq!(e.inner.prevrandao, prevrandao_before, "0x44 view must not move");
    }

    #[test]
    fn test_prev_randao_override_sets_remainder_on_original_seconds() {
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides { random: Some(randao(123)), ..Default::default() })
            .unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000 + 123);
        assert_eq!(e.inner.prevrandao, Some(randao(123)));
    }

    #[test]
    fn test_combined_override_assembles_both() {
        let mut e = env(SECS, REMAINDER);
        apply(
            &mut e,
            BlockOverrides {
                time: Some(SECS + 1000),
                random: Some(randao(123)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(e.milli_timestamp(), (SECS + 1000) * 1000 + 123);
        assert_eq!(e.inner.prevrandao, Some(randao(123)));
    }

    #[test]
    fn test_prev_randao_at_bound_is_rejected() {
        let mut e = env(SECS, REMAINDER);
        let err = apply(&mut e, BlockOverrides { random: Some(randao(1000)), ..Default::default() })
            .unwrap_err();
        assert!(
            err.contains("must be less than 1000, got 1000"),
            "unexpected message: {err}"
        );
    }

    #[test]
    fn test_prev_randao_with_high_bytes_is_rejected() {
        // Truncating before validation would incorrectly accept this as 123.
        let mut e = env(SECS, REMAINDER);
        let mut bytes = [0u8; 32];
        bytes[23] = 0x01; // 2^64
        bytes[31] = 0x7b; // low bits decode to 123 < 1000
        let err = apply(
            &mut e,
            BlockOverrides { random: Some(B256::from(bytes)), ..Default::default() },
        )
        .unwrap_err();
        assert!(err.contains("must be less than 1000"), "unexpected message: {err}");
        assert!(err.contains("18446744073709551739"), "must report the full value: {err}");
    }

    #[test]
    fn test_milli_timestamp_preserves_wrapping_arithmetic() {
        let mut e = env(SECS, REMAINDER);
        apply(
            &mut e,
            BlockOverrides {
                time: Some(u64::MAX),
                random: Some(randao(123)),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(e.milli_timestamp(), u64::MAX.wrapping_mul(1000).wrapping_add(123));
        assert_eq!(e.milli_timestamp(), 18_446_744_073_709_550_739);
    }

    #[test]
    fn test_no_overrides_keep_the_block_values() {
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides::default()).unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000 + REMAINDER);
    }

    #[test]
    fn test_zero_prev_randao_override_is_a_valid_remainder() {
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides { random: Some(B256::ZERO), ..Default::default() }).unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000);
    }

    #[test]
    fn test_validation_is_not_gated_on_activation() {
        // Validation also applies before any fork activation.
        let mut e = env(0, 0);
        assert!(apply(&mut e, BlockOverrides { random: Some(randao(2000)), ..Default::default() })
            .is_err());
    }
}
