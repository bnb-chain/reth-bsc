//! RPC-handler-level tests for the BSC block-override semantics (BEP-706 phase 2,
//! go-bsc `api_jenner_test.go` parity).
//!
//! These go through the real `EthApiServer` handlers (`eth_call`,
//! `eth_estimateGas`, `eth_callMany`, `eth_simulateV1`) over a mock provider, so
//! the whole reth path — `prepare_call_env` / per-simulated-block override
//! application, alloy's `apply_block_overrides`, and the `BlockOverridesExt`
//! hook on [`crate::evm::block_env::BscBlockEnv`] — is exercised, not just the
//! hook in isolation. The same scenarios are verified against a live devnet in
//! E2. Two deliberate scope notes:
//! - the go-side "non-BSC chain unaffected" case lives in reth itself (the
//!   explicit no-op impl + tests on the stock `BlockEnv`);
//! - `eth_estimateGas` has no `blockOverrides` parameter on reth's RPC surface
//!   (upstream API difference, all chains alike), so its coverage here is the
//!   0x70-execution smoke test; the override semantics it would share come
//!   through the same `prepare_call_env` path `eth_call` exercises.

use crate::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    hardforks::bsc::BscHardfork,
    node::{evm::config::BscEvmConfig, primitives::BscPrimitives},
};
use alloy_primitives::{hex, Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload},
    state::StateOverride,
    BlockOverrides, Bundle, TransactionRequest,
};
use reth_chainspec::ForkCondition;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::MockEthProvider;
use reth_rpc_eth_api::EthApiServer;
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

/// Probe contract: returns `[prevrandao (0x44), staticcall(0x70) result]` as two
/// 32-byte words — the same probe the devnet E2 run uses.
///
/// `44 600052 6020 6020 6000 6000 6070 5a fa 50 6040 6000 f3`
const PROBE_CODE: &[u8] = &hex!("44600052602060206000600060705afa5060406000f3");
const PROBE_ADDR: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x0b, 0xee, 0xf0,
]);

/// Jenner activation used by the post-activation spec.
const JENNER_AT: u64 = 1_800_000_000;
/// The mock chain head: 100s after activation, with a 450ms remainder.
const HEAD_SECS: u64 = JENNER_AT + 100;
const HEAD_REMAINDER: u64 = 450;
const HEAD_MS: u64 = HEAD_SECS * 1000 + HEAD_REMAINDER;

fn spec(jenner: Option<u64>) -> Arc<BscChainSpec> {
    let mut cs = bsc_mainnet();
    if let Some(t) = jenner {
        cs.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(t));
    }
    Arc::new(BscChainSpec::from(cs))
}

fn head_header() -> alloy_consensus::Header {
    let mut mix = [0u8; 32];
    mix[24..32].copy_from_slice(&HEAD_REMAINDER.to_be_bytes());
    alloy_consensus::Header {
        // Past mainnet's London activation (31302048): every timestamp-fork
        // helper (incl. Jenner's) carries the is_london_active_at_block gate.
        number: 40_000_000,
        timestamp: HEAD_SECS,
        mix_hash: B256::new(mix),
        gas_limit: 140_000_000,
        base_fee_per_gas: Some(0),
        difficulty: U256::from(2u64),
        // Cancun is active at this timestamp — the EVM env requires these.
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        ..Default::default()
    }
}

/// Builds a full `EthApi` over a mock BSC provider with one canonical header.
/// A macro (not a fn) so the concrete deeply-generic `EthApi` type never has to
/// be named.
macro_rules! bsc_eth_api {
    ($spec:expr) => {{
        let spec = $spec;
        let provider = MockEthProvider::<BscPrimitives, _>::new()
            .with_chain_spec((*spec).clone());
        let header = head_header();
        let hash = header.hash_slow();
        provider.add_header(hash, header.clone());
        provider.add_block(
            hash,
            crate::node::primitives::BscBlock {
                header,
                body: crate::node::primitives::BscBlockBody::default(),
            },
        );
        reth::rpc::eth::core::EthApi::builder(
            provider,
            testing_pool(),
            NoopNetwork::default(),
            BscEvmConfig::new(spec),
        )
        .build()
    }};
}

fn probe_state() -> StateOverride {
    let mut so = StateOverride::default();
    so.insert(
        PROBE_ADDR,
        alloy_rpc_types_eth::state::AccountOverride {
            code: Some(Bytes::from_static(PROBE_CODE)),
            ..Default::default()
        },
    );
    so
}

fn probe_request() -> TransactionRequest {
    TransactionRequest { to: Some(PROBE_ADDR.into()), ..Default::default() }
}

fn randao(ms: u64) -> B256 {
    B256::from(U256::from(ms))
}

fn words(out: &Bytes) -> (u64, u64) {
    assert_eq!(out.len(), 64, "probe must return two words, got {out}");
    (U256::from_be_slice(&out[..32]).to::<u64>(), U256::from_be_slice(&out[32..64]).to::<u64>())
}

/// eth_call with the probe + optional block overrides against the post-Jenner api.
macro_rules! call_probe {
    ($api:expr, $overrides:expr) => {{
        let overrides: Option<BlockOverrides> = $overrides;
        $api.call(probe_request(), None, Some(probe_state()), overrides.map(Box::new)).await
    }};
}

#[tokio::test]
async fn eth_call_serves_the_head_millisecond_timestamp() {
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let out = call_probe!(api, None).expect("call should succeed");
    let (prevrandao, milli) = words(&out);
    assert_eq!(milli, HEAD_MS, "0x70 must serve the head's millisecond timestamp");
    // No prevRandao override: the 0x44 view is the difficulty-derived value.
    assert_eq!(prevrandao, 2, "0x44 keeps BSC's difficulty semantics");
}

#[tokio::test]
async fn eth_call_time_override_resets_the_remainder() {
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let t2 = HEAD_SECS + 10_000;
    let out = call_probe!(
        api,
        Some(BlockOverrides { time: Some(t2), ..Default::default() })
    )
    .expect("call should succeed");
    let (prevrandao, milli) = words(&out);
    assert_eq!(milli, t2 * 1000, "time override resets the remainder to .000");
    assert_eq!(prevrandao, 2, "an untouched 0x44 view must not move");
}

#[tokio::test]
async fn eth_call_time_override_wraps_like_geth() {
    // RPC-level regression for the wrapping arithmetic (review P2):
    // blockOverrides.time = u64::MAX is reachable on both clients and must
    // produce geth's wrapped uint64 value.
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let out = call_probe!(
        api,
        Some(BlockOverrides { time: Some(u64::MAX), ..Default::default() })
    )
    .expect("call should succeed");
    let (_, milli) = words(&out);
    assert_eq!(milli, u64::MAX.wrapping_mul(1000));
    assert_eq!(milli, 18_446_744_073_709_550_616);
}

#[tokio::test]
async fn eth_call_prev_randao_override_is_the_remainder() {
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let out = call_probe!(
        api,
        Some(BlockOverrides { random: Some(randao(55)), ..Default::default() })
    )
    .expect("call should succeed");
    let (prevrandao, milli) = words(&out);
    assert_eq!(milli, HEAD_SECS * 1000 + 55, "remainder replaced on the original seconds");
    assert_eq!(prevrandao, 55, "0x44 serves the override (go: Random is still replaced)");
}

#[tokio::test]
async fn eth_call_combined_override_assembles_both() {
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let t2 = HEAD_SECS + 10_000;
    let out = call_probe!(
        api,
        Some(BlockOverrides { time: Some(t2), random: Some(randao(123)), ..Default::default() })
    )
    .expect("call should succeed");
    let (prevrandao, milli) = words(&out);
    assert_eq!(milli, t2 * 1000 + 123);
    assert_eq!(prevrandao, 123);
}

#[tokio::test]
async fn eth_call_rejects_out_of_range_prev_randao() {
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let err = call_probe!(
        api,
        Some(BlockOverrides { random: Some(randao(1000)), ..Default::default() })
    )
    .expect_err("must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("must be less than 1000, got 1000"), "unexpected: {msg}");
}

#[tokio::test]
async fn eth_call_rejects_prev_randao_with_high_bytes() {
    // Review scenario 6a: high 24 bytes non-zero, low 8 bytes < 1000 — must be
    // rejected with the full value reported (go: big.Int -> IsUint64).
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let mut bytes = [0u8; 32];
    bytes[23] = 0x01; // 2^64
    bytes[31] = 0x7b; // low bits decode to 123
    let err = call_probe!(
        api,
        Some(BlockOverrides { random: Some(B256::from(bytes)), ..Default::default() })
    )
    .expect_err("must be rejected");
    let msg = err.to_string();
    assert!(msg.contains("must be less than 1000"), "unexpected: {msg}");
    assert!(msg.contains("18446744073709551739"), "must report the full value: {msg}");
}

#[tokio::test]
async fn pre_jenner_the_precompile_is_an_empty_account_but_validation_applies() {
    // Before activation 0x70 is a plain empty account (the probe's STATICCALL
    // returns no data, so the second word stays zero) …
    let api = bsc_eth_api!(spec(None));
    let out = call_probe!(
        api,
        Some(BlockOverrides { random: Some(randao(55)), ..Default::default() })
    )
    .expect("call should succeed");
    let (prevrandao, milli) = words(&out);
    assert_eq!(milli, 0, "0x70 must behave as an empty account before Jenner");
    assert_eq!(prevrandao, 55, "0x44 still serves the override");

    // … but the < 1000 validation is client-default behavior, independent of
    // fork state (go C8a).
    let err = call_probe!(
        api,
        Some(BlockOverrides { random: Some(randao(1000)), ..Default::default() })
    )
    .expect_err("validation applies before activation too");
    assert!(err.to_string().contains("must be less than 1000"));
}

#[tokio::test]
async fn eth_call_many_inherits_the_bsc_semantics() {
    // eth_callMany has no go-bsc counterpart; it inherits the semantics through
    // the shared `prepare_call_env` (documented as an inherited behavior
    // extension). Combined override assembles, invalid values reject.
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let t2 = HEAD_SECS + 10_000;
    let bundle = Bundle {
        transactions: vec![probe_request()],
        block_override: Some(BlockOverrides {
            time: Some(t2),
            random: Some(randao(123)),
            ..Default::default()
        }),
    };
    let res = api
        .call_many(vec![bundle], None, Some(probe_state()))
        .await
        .expect("callMany should succeed");
    let out = res[0][0].value.as_ref().expect("call must succeed");
    let (prevrandao, milli) = words(out);
    assert_eq!(milli, t2 * 1000 + 123);
    assert_eq!(prevrandao, 123);

    let bad = Bundle {
        transactions: vec![probe_request()],
        block_override: Some(BlockOverrides { random: Some(randao(1000)), ..Default::default() }),
    };
    let err = api
        .call_many(vec![bad], None, Some(probe_state()))
        .await
        .expect_err("must be rejected");
    assert!(err.to_string().contains("must be less than 1000"));
}

#[tokio::test]
async fn eth_estimate_gas_executes_the_precompile() {
    // reth's eth_estimateGas RPC surface has no blockOverrides parameter
    // (upstream API difference, all chains alike) — this pins that the 0x70
    // path executes fine under gas estimation, which shares `prepare_call_env`
    // with eth_call.
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let gas = api
        .estimate_gas(probe_request(), None, Some(probe_state()))
        .await
        .expect("estimate should succeed");
    assert!(gas > U256::from(21_000u64), "probe + STATICCALL(0x70) must cost more than a transfer");
}

#[tokio::test]
async fn eth_simulate_v1_rejects_out_of_range_prev_randao() {
    // The per-simulated-block override application runs before execution, so
    // the < 1000 validation is handler-testable here. The *success* path is
    // not: simulated blocks execute through the BSC block executor, whose
    // pre-execution hooks read the set-once shared globals (snapshot provider,
    // header reader) that other tests in this binary already claim — the
    // assembled-value scenarios for eth_simulateV1 are covered end-to-end on
    // the live devnet (E2: single-block combo, prevRandao-only without time),
    // and the chained multi-block case is a pre-existing synthetic-parent gap
    // tracked separately.
    let api = bsc_eth_api!(spec(Some(JENNER_AT)));
    let bad = SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: Some(BlockOverrides {
                random: Some(randao(1000)),
                ..Default::default()
            }),
            state_overrides: Some(probe_state()),
            calls: vec![probe_request()],
        }],
        trace_transfers: false,
        validation: false,
        return_full_transactions: false,
    };
    let err = api.simulate_v1(bad, None).await.expect_err("must be rejected");
    assert!(err.to_string().contains("must be less than 1000"));
}
