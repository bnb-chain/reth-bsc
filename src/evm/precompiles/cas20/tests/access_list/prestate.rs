use super::*;
use crate::rpc::prestate::{BscPrestateApiImpl, BscPrestateApiServer};
use alloy_rpc_types::trace::geth::{GethDebugTracingCallOptions, GethTrace};
use alloy_rpc_types_eth::Bundle;
use serde_json::{json, Value};

macro_rules! debug {
    ($host:expr) => {{
        let api = rpc!($host);
        BscPrestateApiImpl(reth_rpc::DebugApi::new(
            api.0,
            reth_tasks::pool::BlockingTaskGuard::new(4),
            &reth_tasks::Runtime::test(),
            futures::stream::empty(),
        ))
    }};
}

fn options(config: Value, mux: bool) -> GethDebugTracingCallOptions {
    serde_json::from_value(if mux {
        json!({"tracer":"muxTracer", "tracerConfig":{"prestateTracer":config,"callTracer":{}}})
    } else {
        json!({"tracer":"prestateTracer", "tracerConfig":config})
    })
    .unwrap()
}

fn prestate(trace: GethTrace, mux: bool) -> Value {
    let trace = serde_json::to_value(trace).unwrap();
    if mux {
        trace["prestateTracer"].clone()
    } else {
        trace
    }
}

#[tokio::test]
async fn debug_calls_run_cas20_code_overrides() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = debug!(h.st);
    // Increment and return slot zero.
    let code = alloy_primitives::hex!("6000546001018060005560005260206000f3");
    let state = [(
        token,
        AccountOverride {
            code: Some(Bytes::copy_from_slice(&code)),
            state_diff: Some([(B256::ZERO, B256::ZERO)].into_iter().collect()),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect();
    let opts = GethDebugTracingCallOptions { state_overrides: Some(state), ..Default::default() };

    let trace =
        api.debug_trace_call(request(token, vec![]), None, Some(opts.clone())).await.unwrap();
    let output = trace.try_into_default_frame().unwrap().return_value;
    assert_eq!(U256::from_be_slice(&output), U256::from(1));

    let bundle = Bundle {
        transactions: vec![request(token, vec![]), request(token, vec![])],
        block_override: None,
    };
    let traces =
        api.debug_trace_call_many(vec![bundle.clone(), bundle], None, Some(opts)).await.unwrap();
    assert_eq!(traces.len(), 2);
    for (index, trace) in traces.into_iter().flatten().enumerate() {
        let output = trace.try_into_default_frame().unwrap().return_value;
        assert_eq!(U256::from_be_slice(&output), U256::from(index + 1));
    }
    // Overrides must not affect later requests.
    let trace = api
        .debug_trace_call(request(token, call_data(SEL_BALANCE_OF, &[a(ALICE)])), None, None)
        .await
        .unwrap();
    let output = trace.try_into_default_frame().unwrap().return_value;
    assert_eq!(U256::from_be_slice(&output), U256::from(123));
}

const CAS20_CODE_HASH: &str = "0x309b8896ee4c1ff7ec1966155373dee42663b6b40c3fedc70ba501684848d2a3";

#[tokio::test]
async fn existing_token_prestate_reports_code_hash() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = debug!(h.st);
    let address = format!("{token:#x}");
    for mux in [false, true] {
        for config in [
            json!({}),
            json!({"includeEmpty":true}),
            json!({"diffMode":true}),
            json!({"diffMode":true,"disableCode":true}),
            json!({"disableCode":true}),
        ] {
            let trace = api
                .debug_trace_call(
                    request(token, call_data(SEL_TRANSFER, &[a(BOB), w(7)])),
                    None,
                    Some(options(config.clone(), mux)),
                )
                .await
                .unwrap();
            let frame = prestate(trace, mux);
            let pre = if config["diffMode"] == true { &frame["pre"] } else { &frame };
            assert_eq!(pre[&address]["codeHash"], CAS20_CODE_HASH, "{config}: {frame}");
            assert_eq!(pre[&address].get("code").is_none(), config["disableCode"] == true);
            assert!(frame["post"][&address].get("codeHash").is_none());
        }
    }
}

#[tokio::test]
async fn native_reads_writes_and_reverted_slots_reach_prestate_and_mux() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = debug!(h.st);
    let balance = mapping_slot(slot_at(SLOT_BALANCES), a(ALICE));
    let bob_balance = mapping_slot(slot_at(SLOT_BALANCES), a(BOB));
    for mux in [false, true] {
        let trace = api
            .debug_trace_call(
                request(token, call_data(SEL_BALANCE_OF, &[a(ALICE)])),
                None,
                Some(options(json!({}), mux)),
            )
            .await
            .unwrap();
        let pre = prestate(trace, mux);
        assert_eq!(
            pre[format!("{token:#x}")]["storage"][format!("{balance:#066x}")],
            format!("{:#066x}", U256::from(123))
        );
        let trace = api
            .debug_trace_call(
                request(token, call_data(SEL_TRANSFER, &[a(BOB), w(7)])),
                None,
                Some(options(json!({"diffMode":true}), mux)),
            )
            .await
            .unwrap();
        let diff = prestate(trace, mux);
        assert_eq!(
            diff["pre"][format!("{token:#x}")]["storage"][format!("{balance:#066x}")],
            format!("{:#066x}", U256::from(123))
        );
        assert_eq!(
            diff["post"][format!("{token:#x}")]["storage"][format!("{bob_balance:#066x}")],
            format!("{:#066x}", U256::from(7))
        );

        let inner = Address::repeat_byte(0xc1);
        let outer = Address::repeat_byte(0xc2);
        let mut opts = options(json!({}), mux);
        opts.state_overrides = Some(
            [
                (inner, AccountOverride { code: Some(forward(token, true)), ..Default::default() }),
                (
                    outer,
                    AccountOverride { code: Some(forward(inner, false)), ..Default::default() },
                ),
            ]
            .into_iter()
            .collect(),
        );
        for target in [inner, outer] {
            let trace = api
                .debug_trace_call(
                    request(target, call_data(SEL_APPROVE, &[a(BOB), w(17)])),
                    None,
                    Some(opts.clone()),
                )
                .await
                .unwrap();
            let pre = prestate(trace, mux);
            let allowance =
                mapping_slot(mapping_slot(slot_at(storage::SLOT_ALLOWANCES), a(inner)), a(BOB));
            assert_eq!(
                pre[format!("{token:#x}")]["storage"][format!("{allowance:#066x}")],
                format!("{:#066x}", U256::ZERO)
            );
        }
    }
}

#[tokio::test]
async fn native_creation_prestate_matches_geth_with_code_disabled_and_prefunding() {
    let h = Harness::new();
    let api = debug!(h.st);
    let created = derive_address(VARIANT_ASSET, ALICE, w(900));
    for mux in [false, true] {
        for hide_code in [false, true] {
            for funded in [false, true] {
                for diff_mode in [false, true] {
                    let mut opts =
                        options(json!({"diffMode":diff_mode,"disableCode":hide_code}), mux);
                    if funded {
                        opts.state_overrides = Some(
                            [(
                                created,
                                AccountOverride {
                                    balance: Some(U256::from(9)),
                                    ..Default::default()
                                },
                            )]
                            .into_iter()
                            .collect(),
                        );
                    }
                    let trace = api
                        .debug_trace_call(
                            request(
                                FACTORY_ADDRESS,
                                encode_create(VARIANT_ASSET, w(900), ALICE, &[]),
                            ),
                            None,
                            Some(opts),
                        )
                        .await
                        .unwrap();
                    let result = prestate(trace, mux);
                    let pre = if diff_mode { &result["pre"] } else { &result };
                    assert_eq!(pre.get(format!("{created:#x}")).is_some(), funded, "{result}");
                    if funded {
                        assert_eq!(pre[format!("{created:#x}")]["balance"], "0x9");
                    }
                    if diff_mode {
                        let post = &result["post"][format!("{created:#x}")];
                        assert!(post["storage"].is_object());
                        assert_eq!(post["codeHash"], CAS20_CODE_HASH);
                        if hide_code {
                            assert!(post.get("code").is_none());
                        } else {
                            assert_eq!(post["code"], "0xef");
                        }
                    }
                    if hide_code {
                        assert!(!serde_json::to_string(&result).unwrap().contains("\"code\""));
                    }
                }
            }
        }
    }
}

#[tokio::test]
async fn empty_native_recipient_respects_include_empty() {
    let h = Harness::new();
    let api = debug!(h.st);
    let recipient = Address::repeat_byte(0x91);
    for mux in [false, true] {
        for value in [0, 1] {
            for include_empty in [None, Some(false), Some(true)] {
                for disable_storage in [false, true] {
                    let mut config = json!({"disableStorage": disable_storage});
                    if let Some(include_empty) = include_empty {
                        config["includeEmpty"] = include_empty.into();
                    }
                    let mut opts = options(config, mux);
                    opts.state_overrides = Some(
                        [(
                            ALICE,
                            AccountOverride {
                                balance: Some(U256::from(100)),
                                ..Default::default()
                            },
                        )]
                        .into_iter()
                        .collect(),
                    );
                    let mut req = request(recipient, vec![]);
                    req.value = Some(U256::from(value));
                    let result =
                        prestate(api.debug_trace_call(req, None, Some(opts)).await.unwrap(), mux);
                    let account = result.get(format!("{recipient:#x}"));
                    assert_eq!(
                        account.is_some(),
                        include_empty == Some(true),
                        "value={value}, mux={mux}, includeEmpty={include_empty:?}: {result}"
                    );
                    if let Some(account) = account {
                        assert_eq!(account, &json!({"balance":"0x0"}));
                    }
                    assert_eq!(result[format!("{ALICE:#x}")]["balance"], "0x64");
                }
            }
        }
    }
}

#[tokio::test]
async fn include_empty_preserves_sequential_call_prestate() {
    let h = Harness::new();
    let api = debug!(h.st);
    let recipient = Address::repeat_byte(0x92);
    for mux in [false, true] {
        let mut opts =
            options(json!({"includeEmpty":true,"disableStorage":true,"disableCode":true}), mux);
        opts.state_overrides = Some(
            [(ALICE, AccountOverride { balance: Some(U256::from(100)), ..Default::default() })]
                .into_iter()
                .collect(),
        );
        let mut req = request(recipient, vec![]);
        req.value = Some(U256::from(1));
        let bundles = vec![
            Bundle { transactions: vec![req.clone(), req.clone()], block_override: None },
            Bundle { transactions: vec![req], block_override: None },
        ];
        let results = api.debug_trace_call_many(bundles, None, Some(opts)).await.unwrap();
        assert_eq!(results.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
        for (index, trace) in results.into_iter().flatten().enumerate() {
            let result = prestate(trace, mux);
            assert_eq!(
                result[format!("{recipient:#x}")],
                json!({"balance":format!("0x{index:x}")})
            );
            assert_eq!(result[format!("{ALICE:#x}")]["balance"], format!("0x{:x}", 100 - index));
        }
    }
}

#[tokio::test]
async fn include_empty_with_diff_mode_is_rejected() {
    let h = Harness::new();
    let api = debug!(h.st);
    for mux in [false, true] {
        for (config, message) in [
            (json!({"includeEmpty":true,"diffMode":true}), "cannot use diffMode with includeEmpty"),
            (json!({"IncludeEmpty":true,"diffMode":true}), "cannot use diffMode with includeEmpty"),
            (json!({"includeEmpty":true,"DiffMode":true}), "cannot use diffMode with includeEmpty"),
            (json!({"includeEmpty":"true"}), "json: cannot unmarshal string into Go struct field PrestateTracerConfig.includeEmpty of type bool"),
            (json!([]), "json: cannot unmarshal array into Go value of type native.PrestateTracerConfig"),
            (json!([true]), "json: cannot unmarshal array into Go value of type native.PrestateTracerConfig"),
        ] {
            let err = api.debug_trace_call(request(ALICE, vec![]), None, Some(options(config, mux))).await.unwrap_err();
            assert_eq!(err.code(), -32000);
            assert_eq!(err.message(), message);
        }
    }
}

#[tokio::test]
async fn include_empty_retains_rejected_evm_creation_candidates() {
    let h = Harness::new();
    let api = debug!(h.st);
    let creator = Address::repeat_byte(0xc3);
    for (code, create2) in [("600060006001f05000", false), ("6000600060006001f55000", true)] {
        let candidate =
            if create2 { creator.create2_from_code(B256::ZERO, []) } else { creator.create(1) };
        for mux in [false, true] {
            let mut opts = options(json!({"includeEmpty":true}), mux);
            opts.state_overrides = Some(
                [(
                    creator,
                    AccountOverride {
                        balance: Some(U256::ZERO),
                        nonce: Some(1),
                        code: Some(code.parse().unwrap()),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            );
            let trace =
                api.debug_trace_call(request(creator, vec![]), None, Some(opts)).await.unwrap();
            assert_eq!(prestate(trace, mux)[format!("{candidate:#x}")], json!({"balance":"0x0"}));
        }
    }
}

#[tokio::test]
async fn prestate_option_casing_preserves_bsc_code_filtering() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = debug!(h.st);
    for mux in [false, true] {
        let trace = api
            .debug_trace_call(
                request(token, call_data(SEL_BALANCE_OF, &[a(ALICE)])),
                None,
                Some(options(
                    json!({"IncludeEmpty":true,"DisableCode":true,"DisableStorage":true}),
                    mux,
                )),
            )
            .await
            .unwrap();
        let pre = prestate(trace, mux);
        let account = pre.get(format!("{token:#x}")).unwrap();
        assert!(account.get("code").is_none());
        assert!(account.get("storage").is_none());
    }
}

#[tokio::test]
async fn native_call_gas_checks_omit_unobserved_empty_recipients() {
    let h = Harness::new();
    let api = debug!(h.st);
    let target = Address::with_last_byte(0x42);
    let recipient = Address::with_last_byte(0x44);
    for mux in [false, true] {
        for (gas, expected) in [(40_000, false), (100_000, true)] {
            let mut opts = options(json!({"includeEmpty":true}), mux);
            opts.state_overrides = Some(
                [(
                    target,
                    AccountOverride {
                        nonce: Some(1),
                        code: Some("60006000600060006001604461fffff15000".parse().unwrap()),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
            );
            let mut req = request(target, vec![]);
            req.gas = Some(gas);
            let result = prestate(api.debug_trace_call(req, None, Some(opts)).await.unwrap(), mux);
            assert_eq!(
                result.get(format!("{recipient:#x}")),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "gas={gas}: {result}"
            );
        }
    }
}

#[tokio::test]
async fn prestate_alias_order_preserves_bsc_code_and_empty_filtering() {
    let mut h = Harness::new();
    let token = token(&mut h);
    let api = debug!(h.st);
    for mux in [false, true] {
        for (config, hide_code) in [
            (
                r#"{"includeEmpty":false,"IncludeEmpty":true,"disableCode":false,"DisableCode":true}"#,
                true,
            ),
            (
                r#"{"IncludeEmpty":false,"includeEmpty":true,"DisableCode":true,"disableCode":false}"#,
                false,
            ),
        ] {
            let opts = options(serde_json::from_str(config).unwrap(), mux);
            let result = prestate(
                api.debug_trace_call(
                    request(token, call_data(SEL_BALANCE_OF, &[a(ALICE)])),
                    None,
                    Some(opts),
                )
                .await
                .unwrap(),
                mux,
            );
            assert_eq!(
                result[format!("{token:#x}")].get("code").is_none(),
                hide_code,
                "{config}: {result}"
            );
            assert_eq!(
                result.get(format!("{ALICE:#x}")),
                Some(&json!({"balance":"0x0"})),
                "{config}: {result}"
            );
        }
    }
}

#[tokio::test]
async fn native_selfdestruct_gas_checks_omit_unobserved_beneficiaries() {
    let h = Harness::new();
    let api = debug!(h.st);
    let parent = Address::with_last_byte(0x42);
    let child = Address::with_last_byte(0x43);
    let beneficiary = Address::with_last_byte(0x44);
    for mux in [false, true] {
        for (gas, expected) in [(19_000, false), (65_535, true)] {
            let mut opts = options(json!({"includeEmpty":true}), mux);
            opts.state_overrides = Some(
                [
                    (
                        parent,
                        AccountOverride {
                            nonce: Some(1),
                            code: Some(
                                format!("60006000600060006000604361{gas:04x}f15000")
                                    .parse()
                                    .unwrap(),
                            ),
                            ..Default::default()
                        },
                    ),
                    (
                        child,
                        AccountOverride {
                            nonce: Some(1),
                            balance: Some(U256::from(1)),
                            code: Some("6044ff".parse().unwrap()),
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            );
            let result = prestate(
                api.debug_trace_call(request(parent, vec![]), None, Some(opts)).await.unwrap(),
                mux,
            );
            assert_eq!(
                result.get(format!("{beneficiary:#x}")),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "gas={gas}: {result}"
            );
        }
    }
}

#[tokio::test]
async fn native_static_create2_bounds_empty_initcode_offset() {
    let h = Harness::new();
    let api = debug!(h.st);
    let parent = Address::with_last_byte(0x42);
    let child = Address::with_last_byte(0x43);
    let candidate = child.create2_from_code(B256::ZERO, []);
    for mux in [false, true] {
        for (offset, expected) in [(0x100000, true), (0x100001, false)] {
            let mut opts = options(json!({"includeEmpty":true}), mux);
            opts.state_overrides = Some(
                [
                    (
                        parent,
                        AccountOverride {
                            nonce: Some(1),
                            code: Some("60006000600060006043620f4240fa5000".parse().unwrap()),
                            ..Default::default()
                        },
                    ),
                    (
                        child,
                        AccountOverride {
                            nonce: Some(1),
                            code: Some(format!("6000600062{offset:06x}6000f5").parse().unwrap()),
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            );
            let result = prestate(
                api.debug_trace_call(request(parent, vec![]), None, Some(opts)).await.unwrap(),
                mux,
            );
            assert_eq!(
                result.get(format!("{candidate:#x}")),
                expected.then(|| json!({"balance":"0x0"})).as_ref(),
                "offset={offset:x}: {result}"
            );
        }
    }
}

#[tokio::test]
async fn native_read_after_failed_gas() {
    let h = Harness::new();
    let api = debug!(h.st);
    let parent = Address::with_last_byte(0x42);
    let child = Address::with_last_byte(0x43);
    let queried = "ca52000000000000000000000000000000000001".parse::<Address>().unwrap();
    let mut mismatches = Vec::new();
    for failed_first in [false, true] {
        let prefix = if failed_first { "600060006000600060006043614a38f150" } else { "" };
        let code = format!(
            "{prefix}366000600037602060006024600073{}61fffffa5060206000f3",
            alloy_primitives::hex::encode(FACTORY_ADDRESS)
        );
        let child_code =
            format!("6000600060006000600173{}61fffff15000", alloy_primitives::hex::encode(queried));
        for mux in [false, true] {
            let mut opts = options(json!({"includeEmpty":true}), mux);
            opts.state_overrides = Some(
                [
                    (
                        parent,
                        AccountOverride {
                            nonce: Some(1),
                            code: Some(code.parse().unwrap()),
                            ..Default::default()
                        },
                    ),
                    (
                        child,
                        AccountOverride {
                            nonce: Some(1),
                            code: Some(child_code.parse().unwrap()),
                            ..Default::default()
                        },
                    ),
                ]
                .into_iter()
                .collect(),
            );
            let trace = api
                .debug_trace_call(
                    request(parent, call_data(SEL_IS_CAS20_INITIALIZED, &[a(queried)])),
                    None,
                    Some(opts),
                )
                .await
                .unwrap();
            let pre = prestate(trace, mux);
            if pre.get(format!("{queried:#x}")) != Some(&json!({"balance":"0x0"})) {
                mismatches.push((failed_first, mux));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:?}");
}

#[tokio::test]
async fn go_bsc_native_prestate_matrix() {
    let cases: Vec<Value> = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/prestate/cases.json"
    )))
    .unwrap();
    let expected: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/prestate/expected.json"
    )))
    .unwrap();
    let mut mismatches = Vec::new();
    for case in cases {
        let h = Harness::new();
        let module = debug!(h.st).into_rpc();
        let mut overrides = case["accounts"].clone();
        for account in overrides.as_object_mut().unwrap().values_mut() {
            account["nonce"] = json!(format!("0x{:x}", account["nonce"].as_u64().unwrap()));
        }
        for mux in [false, true] {
            let name = format!("{}/mux={mux}", case["name"].as_str().unwrap());
            let config = case["config"].as_str().unwrap();
            let (tracer, config) = if mux {
                ("muxTracer", format!(r#"{{"prestateTracer":{config}}}"#))
            } else {
                ("prestateTracer", config.to_owned())
            };
            let call = json!({"from":Address::ZERO,"to":Address::with_last_byte(0x42),"gas":format!("0x{:x}",case["gas"].as_u64().unwrap()),"input":case["input"]});
            let coinbase = Address::with_last_byte(0x99);
            let request = format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"debug_traceCall","params":[{call},"latest",{{"tracer":"{tracer}","tracerConfig":{config},"stateOverrides":{overrides},"blockOverrides":{{"coinbase":"{coinbase:#x}"}}}}]}}"#
            );
            let (response, _) = module.raw_json_request(&request, 1).await.unwrap();
            let response: Value = serde_json::from_str(response.get()).unwrap();
            assert!(response.get("error").is_none(), "{name}: {response}");
            let actual =
                if mux { &response["result"]["prestateTracer"] } else { &response["result"] };
            if actual != &expected[&name] {
                mismatches.push(format!("{name}: expected {}, got {actual}", expected[&name]));
            }
        }
    }
    assert!(mismatches.is_empty(), "{}", mismatches.join("\n"));
}
