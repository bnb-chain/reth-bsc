//! Execute the override matrix through public RPC handlers, using mainnet fork
//! boundaries rather than moving only a late fork in an otherwise modern env.
use super::*;
use alloy_consensus::Header;
use alloy_rpc_types::trace::geth::{
    GethDebugTracingCallOptions, GethDebugTracingOptions, GethTrace,
};
use reth_rpc::DebugApi;

const KEPLER: u64 = 1_705_996_800;
const CANCUN: u64 = 1_718_863_500;
const LONDON: u64 = 31_302_048;
const DIFFICULTY: u8 = 0x44;
const BLOBBASEFEE: u8 = 0x4a;

fn header(number: u64, time: u64) -> Header {
    let cancun = number >= LONDON && time >= CANCUN;
    Header {
        number,
        timestamp: time,
        difficulty: U256::from(2),
        gas_limit: 140_000_000,
        base_fee_per_gas: (number >= LONDON).then_some(0),
        excess_blob_gas: cancun.then_some(0),
        blob_gas_used: cancun.then_some(0),
        ..Default::default()
    }
}

// Return 42 only if the opcode sees the expected value. A wrong value fails gas
// estimation too, which would not be detected by merely checking its success.
fn requires(opcode: u8, expected: U256) -> Vec<u8> {
    let mut code = vec![opcode, 0x7f];
    code.extend(expected.to_be_bytes::<32>());
    code.extend([0x14, 0x60, 39, 0x57, 0xfe, 0x5b]);
    code.extend(RETURNS_42);
    code
}

fn trace_value(trace: GethTrace) -> U256 {
    let frame = trace.try_into_call_frame().unwrap();
    assert!(frame.error.is_none(), "{frame:?}");
    value(frame.output.as_ref().expect("call tracer output"))
}

// The macro avoids naming EthApi's nested generic type. Each route gets the
// same overrides, including the inspector path and repeated bundle execution.
macro_rules! check_routes {
    ($api:expr, $hash:expr, $ov:expr, $code:expr) => {
        check_routes!($api, $hash, $ov, request(PROXY, &[1]), overrides(PROXY, &$code))
    };
    ($api:expr, $hash:expr, $ov:expr, $req:expr, $state:expr) => {{
        let (api, hash, ov, req, state) = (&$api, $hash, $ov, $req, $state);
        let out = api
            .call(req.clone(), Some(hash.into()), Some(state.clone()), Some(Box::new(ov.clone())))
            .await
            .unwrap();
        assert_eq!(value(&out), U256::from(42));
        let gas = api
            .estimate_gas(
                req.clone(),
                Some(hash.into()),
                Some(state.clone()),
                Some(Box::new(ov.clone())),
            )
            .await
            .unwrap();
        let mut estimated = req.clone();
        estimated.gas = Some(gas.to());
        api.call(estimated, Some(hash.into()), Some(state.clone()), Some(Box::new(ov.clone())))
            .await
            .unwrap();
        let bundle = Bundle {
            transactions: vec![req.clone(), req.clone()],
            block_override: Some(ov.clone()),
        };
        let context = StateContext { block_number: Some(hash.into()), ..Default::default() };
        let results = api
            .call_many(
                vec![bundle.clone(), bundle.clone()],
                Some(context),
                Some(state.clone()),
            )
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        for bundle in results {
            assert_eq!(bundle.len(), 2);
            for result in bundle {
                assert!(result.error.is_none(), "{result:?}");
                assert_eq!(value(&result.value.unwrap()), U256::from(42));
            }
        }
        let debug = DebugApi::new(
            api.0.clone(),
            reth_tasks::pool::BlockingTaskGuard::new(4),
            &reth_tasks::Runtime::test(),
            futures::stream::empty(),
        );
        let mut opts: GethDebugTracingCallOptions =
            GethDebugTracingOptions::call_tracer(Default::default()).into();
        opts.state_overrides = Some(state);
        opts.block_overrides = Some(ov);
        let trace = debug.debug_trace_call(req, Some(hash.into()), opts.clone()).await.unwrap();
        assert_eq!(trace_value(trace), U256::from(42));
        let results = debug
            .debug_trace_call_many(vec![bundle.clone(), bundle], Some(context), Some(opts))
            .await
            .unwrap();
        assert_eq!(results.len(), 2);
        for bundle in results {
            assert_eq!(bundle.len(), 2);
            for trace in bundle {
                assert_eq!(trace_value(trace), U256::from(42));
            }
        }
    }};
}

#[tokio::test]
async fn bidirectional_mainnet_forks_preserve_difficulty_on_all_call_routes() {
    for (number, selected, time, override_number) in [
        (35_400_000, KEPLER - 1, KEPLER, None),
        (35_400_000, KEPLER, KEPLER - 1, None),
        (40_000_000, CANCUN - 1, CANCUN, None),
        (40_000_000, CANCUN, CANCUN - 1, None),
        (LONDON - 1, CANCUN, CANCUN, Some(LONDON)),
        (LONDON, CANCUN, CANCUN, Some(LONDON - 1)),
        (35_400_000, KEPLER - 1, KEPLER - 1, None),
        (35_400_000, KEPLER, KEPLER, None),
    ] {
        let (api, hash) = rpc!(chain bsc_mainnet(), header header(number, selected));
        for (difficulty, random, expected) in [
            (None, None, 2),
            (Some(U256::ZERO), None, 0),
            (Some(U256::from(7)), None, 7),
            (None, Some(B256::ZERO), 0),
            (None, Some(B256::from(U256::from(999))), 999),
            (Some(U256::from(7)), Some(B256::from(U256::from(9))), 9),
        ] {
            let ov = BlockOverrides {
                time: (time != selected).then_some(time),
                number: override_number.map(U256::from),
                difficulty,
                random,
                ..Default::default()
            };
            check_routes!(api, hash, ov, requires(DIFFICULTY, U256::from(expected)));
        }
        // Overrides never mutate the canonical provider or leak into another call.
        let out = api
            .call(
                request(PROXY, &[]),
                Some(hash.into()),
                Some(overrides(PROXY, &requires(DIFFICULTY, U256::from(2)))),
                None,
            )
            .await
            .unwrap();
        assert_eq!(value(&out), U256::from(42));
    }
}

#[tokio::test]
async fn crossing_cancun_initializes_only_absent_blob_context() {
    let (api, hash) = rpc!(chain bsc_mainnet(), header header(40_000_000, CANCUN - 1));
    for fee in [None, Some(U256::ZERO), Some(U256::from(42))] {
        let ov = BlockOverrides { time: Some(CANCUN), blob_base_fee: fee, ..Default::default() };
        check_routes!(api, hash, ov, requires(BLOBBASEFEE, fee.unwrap_or(U256::from(1))));
    }
    // Leaving Cancun deactivates the opcode, even when its value was overridden.
    let (api, hash) = rpc!(chain bsc_mainnet(), header header(40_000_000, CANCUN));
    let ov = Some(Box::new(BlockOverrides {
        time: Some(CANCUN - 1),
        blob_base_fee: Some(U256::from(42)),
        ..Default::default()
    }));
    let state = Some(overrides(PROXY, &requires(BLOBBASEFEE, U256::from(42))));
    let err = api
        .call(request(PROXY, &[]), Some(hash.into()), state.clone(), ov.clone())
        .await
        .unwrap_err();
    assert!(err.message().contains("NotActivated"), "{err}");
    let err =
        api.estimate_gas(request(PROXY, &[]), Some(hash.into()), state, ov).await.unwrap_err();
    assert!(err.message().contains("NotActivated"), "{err}");
}

#[tokio::test]
async fn crossing_kepler_activates_and_deactivates_push0() {
    let push0 = hex!("5f50602a60005260206000f3");
    let (before, hash) = rpc!(chain bsc_mainnet(), header header(35_400_000, KEPLER - 1));
    check_routes!(before, hash, BlockOverrides { time: Some(KEPLER), ..Default::default() }, push0);
    let (after, hash) = rpc!(chain bsc_mainnet(), header header(35_400_000, KEPLER));
    let ov = Some(Box::new(BlockOverrides { time: Some(KEPLER - 1), ..Default::default() }));
    let state = Some(overrides(PROXY, &push0));
    let err = after
        .call(request(PROXY, &[]), Some(hash.into()), state.clone(), ov.clone())
        .await
        .unwrap_err();
    assert!(err.message().contains("NotActivated"), "{err}");
    let err =
        after.estimate_gas(request(PROXY, &[]), Some(hash.into()), state, ov).await.unwrap_err();
    assert!(err.message().contains("NotActivated"), "{err}");
}

#[test]
fn canonical_environments_and_rpc_configuration_are_preserved() {
    let config = BscEvmConfig::new(Arc::new(BscChainSpec::from(bsc_mainnet())));
    for (number, time) in [
        (1, 1),
        (LONDON, KEPLER - 1),
        (35_400_000, KEPLER),
        (40_000_000, CANCUN),
        (70_000_000, NOW),
    ] {
        let env = config.evm_env(&header(number, time)).unwrap();
        assert_eq!(config.with_block_rules(env.clone()), env);
    }
    for cap in [123_456, u64::MAX] {
        let mut env = config.evm_env(&header(35_400_000, KEPLER - 1)).unwrap();
        env.cfg_env.tx_gas_limit_cap = Some(cap);
        env.cfg_env.disable_eip3607 = true;
        env.cfg_env.disable_base_fee = true;
        env.cfg_env.disable_block_gas_limit = true;
        env.cfg_env.disable_nonce_check = true;
        env.cfg_env.disable_fee_charge = true;
        env.cfg_env.memory_limit = 12_345_678;
        env.cfg_env.max_blobs_per_tx = Some(2);
        env.block_env.gas_limit = 765_432;
        env.block_env.basefee = 123;
        env.block_env.disabled_cas20.insert(TOKEN);
        env.block_env.timestamp = U256::from(NOW);
        let cfg = env.cfg_env.clone();
        let updated = config.with_block_rules(env);
        let mut expected = cfg;
        expected.set_spec_and_mainnet_gas_params(updated.cfg_env.spec);
        assert_eq!(updated.cfg_env, expected);
        assert_eq!(updated.block_env.gas_limit, 765_432);
        assert_eq!(updated.block_env.basefee, 123);
        assert!(updated.block_env.disabled_cas20.contains(&TOKEN));
        assert_eq!(
            config.with_block_rules(updated.clone()),
            updated,
            "normalization is idempotent"
        );
    }
}

#[tokio::test]
async fn simulation_converts_both_directions_with_and_without_inspection() {
    for boundary in [KEPLER, CANCUN] {
        for (parent_time, target_time) in [(boundary - 13, boundary), (boundary - 2, boundary - 1)]
        {
            // simulateV1 derives parent+12 first. Both overrides are later than
            // the parent, but move the derived env across the fork in opposite directions.
            let (api, hash) = rpc!(chain bsc_mainnet(), header header(40_000_000, parent_time));
            for trace_transfers in [false, true] {
                for validation in [false, true] {
                    for (difficulty, random, expected) in [
                        (None, None, 0),
                        (Some(U256::ZERO), None, 0),
                        (Some(U256::from(7)), None, 7),
                        (Some(U256::from(7)), Some(B256::from(U256::from(9))), 9),
                    ] {
                        let payload = SimulatePayload {
                            block_state_calls: vec![SimBlock {
                                block_overrides: Some(BlockOverrides {
                                    time: Some(target_time),
                                    difficulty,
                                    random,
                                    ..Default::default()
                                }),
                                state_overrides: Some(overrides(
                                    PROXY,
                                    &requires(DIFFICULTY, U256::from(expected)),
                                )),
                                calls: vec![request(PROXY, &[1])],
                            }],
                            trace_transfers,
                            validation,
                            return_full_transactions: false,
                        };
                        let result = api.simulate_v1(payload, Some(hash.into())).await.unwrap();
                        assert!(result[0].calls[0].status, "{result:?}");
                        assert_eq!(value(&result[0].calls[0].return_data), U256::from(42));
                        let synthetic = &result[0].inner.header.inner;
                        assert_eq!(
                            synthetic.difficulty,
                            difficulty.unwrap_or_default(),
                            "EVM representation must not rewrite the requested header difficulty"
                        );
                        assert_eq!(
                            synthetic.mix_hash,
                            random.unwrap_or_default(),
                            "EVM representation must not rewrite the requested mixHash"
                        );
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn debug_call_many_implicit_advance_also_converts_the_environment() {
    for boundary in [KEPLER, CANCUN] {
        let (api, hash) = rpc!(chain bsc_mainnet(), header header(40_000_000, boundary - 1));
        let debug = DebugApi::new(
            api.0,
            reth_tasks::pool::BlockingTaskGuard::new(4),
            &reth_tasks::Runtime::test(),
            futures::stream::empty(),
        );
        let mut opts: GethDebugTracingCallOptions =
            GethDebugTracingOptions::call_tracer(Default::default()).into();
        opts.state_overrides = Some(overrides(PROXY, &requires(DIFFICULTY, U256::from(2))));
        let bundle = Bundle { transactions: vec![request(PROXY, &[])], block_override: None };
        let result = debug
            .debug_trace_call_many(
                vec![bundle.clone(), bundle],
                Some(StateContext { block_number: Some(hash.into()), ..Default::default() }),
                Some(opts),
            )
            .await
            .unwrap();
        for frame in result.into_iter().flatten() {
            assert_eq!(trace_value(frame), U256::from(42));
        }
    }
}

#[tokio::test]
async fn invalid_prev_randao_stays_invalid_across_forks_on_every_rpc() {
    for random in [U256::from(1000), U256::from(1) << 200] {
        let (api, hash) = rpc!(chain bsc_mainnet(), header header(40_000_000, KEPLER - 13));
        let ov = BlockOverrides {
            time: Some(KEPLER),
            random: Some(random.into()),
            ..Default::default()
        };
        let req = request(PROXY, &[]);
        let err = api
            .call(req.clone(), Some(hash.into()), None, Some(Box::new(ov.clone())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), -32602);
        let err = api
            .estimate_gas(req.clone(), Some(hash.into()), None, Some(Box::new(ov.clone())))
            .await
            .unwrap_err();
        assert_eq!(err.code(), -32602);
        let bundle = Bundle { transactions: vec![req.clone()], block_override: Some(ov.clone()) };
        let context = StateContext { block_number: Some(hash.into()), ..Default::default() };
        let err = api.call_many(vec![bundle.clone()], Some(context), None).await.unwrap_err();
        assert!(err.to_string().contains("must be less than 1000"), "{err}");
        let debug = DebugApi::new(
            api.0.clone(),
            reth_tasks::pool::BlockingTaskGuard::new(4),
            &reth_tasks::Runtime::test(),
            futures::stream::empty(),
        );
        let opts =
            GethDebugTracingCallOptions { block_overrides: Some(ov.clone()), ..Default::default() };
        assert!(debug
            .debug_trace_call(req.clone(), Some(hash.into()), opts.clone())
            .await
            .unwrap_err()
            .to_string()
            .contains("must be less than 1000"));
        assert!(debug
            .debug_trace_call_many(vec![bundle], Some(context), Some(opts))
            .await
            .unwrap_err()
            .to_string()
            .contains("must be less than 1000"));
        let payload = SimulatePayload {
            block_state_calls: vec![SimBlock {
                block_overrides: Some(ov),
                state_overrides: None,
                calls: vec![req],
            }],
            trace_transfers: false,
            validation: false,
            return_full_transactions: false,
        };
        assert_eq!(api.simulate_v1(payload, Some(hash.into())).await.unwrap_err().code(), -32602);
    }
}

#[tokio::test]
async fn block_gas_override_keeps_distinct_call_and_estimate_policies() {
    const NEEDS_OVER_2_24: &[u8] = &hex!("63010000005a11600b57fe5b00");
    const OSAKA: u64 = 1_777_343_400;
    let (api, hash) = rpc!(chain bsc_mainnet(), header header(70_000_000, OSAKA - 1));
    let mut req = request(PROXY, &[1]);
    req.gas = Some(20_000_000);
    let ov = BlockOverrides { time: Some(OSAKA), gas_limit: Some(30_000), ..Default::default() };
    let state = overrides(PROXY, NEEDS_OVER_2_24);
    // eth_call and debug calls lift the protocol tx cap and block limit. The
    // estimator must still bound its search by min(block override, fork cap).
    api.call(req.clone(), Some(hash.into()), Some(state.clone()), Some(Box::new(ov.clone())))
        .await
        .unwrap();
    assert!(api
        .estimate_gas(
            req.clone(),
            Some(hash.into()),
            Some(state.clone()),
            Some(Box::new(ov.clone()))
        )
        .await
        .is_err());
    let debug = DebugApi::new(
        api.0,
        reth_tasks::pool::BlockingTaskGuard::new(4),
        &reth_tasks::Runtime::test(),
        futures::stream::empty(),
    );
    let opts = GethDebugTracingCallOptions {
        block_overrides: Some(ov),
        state_overrides: Some(state),
        ..Default::default()
    };
    let frame = debug
        .debug_trace_call(req, Some(hash.into()), opts)
        .await
        .unwrap()
        .try_into_default_frame()
        .unwrap();
    assert!(!frame.failed, "{frame:?}");
}

#[tokio::test]
async fn blob_fee_defaults_agree_before_transaction_conversion_and_execution() {
    for initial_fee in [1, 7] {
        let mut chain = bsc_mainnet();
        chain.blob_params.cancun.min_blob_fee = initial_fee;
        let (api, hash) = rpc!(chain chain, header header(40_000_000, CANCUN - 1));
        for fee in [None, Some(U256::ZERO), Some(U256::from(42))] {
            let mut req = request(PROXY, &[1]);
            req.blob_versioned_hashes = Some(vec![B256::repeat_byte(1)]);
            let mut state =
                overrides(PROXY, &requires(BLOBBASEFEE, fee.unwrap_or(U256::from(initial_fee))));
            state.insert(
                Address::ZERO,
                AccountOverride { balance: Some(U256::from(1_000_000_000)), ..Default::default() },
            );
            let ov =
                BlockOverrides { time: Some(CANCUN), blob_base_fee: fee, ..Default::default() };
            check_routes!(api, hash, ov.clone(), req.clone(), state.clone());
            // A transaction fee cap of zero must not be mistaken for an absent
            // fee cap. It succeeds only with the explicit zero block blob fee.
            req.max_fee_per_blob_gas = Some(0);
            let call = api
                .call(
                    req.clone(),
                    Some(hash.into()),
                    Some(state.clone()),
                    Some(Box::new(ov.clone())),
                )
                .await;
            let estimate =
                api.estimate_gas(req, Some(hash.into()), Some(state), Some(Box::new(ov))).await;
            if fee == Some(U256::ZERO) {
                assert_eq!(value(&call.unwrap()), U256::from(42));
                estimate.unwrap();
            } else {
                for err in [call.unwrap_err(), estimate.unwrap_err()] {
                    assert_eq!(err.message(), "max fee per blob gas less than block blob gas fee");
                }
            }
        }
    }
}

#[test]
fn reversing_forks_retains_overridden_blob_fees_and_opcode_values() {
    let config = BscEvmConfig::new(Arc::new(BscChainSpec::from(bsc_mainnet())));
    let mut env = config.evm_env(&header(40_000_000, CANCUN)).unwrap();
    let ov = BlockOverrides {
        time: Some(KEPLER - 1),
        difficulty: Some(U256::ZERO),
        blob_base_fee: Some(U256::ZERO),
        ..Default::default()
    };
    apply_block_overrides(ov.clone(), &mut InMemoryDB::default(), env.block_env.inner_mut());
    env.block_env.apply_block_overrides_ext(&ov).unwrap();
    env = config.with_block_rules(env);
    assert_eq!(env.block_env.difficulty, U256::ZERO);
    env.block_env.timestamp = U256::from(CANCUN);
    env = config.with_block_rules(env);
    assert_eq!(Block::prevrandao(&env.block_env), Some(B256::ZERO));
    assert_eq!(env.block_env.blob_excess_gas_and_price.unwrap().blob_gasprice, 0);
    assert_eq!(env.cfg_env.max_blobs_per_tx, Some(6));
}

#[tokio::test]
async fn explicit_block_fields_survive_fork_conversion() {
    let (api, hash) = rpc!(chain bsc_mainnet(), header header(35_400_000, KEPLER - 1));
    let ov = BlockOverrides {
        time: Some(CANCUN),
        number: Some(U256::from(40_000_000)),
        coinbase: Some(Address::with_last_byte(123)),
        gas_limit: Some(234_567),
        base_fee: Some(U256::from(3)),
        ..Default::default()
    };
    for (opcode, expected) in
        [(0x42, CANCUN), (0x43, 40_000_000), (0x41, 123), (0x45, 234_567), (0x48, 3)]
    {
        let mut req = request(PROXY, &[1]);
        // A nonzero gas price preserves BASEFEE's opcode value in call helpers.
        req.gas_price = Some(7);
        let mut state = overrides(PROXY, &requires(opcode, U256::from(expected)));
        state.insert(
            Address::ZERO,
            AccountOverride { balance: Some(U256::from(1_000_000_000)), ..Default::default() },
        );
        check_routes!(api, hash, ov.clone(), req, state);
    }
}
