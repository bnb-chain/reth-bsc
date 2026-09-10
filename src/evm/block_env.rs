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
    /// The multiply/add **wraps** on overflow, matching go-bsc's plain `uint64`
    /// arithmetic (`Header.MilliTimestamp()` / `BlockOverrides.Apply`): an
    /// out-of-domain seconds value (e.g. `blockOverrides.time = u64::MAX`, an
    /// RPC-reachable input on both clients) must produce the same wrapped value
    /// as geth. Only the `U256 -> u64` narrowing saturates — go's `Time` *is* a
    /// `u64`, so a wider value is unrepresentable there and has no parity
    /// baseline.
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

/// Upper bound (exclusive) for the value accepted as the `prevRandao` block
/// override: on BSC the underlying header field (`mixHash`) carries the
/// sub-second millisecond remainder of the block timestamp (BEP-520) instead of
/// a randomness beacon. Consensus enforces the same bound on real headers
/// (`MilliTimestamp() / 1000` must equal `Time`). Mirrors go-bsc's
/// `MaxBSCMilliRemainder`.
pub const MAX_BSC_MILLI_REMAINDER: u64 = 1000;

/// Interprets a `prevRandao` block override the BSC way: the 32-byte value is
/// the millisecond remainder and must be below [`MAX_BSC_MILLI_REMAINDER`],
/// exactly like the `mixHash` of a real BSC header. Mirrors go-bsc's
/// `BSCMilliRemainder`: the full 32 bytes are parsed and anything that does not
/// fit in a `u64` is rejected before truncating, so a value with non-zero high
/// bytes can never be silently accepted via its low 64 bits.
pub fn bsc_milli_remainder(prev_randao: &B256) -> Result<u64, String> {
    let v = U256::from_be_bytes(prev_randao.0);
    if v > U256::from(u64::MAX) {
        return Err(format!(
            "block override \"prevRandao\" on BSC carries the millisecond remainder of the \
             block timestamp (BEP-520/BEP-706) and must be less than {MAX_BSC_MILLI_REMAINDER}, \
             got {v}"
        ));
    }
    let ms = v.to::<u64>();
    if ms >= MAX_BSC_MILLI_REMAINDER {
        return Err(format!(
            "block override \"prevRandao\" on BSC carries the millisecond remainder of the \
             block timestamp (BEP-520/BEP-706) and must be less than {MAX_BSC_MILLI_REMAINDER}, \
             got {ms}"
        ));
    }
    Ok(ms)
}

/// BSC semantics for RPC block overrides (go-bsc `BlockOverrides.Apply` parity,
/// [#3792](https://github.com/bnb-chain/bsc/pull/3792)). Invoked by reth right
/// after `apply_block_overrides` wrote the standard fields into `inner`, so the
/// seconds are already the post-override value:
///
/// - `time` override → the sub-second remainder resets to `.000`
///   (`BlockOverrides` has no millisecond field; a simultaneous `prevRandao`
///   override supplies the remainder below);
/// - `prevRandao` override → the value **is** the millisecond remainder:
///   validated (`< 1000`, mirroring the consensus rule on real headers,
///   rejected otherwise so callers cannot pass arbitrary random values) and
///   assembled into the millisecond timestamp served by the BEP-706 precompile
///   (`time * 1000 + prevRandao`). `prevrandao` itself (the `0x44` opcode
///   view) was already replaced by `apply_block_overrides` and stays replaced.
///
/// Each override applies independently: pass one and only it takes effect,
/// pass both and both do. The validation is client-default behavior — it is
/// not gated on Jenner activation, matching go-bsc.
impl reth_rpc_eth_types::BlockOverridesExt for BscBlockEnv {
    fn apply_block_overrides_ext(
        &mut self,
        overrides: &alloy_rpc_types_eth::BlockOverrides,
    ) -> Result<(), String> {
        // Branch order mirrors go-bsc's `Apply`: `time` first, `prevRandao`
        // second, so a combined override ends with the overridden remainder.
        if overrides.time.is_some() {
            self.milli_remainder = 0;
        }
        if let Some(prev_randao) = &overrides.random {
            self.milli_remainder = bsc_milli_remainder(prev_randao)?;
        }
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

    // ---- BSC block-override semantics (go-bsc C8/C8a parity) ----
    //
    // These drive the exact pipeline reth's `prepare_call_env` / `simulate_v1`
    // run: alloy's `apply_block_overrides` writes the standard fields into the
    // inner env, then the `BlockOverridesExt` hook applies the BSC millisecond
    // semantics. RPC end-to-end coverage of the same scenarios (eth_call /
    // eth_estimateGas / eth_callMany / eth_simulateV1) is exercised on a live
    // devnet (E2).

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
        // Scenario 1: `time` alone — 0x70 must serve NewTime*1000 (remainder
        // resets to .000), and the 0x44 view is untouched.
        let mut e = env(SECS, REMAINDER);
        let prevrandao_before = e.inner.prevrandao;
        apply(&mut e, BlockOverrides { time: Some(SECS + 1000), ..Default::default() }).unwrap();
        assert_eq!(e.milli_timestamp(), (SECS + 1000) * 1000);
        assert_eq!(e.milli_remainder, 0);
        assert_eq!(e.inner.prevrandao, prevrandao_before, "0x44 view must not move");
    }

    #[test]
    fn test_prev_randao_override_sets_remainder_on_original_seconds() {
        // Scenario 4: `prevRandao` alone — remainder replaced, seconds kept.
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides { random: Some(randao(123)), ..Default::default() })
            .unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000 + 123);
        // `Random` is still replaced (0x44 serves the override), go parity.
        assert_eq!(e.inner.prevrandao, Some(randao(123)));
    }

    #[test]
    fn test_combined_override_assembles_both() {
        // Scenario 3: `time + prevRandao` — assembled: seconds from time,
        // remainder from prevRandao, both views exact.
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
        // Scenario 6: >= 1000 must be rejected with the go-parity message.
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
        // Scenario 6a: high 24 bytes non-zero with valid low 8 bytes must be
        // rejected too (go: big.Int -> IsUint64 interception) — an
        // implementation that truncates to the low 64 bits would wrongly
        // accept 0x…0001_0000_0000_0000_007b as 123.
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
    fn test_wrapping_matches_geth_u64_arithmetic() {
        // go-bsc computes `Time*1000 + remainder` with plain `uint64` maths, so
        // an extreme-but-RPC-reachable `blockOverrides.time = u64::MAX` wraps.
        // (u64::MAX * 1000) mod 2^64 == 2^64 - 1000; + 123 remainder.
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
        // Nothing passed, nothing overridden: an untouched env keeps the real
        // header's remainder (go semantics: absent fields fall back to the
        // block's own values).
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides::default()).unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000 + REMAINDER);
    }

    #[test]
    fn test_zero_prev_randao_override_is_a_valid_remainder() {
        // simulateV1's default zeroed prevrandao (and an explicit zero
        // override) is the legal `.000` remainder, not an error.
        let mut e = env(SECS, REMAINDER);
        apply(&mut e, BlockOverrides { random: Some(B256::ZERO), ..Default::default() }).unwrap();
        assert_eq!(e.milli_timestamp(), SECS * 1000);
    }

    #[test]
    fn test_validation_is_not_gated_on_activation() {
        // go C8a: the < 1000 check is client-default behavior, independent of
        // fork state — the hook itself never consults the chain spec, which
        // this pins structurally (no spec is even reachable from here).
        let mut e = env(0, 0);
        assert!(apply(&mut e, BlockOverrides { random: Some(randao(2000)), ..Default::default() })
            .is_err());
    }
}
