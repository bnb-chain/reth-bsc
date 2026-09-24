use super::*;
use crate::evm::precompiles::cas20::FACTORY_ADDRESS;
use alloy_primitives::Address;
use alloy_rpc_types::trace::geth::{DiffMode, PreStateMode};
use serde_json::json;

#[test]
fn empty_filter_preserves_existing_code_and_non_cas20_accounts() {
    let existing =
        Address::from_slice(&alloy_primitives::hex!("ca52000000000000000000000000000000000001"));
    let empty =
        Address::from_slice(&alloy_primitives::hex!("ca52000000000000000000000000000000000002"));
    let other = Address::repeat_byte(0x11);
    let mut trace = GethTrace::PreStateTracer(PreStateFrame::Default(PreStateMode(
        [
            (
                existing,
                AccountState { code: Some(Bytes::from_static(&[0xef])), ..Default::default() },
            ),
            (empty, AccountState::default()),
            (other, AccountState::default()),
        ]
        .into_iter()
        .collect(),
    )));
    Plan::Prestate { hide_code: true, include_empty: false }.apply(&mut trace);
    let value = serde_json::to_value(trace).unwrap();
    assert!(value.get(format!("{existing:#x}")).is_some());
    assert!(value[format!("{existing:#x}")].get("code").is_none());
    assert!(value.get(format!("{empty:#x}")).is_none());
    assert!(value.get(format!("{other:#x}")).is_some());
}

#[test]
fn include_empty_and_invalid_tracer_config_are_preserved() {
    let mut opts: GethDebugTracingOptions = serde_json::from_value(json!({"tracer":"prestateTracer","tracerConfig":{"diffMode":true,"disableCode":true,"includeEmpty":true}})).unwrap();
    let plan = Plan::prepare(&mut opts);
    assert_eq!(opts.tracer_config.0["disableCode"], false);
    let mut trace = GethTrace::PreStateTracer(PreStateFrame::Diff(DiffMode {
        pre: [(FACTORY_ADDRESS, AccountState::default())].into_iter().collect(),
        ..Default::default()
    }));
    plan.apply(&mut trace);
    assert!(serde_json::to_value(trace).unwrap()["pre"]
        .get(format!("{FACTORY_ADDRESS:#x}"))
        .is_some());
    opts.tracer_config.0 = json!({"disableCode":"invalid"});
    let original = opts.clone();
    assert!(matches!(Plan::prepare(&mut opts), Plan::None));
    assert_eq!(opts, original);
}
