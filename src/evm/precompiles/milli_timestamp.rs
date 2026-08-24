//! BEP-706: millisecond-precision block timestamp precompile (`0x70`, Jenner fork).
//!
//! Unlike every other BSC precompile, `0x70` reads block context (the millisecond
//! timestamp), so it is NOT part of the static fork-cumulative tables in
//! [`super`] — it is registered dynamically by `BscEvm::new` via
//! [`milli_timestamp_precompile`], which captures the per-block millisecond
//! remainder and reads the seconds live from the EVM's block env at call time.

use alloy_evm::precompiles::{DynPrecompile, PrecompileInput};
use alloy_primitives::{Address, U256};
use revm::context::Block;
use revm::precompile::{
    u64_to_address, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
};
use std::borrow::Cow;

/// Gas cost of the BEP-706 millisecond-timestamp precompile (BEP-706 §4.4).
pub(crate) const MILLI_TIMESTAMP_GAS: u64 = 20;

/// The precompile id reported to tracers and `eth_config` — matches go-bsc's
/// `Name() = "MILLI_TIMESTAMP"`.
const MILLI_TIMESTAMP_ID: PrecompileId = PrecompileId::Custom(Cow::Borrowed("MILLI_TIMESTAMP"));

/// Core BEP-706 semantics: charge a flat 20 gas, ignore the calldata entirely, and
/// return the block's millisecond timestamp as a left-padded 32-byte big-endian
/// integer.
///
/// `milli_timestamp == 0` falls back to `timestamp_secs * 1000` — the same defensive
/// fallback as go-bsc's `RunWithBlockContext` for block contexts built without a
/// millisecond source ("degraded but correct", never a near-1970 garbage value).
/// With [`crate::evm::block_env::BscBlockEnv`] computing the value as
/// `timestamp * 1000 + remainder` this branch is structurally unreachable for any
/// non-zero timestamp, but the go-parity behavior is kept (and pinned by tests).
fn run_milli_timestamp(
    milli_timestamp: u64,
    timestamp_secs: u64,
    _input: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    if MILLI_TIMESTAMP_GAS > gas_limit {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }

    let ts = if milli_timestamp == 0 { timestamp_secs.saturating_mul(1000) } else { milli_timestamp };
    let output = U256::from(ts).to_be_bytes::<32>();
    Ok(PrecompileOutput::new(MILLI_TIMESTAMP_GAS, output.to_vec().into(), reservoir))
}

/// Factory for the dynamically registered `0x70` precompile.
///
/// Returns the address and a **stateful** [`DynPrecompile`]
/// (`supports_caching() == false`): the output changes every block, so the engine's
/// `CachedPrecompile` wrapping (`map_cacheable_precompiles`) must never cache it.
/// The closure captures only the sub-second `milli_remainder`; the seconds are read
/// live from the EVM's block env at call time, so direct `timestamp` mutations
/// (block overrides, `debug_traceCallMany`, `eth_callBundle`) are always reflected.
pub(crate) fn milli_timestamp_precompile(milli_remainder: u64) -> (Address, DynPrecompile) {
    let address = u64_to_address(0x70);
    let precompile = DynPrecompile::new_stateful(MILLI_TIMESTAMP_ID, move |input: PrecompileInput<'_>| {
        let timestamp_secs = input.internals.block_env().timestamp().saturating_to::<u64>();
        let milli_timestamp = timestamp_secs.saturating_mul(1000).saturating_add(milli_remainder);
        let result =
            run_milli_timestamp(milli_timestamp, timestamp_secs, input.data, input.gas, input.reservoir);

        // Same bring-up diagnostics the fixed-address precompiles get from the
        // static tracing wrappers in `super` — the dynamic 0x70 is not covered by
        // `traced_wrapper_for_address`, so it logs from inside the closure.
        if let Some(ctx) = super::current_precompile_trace_context() {
            if super::should_trace_precompiles(&ctx) {
                super::log_precompile_call(
                    &ctx,
                    address,
                    MILLI_TIMESTAMP_ID,
                    input.gas,
                    input.data.len(),
                    &result,
                );
            }
        }

        result
    });
    (address, precompile)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_evm::precompiles::Precompile as _;

    fn output_bytes(res: &PrecompileResult) -> Vec<u8> {
        match res {
            Ok(out) => out.bytes.to_vec(),
            Err(e) => panic!("precompile failed: {e:?}"),
        }
    }

    /// Mirrors go-bsc `TestMilliTimestamp_ReturnsHeaderValue`: a known
    /// seconds+remainder pair returns exactly the millisecond value, big-endian,
    /// left-padded to 32 bytes.
    #[test]
    fn returns_millisecond_value_as_32_byte_big_endian() {
        let ms: u64 = 1_790_000_000_750;
        let res = run_milli_timestamp(ms, ms / 1000, &[], MILLI_TIMESTAMP_GAS, 0);
        let bytes = output_bytes(&res);
        assert_eq!(bytes.len(), 32);
        assert_eq!(U256::from_be_slice(&bytes), U256::from(ms));
        assert!(bytes[..24].iter().all(|b| *b == 0), "left-padded with zeros");
        assert_eq!(res.unwrap().gas_used, MILLI_TIMESTAMP_GAS);
    }

    /// Mirrors go-bsc `TestMilliTimestamp_IgnoresInput`: empty vs arbitrary garbage
    /// calldata produce byte-identical results (BEP-706 §4.2).
    #[test]
    fn ignores_calldata_entirely() {
        let ms: u64 = 1_790_000_000_001;
        let empty = run_milli_timestamp(ms, ms / 1000, &[], 100_000, 0).unwrap();
        let garbage =
            run_milli_timestamp(ms, ms / 1000, &[0xde, 0xad, 0xbe, 0xef, 0x00, 0x42], 100_000, 0)
                .unwrap();
        assert_eq!(empty, garbage);
    }

    /// Mirrors go-bsc `TestMilliTimestamp_GasCost`: exactly 20 gas — a 19-gas budget
    /// runs out, a 20-gas budget succeeds and consumes all of it.
    #[test]
    fn charges_exactly_twenty_gas() {
        let ms: u64 = 1_790_000_000_000;
        let oog = run_milli_timestamp(ms, ms / 1000, &[], MILLI_TIMESTAMP_GAS - 1, 7).unwrap();
        assert_eq!(oog.halt_reason(), Some(&PrecompileHalt::OutOfGas));

        let ok = run_milli_timestamp(ms, ms / 1000, &[], MILLI_TIMESTAMP_GAS, 0).unwrap();
        assert_eq!(ok.gas_used, MILLI_TIMESTAMP_GAS);
        assert!(ok.is_success());
    }

    /// Mirrors go-bsc `TestMilliTimestamp_ZeroMilliTimestampFallback`: a zero
    /// millisecond value degrades to `timestamp_secs * 1000`, never a bare 0.
    #[test]
    fn zero_millisecond_value_falls_back_to_second_precision() {
        let res = run_milli_timestamp(0, 1_780_000_000, &[], MILLI_TIMESTAMP_GAS, 0);
        assert_eq!(U256::from_be_slice(&output_bytes(&res)), U256::from(1_780_000_000_000u64));
    }

    /// Fixed-seed randomized coverage — the go-bsc `FuzzMilliTimestamp` equivalent
    /// (dedicated fuzz infra is deliberately not added; a deterministic seeded loop
    /// gives the same coverage in CI).
    ///
    /// Inputs stay inside the header-legal domain (`time <= u64::MAX / 1000`,
    /// `remainder < 1000` — `calculate_millisecond_timestamp` is `secs * 1000 + part`
    /// and consensus bounds the remainder, so out-of-domain values cannot appear on a
    /// real header and are not part of the tested behavior).
    #[test]
    fn randomized_output_depends_only_on_the_block_env() {
        use rand::{rngs::StdRng, Rng, SeedableRng};

        let mut rng = StdRng::seed_from_u64(0xB5C_706);
        let mut scratch = vec![0u8; 512];

        for i in 0..100_000u64 {
            let secs: u64 = rng.random_range(0..=u64::MAX / 1000);
            let remainder: u64 = rng.random_range(0..1000);
            let ms = secs * 1000 + remainder;

            // Random calldata (length and content).
            let len = rng.random_range(0..scratch.len());
            rng.fill(&mut scratch[..len]);

            let with_input =
                run_milli_timestamp(ms, secs, &scratch[..len], 1_000_000, 0).unwrap();
            let without_input = run_milli_timestamp(ms, secs, &[], 1_000_000, 0).unwrap();

            // Output is 32 bytes, only a function of the env, gas is flat 20.
            assert_eq!(with_input, without_input, "iteration {i}: input must be ignored");
            assert_eq!(with_input.bytes.len(), 32);
            assert_eq!(with_input.gas_used, MILLI_TIMESTAMP_GAS);
            let expected = if ms == 0 { secs.saturating_mul(1000) } else { ms };
            assert_eq!(U256::from_be_slice(&with_input.bytes), U256::from(expected));
        }
    }

    /// The factory must produce a *stateful* precompile — `supports_caching() ==
    /// false` — or the engine's `CachedPrecompile` wrapping would serve a stale
    /// cross-block value for `0x70`.
    #[test]
    fn factory_precompile_is_not_cacheable() {
        let (address, precompile) = milli_timestamp_precompile(750);
        assert_eq!(address, u64_to_address(0x70));
        assert_eq!(precompile.precompile_id(), &MILLI_TIMESTAMP_ID);
        assert!(
            !precompile.supports_caching(),
            "0x70's output changes every block and must never be cached"
        );
    }
}
