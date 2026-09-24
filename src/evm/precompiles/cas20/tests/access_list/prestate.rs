use super::*;
use crate::rpc::prestate::{BscPrestateApiImpl, BscPrestateApiServer};
use alloy_rpc_types::trace::geth::{GethDebugTracingCallOptions, GethTrace};
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
