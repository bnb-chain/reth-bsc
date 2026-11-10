//! In-process E2E for the `eth_simulateV1` success path of the BSC block-override
//! semantics (BEP-706 phase 2) — the go-bsc `TestJennerBlockOverrides_SimulateV1`
//! counterpart.
//!
//! This lives in an integration-test binary on purpose: simulated blocks execute
//! through the BSC block executor, whose parent-header lookup goes through the
//! process-wide shared header provider (`reth_bsc::shared`). Those are set-once
//! globals — inside the unit-test binary other tests claim them first, but each
//! integration test binary is a fresh process, so this file owns them and the
//! success path closes the automation loop that the unit tests (rejection path)
//! and the live devnet run started.
//!
//! Simulation mode skips Parlia finalization, so no snapshot provider is needed.
//! Exercise the BSC handler registered by the node, including chained synthetic
//! parents, rather than the generic Ethereum RPC implementation.

use alloy_primitives::{hex, Address, Bytes, B256, U256};
use alloy_rpc_types_eth::{
    simulate::{SimBlock, SimulatePayload},
    state::StateOverride,
    BlockOverrides, TransactionRequest,
};
use reth_bsc::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    hardforks::bsc::BscHardfork,
    node::{evm::config::BscEvmConfig, primitives::BscPrimitives},
    rpc::code_overrides::{BscCodeOverridesApiImpl, BscCodeOverridesApiServer},
};
use reth_chainspec::ForkCondition;
use reth_network_api::noop::NoopNetwork;
use reth_provider::test_utils::MockEthProvider;
use reth_transaction_pool::test_utils::testing_pool;
use std::sync::Arc;

/// Probe: returns `[prevrandao (0x44), staticcall(0x70) result]` as two words.
const PROBE_CODE: &[u8] = &hex!("44600052602060206000600060705afa5060406000f3");
const PROBE_ADDR: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x0b, 0xee, 0xf0,
]);

const JENNER_AT: u64 = 1_800_000_000;
const HEAD_SECS: u64 = JENNER_AT + 100;
const HEAD_REMAINDER: u64 = 450;

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
    TransactionRequest { to: Some(PROBE_ADDR.into()), gas: Some(500_000), ..Default::default() }
}

fn randao(ms: u64) -> B256 {
    B256::from(U256::from(ms))
}

fn words(out: &Bytes) -> (u64, u64) {
    assert_eq!(out.len(), 64, "probe must return two words, got {out}");
    (U256::from_be_slice(&out[..32]).to::<u64>(), U256::from_be_slice(&out[32..64]).to::<u64>())
}

fn sim_block(overrides: BlockOverrides) -> SimulatePayload<TransactionRequest> {
    SimulatePayload {
        block_state_calls: vec![SimBlock {
            block_overrides: Some(overrides),
            state_overrides: Some(probe_state()),
            calls: vec![probe_request()],
        }],
        trace_transfers: false,
        validation: false,
        return_full_transactions: false,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn simulate_v1_success_paths_apply_the_bsc_semantics() {
    let spec = {
        let mut cs = bsc_mainnet();
        cs.hardforks.insert(BscHardfork::Jenner, ForkCondition::Timestamp(JENNER_AT));
        Arc::new(BscChainSpec::from(cs))
    };

    let provider = MockEthProvider::<BscPrimitives, _>::new().with_chain_spec((*spec).clone());
    let mut mix = [0u8; 32];
    mix[24..32].copy_from_slice(&HEAD_REMAINDER.to_be_bytes());
    let header = alloy_consensus::Header {
        // Past mainnet London (31302048): the Jenner helper carries the London gate.
        number: 40_000_000,
        timestamp: HEAD_SECS,
        mix_hash: B256::new(mix),
        gas_limit: 140_000_000,
        base_fee_per_gas: Some(0),
        difficulty: U256::from(2u64),
        excess_blob_gas: Some(0),
        blob_gas_used: Some(0),
        ..Default::default()
    };
    let hash = header.hash_slow();
    provider.add_header(hash, header.clone());
    provider.add_block(
        hash,
        reth_bsc::node::primitives::BscBlock {
            header: header.clone(),
            body: reth_bsc::node::primitives::BscBlockBody::default(),
        },
    );

    // Fresh process: this binary owns the shared header provider the BSC block
    // executor resolves simulated parents through.
    reth_bsc::shared::set_header_provider(Arc::new(provider.clone()))
        .expect("integration test binary must be the first to set the header provider");

    let api = reth::rpc::eth::core::EthApi::builder(
        provider.clone(),
        testing_pool(),
        NoopNetwork::default(),
        BscEvmConfig::new(spec),
    )
    .build();
    let api = BscCodeOverridesApiImpl(api);

    // time + prevRandao: both apply — seconds from time, remainder from prevRandao.
    let t1 = HEAD_SECS + 100;
    let blocks = api
        .simulate_v1(
            sim_block(BlockOverrides {
                time: Some(t1),
                random: Some(randao(7)),
                ..Default::default()
            }),
            None,
        )
        .await
        .expect("combined override must simulate");
    let (prevrandao, milli) = words(&blocks[0].calls[0].return_data);
    assert_eq!(milli, t1 * 1000 + 7, "0x70 must serve the assembled millisecond timestamp");
    assert_eq!(prevrandao, 7, "0x44 must serve the override");
    assert_eq!(blocks[0].inner.header.inner.timestamp, t1);

    // time only: remainder resets to .000.
    let t2 = HEAD_SECS + 200;
    let blocks = api
        .simulate_v1(sim_block(BlockOverrides { time: Some(t2), ..Default::default() }), None)
        .await
        .expect("time-only override must simulate");
    let (prevrandao, milli) = words(&blocks[0].calls[0].return_data);
    assert_eq!(milli, t2 * 1000);
    assert_eq!(prevrandao, 2, "simulated blocks inherit parent difficulty");
    assert_eq!(blocks[0].inner.header.inner.difficulty, U256::from(2));
    assert_eq!(blocks[0].inner.header.inner.mix_hash, B256::ZERO);

    // prevRandao only, no time: sanitize supplies the default next-block seconds,
    // the override supplies the remainder (the go sixth-round-review scenario).
    let blocks = api
        .simulate_v1(
            sim_block(BlockOverrides { random: Some(randao(42)), ..Default::default() }),
            None,
        )
        .await
        .expect("prevRandao-only override must simulate");
    let block_secs = blocks[0].inner.header.inner.timestamp;
    let (prevrandao, milli) = words(&blocks[0].calls[0].return_data);
    assert!(block_secs > HEAD_SECS, "sanitize must advance the simulated timestamp");
    assert_eq!(milli, block_secs * 1000 + 42);
    assert_eq!(prevrandao, 42);

    // And the client-default validation still rejects out-of-range values here.
    let err = api
        .simulate_v1(
            sim_block(BlockOverrides { random: Some(randao(1000)), ..Default::default() }),
            None,
        )
        .await
        .expect_err("out-of-range remainder must be rejected");
    assert!(err.to_string().contains("must be less than 1000"));

    // Both Parlia difficulties must work with or without overrides, validation,
    // and transfer tracing. The base header's .450 remainder must not leak.
    for difficulty in [1u64, 2] {
        let mut base = header.clone();
        base.difficulty = U256::from(difficulty);
        let hash = base.hash_slow();
        provider.add_header(hash, base.clone());
        provider.add_block(
            hash,
            reth_bsc::node::primitives::BscBlock { header: base, body: Default::default() },
        );
        let at = Some(hash.into());
        let direct = api.call(probe_request(), at, Some(probe_state()), None).await.unwrap();
        assert_eq!(words(&direct).0, difficulty);

        for validation in [false, true] {
            for trace_transfers in [false, true] {
                for time in [None, Some(HEAD_SECS + 200)] {
                    let mut request = sim_block(BlockOverrides { time, ..Default::default() });
                    if time.is_none() {
                        request.block_state_calls[0].block_overrides = None;
                    }
                    request.validation = validation;
                    request.trace_transfers = trace_transfers;
                    let blocks = api.simulate_v1(request, at).await.unwrap_or_else(|error| {
                        panic!("difficulty={difficulty}, validation={validation}, trace={trace_transfers}, time={time:?}: {error}")
                    });
                    let block = &blocks[0];
                    assert!(block.calls[0].status);
                    assert_eq!(
                        words(&block.calls[0].return_data),
                        (difficulty, block.inner.header.inner.timestamp * 1000)
                    );
                    assert_eq!(block.inner.header.inner.difficulty, U256::from(difficulty));
                    assert_eq!(block.inner.header.inner.mix_hash, B256::ZERO);
                }
            }
        }

        // Explicit zero is not an absent override. A randomness override changes
        // the opcode and remainder, while difficulty remains a separate header field.
        for (overrides, want_difficulty, want_opcode, want_remainder) in [
            (BlockOverrides::default(), difficulty, difficulty, 0),
            (BlockOverrides { difficulty: Some(U256::ZERO), ..Default::default() }, 0, 0, 0),
            (BlockOverrides { difficulty: Some(U256::from(5)), ..Default::default() }, 5, 5, 0),
            (BlockOverrides { random: Some(randao(0)), ..Default::default() }, difficulty, 0, 0),
            (BlockOverrides { random: Some(randao(7)), ..Default::default() }, difficulty, 7, 7),
            (
                BlockOverrides {
                    difficulty: Some(U256::from(5)),
                    random: Some(randao(7)),
                    ..Default::default()
                },
                5,
                7,
                7,
            ),
        ] {
            let mut request = sim_block(overrides);
            request
                .block_state_calls
                .extend(sim_block(BlockOverrides::default()).block_state_calls);
            let blocks = api.simulate_v1(request, at).await.unwrap();
            assert_eq!(blocks.len(), 2);
            for (i, block) in blocks.iter().enumerate() {
                assert!(block.calls[0].status);
                assert_eq!(block.inner.header.inner.difficulty, U256::from(want_difficulty));
                let (opcode, remainder) =
                    if i == 0 { (want_opcode, want_remainder) } else { (want_difficulty, 0) };
                assert_eq!(
                    words(&block.calls[0].return_data),
                    (opcode, block.inner.header.inner.timestamp * 1000 + remainder)
                );
                assert_eq!(block.inner.header.inner.mix_hash, randao(remainder));
            }
            assert_eq!(blocks[1].inner.header.inner.parent_hash, blocks[0].inner.header.hash);
        }
    }
}
