//! Behavioural tests for the CAS20 family, driven over a mock host state so gas
//! and storage can be inspected directly, plus EVM-level tests for the routing.
//! The scenarios mirror go-bsc's core/vm/cas20_*_test.go.

use super::{
    abi::*,
    errors::{Exit, Outcome},
    factory::derive_address,
    permit::{domain_separator, ecrecover_address},
    policy::{ALWAYS_ALLOW, ALWAYS_BLOCK, TYPE_ALLOWLIST, TYPE_BLOCKLIST, TYPE_UNION},
    resolve,
    sigs::*,
    storage::{addr_key, erc7201_root, mapping_slot, slot_at, SLOT_BALANCES},
    test_host::{run_call, CallSpec, MockHost},
    token::PAUSE_TRANSFER,
    *,
};
use alloy_evm::precompiles::PrecompileLookup;
use alloy_primitives::{keccak256, Address, Log, B256, U256};
use std::collections::HashMap;

const ADMIN: Address =
    Address::new([0x60, 0xfe, 0xed, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
const ALICE: Address =
    Address::new([0xa1, 0x1c, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
const BOB: Address =
    Address::new([0xb0, 0xb0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
const NOW: u64 = 1_800_000_000;
const GAS: u64 = 5_000_000;

struct Out {
    exit: Exit,
    used: u64,
    refund: i64,
    logs: Vec<Log>,
}

impl Out {
    fn ret(&self) -> &[u8] {
        match &self.exit {
            Exit::Return(b) => b,
            other => panic!("expected a return, got {other:?}"),
        }
    }

    fn revert(&self) -> &[u8] {
        match &self.exit {
            Exit::Revert(b) => b,
            other => panic!("expected a revert, got {other:?}"),
        }
    }

    fn word(&self) -> B256 {
        B256::from_slice(self.ret())
    }

    fn u256(&self) -> U256 {
        U256::from_be_bytes(self.word().0)
    }
}

struct Harness {
    st: MockHost,
}

impl Harness {
    /// The fork plants the sentinels; opening the features and appointing the admin
    /// stay local, since the fork opens nothing (BEP-702 3.15). The seed counts as
    /// committed state here.
    fn new() -> Self {
        let mut st = MockHost::new(714, NOW);
        st.seed_activation(ADMIN);
        st.finalize();
        Self { st }
    }

    #[allow(clippy::too_many_arguments)]
    fn call_opts(
        &mut self,
        caller: Address,
        to: Address,
        input: &[u8],
        gas: u64,
        is_static: bool,
        direct: bool,
        value: U256,
    ) -> Out {
        let r =
            run_call(&mut self.st, CallSpec { caller, to, gas, is_static, direct, value }, input);
        let exit = match r.outcome {
            Outcome::Return(b) => Exit::Return(b),
            Outcome::Revert(b) => Exit::Revert(b),
            Outcome::OutOfGas => Exit::OutOfGas,
            Outcome::Fatal(msg) => panic!("fatal: {msg}"),
        };
        Out { exit, used: r.used, refund: r.refund, logs: r.logs }
    }

    /// A transaction boundary: what was written is now committed, and nothing is warm.
    fn finalize(&mut self) {
        self.st.finalize();
    }

    fn call(&mut self, caller: Address, to: Address, input: &[u8]) -> Out {
        self.call_opts(caller, to, input, GAS, false, true, U256::ZERO)
    }

    fn create(
        &mut self,
        creator: Address,
        variant: u8,
        salt: u64,
        admin: Address,
        calls: &[Vec<u8>],
    ) -> Address {
        let out =
            self.call(creator, FACTORY_ADDRESS, &encode_create(variant, w(salt), admin, calls));
        Address::from_word(out.word())
    }
}

fn w(n: u64) -> B256 {
    B256::from(U256::from(n))
}

fn a(addr: Address) -> B256 {
    addr_key(addr)
}

fn u256_of(b: B256) -> U256 {
    U256::from_be_bytes(b.0)
}

fn call_data(sel: Selector, words: &[B256]) -> Vec<u8> {
    let mut out = sel.to_vec();
    for x in words {
        out.extend_from_slice(x.as_slice());
    }
    out
}

fn u8_array_call(sel: Selector, vals: &[u8]) -> Vec<u8> {
    let words: Vec<B256> = vals.iter().map(|&v| w(v as u64)).collect();
    let mut out = sel.to_vec();
    out.extend(encode_tuple(&[abi_word_array(&words)]));
    out
}

fn asset_params(name: &[u8], symbol: &[u8], admin: Address, decimals: u8) -> Vec<u8> {
    abi_encode_struct(&[
        abi_word(w(1)),
        abi_string(name),
        abi_string(symbol),
        abi_word(a(admin)),
        abi_word(w(decimals as u64)),
    ])
}

fn stablecoin_params(name: &[u8], symbol: &[u8], admin: Address, currency: &[u8]) -> Vec<u8> {
    abi_encode_struct(&[
        abi_word(w(1)),
        abi_string(name),
        abi_string(symbol),
        abi_word(a(admin)),
        abi_string(currency),
    ])
}

fn encode_create(variant: u8, salt: B256, admin: Address, calls: &[Vec<u8>]) -> Vec<u8> {
    let params = if variant == VARIANT_STABLECOIN {
        stablecoin_params(b"Test Stable", b"TS", admin, b"USD")
    } else {
        asset_params(b"Test Token", b"TT", admin, 18)
    };
    encode_create_with_params(variant, salt, &params, calls)
}

fn encode_create_with_params(variant: u8, salt: B256, params: &[u8], calls: &[Vec<u8>]) -> Vec<u8> {
    let elems: Vec<Vec<u8>> = calls
        .iter()
        .map(|c| {
            let mut e = w(c.len() as u64).to_vec();
            e.extend_from_slice(c);
            e.resize(32 + c.len().div_ceil(32) * 32, 0);
            e
        })
        .collect();
    let mut arr = w(calls.len() as u64).to_vec();
    // element offsets are relative to just after the length word
    let mut cur = 32 * calls.len();
    for e in &elems {
        arr.extend_from_slice(w(cur as u64).as_slice());
        cur += e.len();
    }
    for e in &elems {
        arr.extend_from_slice(e);
    }
    let mut out = SEL_CREATE_CAS20.to_vec();
    out.extend(encode_tuple(&[
        abi_word(w(variant as u64)),
        abi_word(salt),
        abi_bytes(params),
        AbiPart::Dynamic(arr),
    ]));
    out
}

fn assert_sel(data: &[u8], sel: Selector) {
    assert!(data.len() >= 4, "revert data too short: {data:x?}");
    assert_eq!(&data[..4], &sel, "revert selector");
}

fn assert_rev(data: &[u8], sel: Selector, words: &[B256]) {
    assert_sel(data, sel);
    assert_eq!(data.len(), 4 + 32 * words.len(), "revert data length");
    for (i, x) in words.iter().enumerate() {
        assert_eq!(&data[4 + 32 * i..4 + 32 * (i + 1)], x.as_slice(), "revert word {i}");
    }
}

// --- routing ------------------------------------------------------------------

#[test]
fn addresses_route_by_prefix_and_singleton() {
    let asset = derive_address(VARIANT_ASSET, ALICE, w(1));
    let stable = derive_address(VARIANT_STABLECOIN, ALICE, w(1));
    assert!(is_cas20_address(asset) && is_cas20_address(stable));
    assert_eq!(&asset[..2], &MARKER_PREFIX);
    assert_eq!(asset[10], VARIANT_ASSET);
    assert_eq!(stable[10], VARIANT_STABLECOIN);
    assert_eq!(resolve(asset), Some(Kind::Asset));
    assert_eq!(resolve(stable), Some(Kind::Stablecoin));
    assert_eq!(resolve(FACTORY_ADDRESS), Some(Kind::Factory));
    assert_eq!(resolve(POLICY_REGISTRY_ADDRESS), Some(Kind::Policy));
    assert_eq!(resolve(ACTIVATION_REGISTRY_ADDRESS), Some(Kind::Activation));

    // An unknown variant ordinal is in the space but resolves to nothing.
    let mut unknown = asset;
    unknown.0[10] = 0x07;
    assert!(is_cas20_address(unknown) && resolve(unknown).is_none());
    // A dirty reserved byte leaves the space.
    let mut outside = asset;
    outside.0[5] = 1;
    assert!(!is_cas20_address(outside) && resolve(outside).is_none());
    assert!(!is_cas20_routed(ALICE));
    assert!(is_cas20_routed(FACTORY_ADDRESS) && is_cas20_routed(asset));
    let jenner = Cas20Lookup::new(crate::hardforks::bsc::BscHardfork::Jenner);
    assert!(jenner.lookup(&asset).is_some() && jenner.lookup(&ALICE).is_none());
    assert!(jenner.lookup(&unknown).is_none() && !is_cas20_routed(unknown));
    assert!(!Cas20Lookup::new(crate::hardforks::bsc::BscHardfork::Pasteur).is_active());
}

// --- the layout fixture ---------------------------------------------------------

#[test]
fn layout_fixture_follows_the_code() {
    use super::{activation, asset, policy, stablecoin, storage};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("testdata/cas20_layout.json")).expect("valid fixture");
    let roots: HashMap<&str, B256> = [
        (storage::NAMESPACE, ROOT_CORE),
        (asset::NAMESPACE, ROOT_ASSET),
        (stablecoin::NAMESPACE, ROOT_STABLECOIN),
        (policy::NAMESPACE, ROOT_POLICY),
        (activation::NAMESPACE, ROOT_ACTIVATION),
    ]
    .into_iter()
    .collect();
    let slots: HashMap<(&str, &str), u64> = [
        (("bsc.cas20", "name"), storage::SLOT_NAME),
        (("bsc.cas20", "symbol"), storage::SLOT_SYMBOL),
        (("bsc.cas20", "contractURI"), storage::SLOT_CONTRACT_URI),
        (("bsc.cas20", "totalSupply"), storage::SLOT_TOTAL_SUPPLY),
        (("bsc.cas20", "balances"), storage::SLOT_BALANCES),
        (("bsc.cas20", "allowances"), storage::SLOT_ALLOWANCES),
        (("bsc.cas20", "roles"), storage::SLOT_ROLES),
        (("bsc.cas20", "roleAdmins"), storage::SLOT_ROLE_ADMINS),
        (("bsc.cas20", "adminCount"), storage::SLOT_ADMIN_COUNT),
        (("bsc.cas20", "transferPolicies"), storage::SLOT_TRANSFER_POLICIES),
        (("bsc.cas20", "mintPolicy"), storage::SLOT_MINT_POLICY),
        (("bsc.cas20", "paused"), storage::SLOT_PAUSED),
        (("bsc.cas20", "supplyCap"), storage::SLOT_SUPPLY_CAP),
        (("bsc.cas20", "nonces"), storage::SLOT_NONCES),
        (("bsc.cas20", "seizePolicies"), storage::SLOT_SEIZE_POLICIES),
        (("bsc.cas20.asset", "decimals"), asset::SLOT_DECIMALS),
        (("bsc.cas20.asset", "multiplier"), asset::SLOT_MULTIPLIER),
        (("bsc.cas20.asset", "announcements"), asset::SLOT_ANNOUNCEMENTS),
        (("bsc.cas20.asset", "extraMetadata"), asset::SLOT_EXTRA_META),
        (("bsc.cas20.asset", "pendingMultiplier"), asset::SLOT_PENDING),
        (("bsc.cas20.stablecoin", "currency"), stablecoin::SLOT_CURRENCY),
        (("bsc.policy_registry", "policies"), policy::SLOT_POLICIES),
        (("bsc.policy_registry", "members"), policy::SLOT_MEMBERS),
        (("bsc.policy_registry", "pendingAdmins"), policy::SLOT_PENDING_ADMINS),
        (("bsc.policy_registry", "counter"), policy::SLOT_COUNTER),
        (("bsc.policy_registry", "children"), policy::SLOT_CHILDREN),
        (("bsc.activation_registry", "features"), activation::SLOT_FEATURES),
        (("bsc.activation_registry", "admin"), activation::SLOT_ADMIN),
    ]
    .into_iter()
    .collect();

    let namespaces = fixture["namespaces"].as_array().unwrap();
    assert_eq!(namespaces.len(), roots.len(), "every namespace is covered");
    let mut seen = 0;
    for ns in namespaces {
        let name = ns["name"].as_str().unwrap();
        let root: B256 = ns["root"].as_str().unwrap().parse().unwrap();
        assert_eq!(erc7201_root(name), root, "root of {name}");
        assert_eq!(roots[name], root, "constant for {name}");
        for f in ns["fields"].as_array().unwrap() {
            let field = f["name"].as_str().unwrap();
            let slot = f["slot"].as_u64().unwrap();
            assert_eq!(slots[&(name, field)], slot, "{name}.{field}");
            seen += 1;
        }
    }
    assert_eq!(seen, slots.len(), "every field constant is in the fixture");
    assert_eq!(fixture["derivation"]["string_max_len"].as_u64().unwrap(), storage::MAX_STRING_LEN);
}

// --- the factory and the ERC-20 surface -----------------------------------------

#[test]
fn factory_creates_a_token_that_transfers() {
    let mut h = Harness::new();
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        1,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_MINT, &[a(ALICE), w(1000)]),
        ],
    );
    assert_eq!(token, derive_address(VARIANT_ASSET, ALICE, w(1)));
    assert_eq!(h.st.code_hash_of(token), Some(MARKER_CODE_HASH));

    assert_eq!(h.call(BOB, token, &call_data(SEL_NAME, &[])).ret(), enc_string(b"Test Token"));
    assert_eq!(h.call(BOB, token, &call_data(SEL_SYMBOL, &[])).ret(), enc_string(b"TT"));
    assert_eq!(h.call(BOB, token, &call_data(SEL_DECIMALS, &[])).u256(), U256::from(18));
    assert_eq!(h.call(BOB, token, &call_data(SEL_TOTAL_SUPPLY, &[])).u256(), U256::from(1000));
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_BALANCE_OF, &[a(ALICE)])).u256(),
        U256::from(1000)
    );
    assert_eq!(h.call(BOB, token, &call_data(SEL_SUPPLY_CAP, &[])).u256(), NO_SUPPLY_CAP);
    assert!(
        h.call(BOB, token, &call_data(SEL_HAS_ROLE, &[ROLE_DEFAULT_ADMIN, a(ALICE)])).ret()
            == enc_bool(true)
    );

    let out = h.call(ALICE, token, &call_data(SEL_TRANSFER, &[a(BOB), w(400)]));
    assert_eq!(out.ret(), enc_bool(true));
    assert_eq!(out.logs.len(), 1);
    assert_eq!(out.logs[0].address, token);
    assert_eq!(out.logs[0].topics(), &[TOPIC_TRANSFER, a(ALICE), a(BOB)]);
    assert_eq!(out.logs[0].data.data.as_ref(), w(400).as_slice());
    assert_eq!(h.call(BOB, token, &call_data(SEL_BALANCE_OF, &[a(ALICE)])).u256(), U256::from(600));
    assert_eq!(h.call(BOB, token, &call_data(SEL_BALANCE_OF, &[a(BOB)])).u256(), U256::from(400));
    // The balance mapping lives where BEP-702 3.17 says it does.
    assert_eq!(h.st.get(token, mapping_slot(slot_at(SLOT_BALANCES), a(BOB))), U256::from(400));

    let out = h.call(BOB, token, &call_data(SEL_TRANSFER, &[a(ALICE), w(401)]));
    assert_rev(out.revert(), ERR_INSUFFICIENT_BALANCE, &[a(BOB), w(400), w(401)]);
    let out = h.call(BOB, token, &call_data(SEL_TRANSFER, &[a(Address::ZERO), w(1)]));
    assert_rev(out.revert(), ERR_INVALID_RECEIVER, &[a(Address::ZERO)]);

    // A repeated creation at the same salt finds the address occupied.
    let out = h.call(ALICE, FACTORY_ADDRESS, &encode_create(VARIANT_ASSET, w(1), ALICE, &[]));
    assert_rev(out.revert(), ERR_TOKEN_ALREADY_EXISTS, &[a(token)]);
    // getCAS20Address predicts what createCAS20 derives.
    let out =
        h.call(BOB, FACTORY_ADDRESS, &call_data(SEL_GET_CAS20_ADDRESS, &[w(0), a(ALICE), w(1)]));
    assert_eq!(out.word(), a(token));
    assert_eq!(
        h.call(BOB, FACTORY_ADDRESS, &call_data(SEL_IS_CAS20_INITIALIZED, &[a(token)])).ret(),
        enc_bool(true)
    );
    assert_eq!(
        h.call(BOB, FACTORY_ADDRESS, &call_data(SEL_VARIANT_OF, &[a(token)])).u256(),
        U256::ZERO
    );
}

#[test]
fn approvals_and_transfer_from() {
    let mut h = Harness::new();
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        2,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_MINT, &[a(ALICE), w(100)]),
        ],
    );
    let out = h.call(ALICE, token, &call_data(SEL_APPROVE, &[a(BOB), w(30)]));
    assert_eq!(out.ret(), enc_bool(true));
    assert_eq!(out.logs[0].topics(), &[TOPIC_APPROVAL, a(ALICE), a(BOB)]);
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_ALLOWANCE, &[a(ALICE), a(BOB)])).u256(),
        U256::from(30)
    );

    let out = h.call(BOB, token, &call_data(SEL_TRANSFER_FROM, &[a(ALICE), a(BOB), w(31)]));
    assert_rev(out.revert(), ERR_INSUFFICIENT_ALLOWANCE, &[a(BOB), w(30), w(31)]);
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_TRANSFER_FROM, &[a(ALICE), a(BOB), w(10)])).ret(),
        enc_bool(true)
    );
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_ALLOWANCE, &[a(ALICE), a(BOB)])).u256(),
        U256::from(20)
    );

    // An infinite allowance is not drawn down.
    h.call(ALICE, token, &call_data(SEL_APPROVE, &[a(BOB), B256::repeat_byte(0xff)]));
    h.call(BOB, token, &call_data(SEL_TRANSFER_FROM, &[a(ALICE), a(BOB), w(10)]));
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_ALLOWANCE, &[a(ALICE), a(BOB)])).u256(),
        U256::MAX
    );

    // Clearing a committed allowance earns the EIP-3529 refund; clearing one this
    // transaction created earns back what its creation cost instead.
    let out = h.call(ALICE, token, &call_data(SEL_APPROVE, &[a(BOB), w(0)]));
    assert_eq!(out.refund, 20000 - 100);
    h.call(ALICE, token, &call_data(SEL_APPROVE, &[a(BOB), w(5)]));
    h.finalize();
    let out = h.call(ALICE, token, &call_data(SEL_APPROVE, &[a(BOB), w(0)]));
    assert_eq!(out.refund, 4800);
}

#[test]
fn call_forms_are_refused_as_typed_errors() {
    let mut h = Harness::new();
    let token = h.create(ALICE, VARIANT_ASSET, 3, ALICE, &[]);
    let transfer = call_data(SEL_TRANSFER, &[a(BOB), w(0)]);

    // STATICCALL: a typed revert at entry, remaining gas returned.
    let out = h.call_opts(ALICE, token, &transfer, GAS, true, true, U256::ZERO);
    assert_rev(out.revert(), ERR_STATIC_CALL_NOT_ALLOWED, &[]);
    assert!(out.used < GAS);
    // Reads work in a static frame.
    let out = h.call_opts(
        ALICE,
        token,
        &call_data(SEL_BALANCE_OF, &[a(ALICE)]),
        GAS,
        true,
        true,
        U256::ZERO,
    );
    assert_eq!(out.u256(), U256::ZERO);

    // DELEGATECALL / CALLCODE.
    let out = h.call_opts(ALICE, token, &transfer, GAS, false, false, U256::ZERO);
    assert_rev(out.revert(), ERR_DELEGATE_CALL_NOT_ALLOWED, &[]);
    // Value.
    let out = h.call_opts(ALICE, token, &transfer, GAS, false, true, U256::from(1));
    assert_rev(out.revert(), ERR_NON_PAYABLE, &[]);
    // Malformed calldata and unknown selectors revert empty.
    assert_eq!(h.call(ALICE, token, &[0x12, 0x34, 0x56, 0x78]).revert(), b"");
    assert_eq!(h.call(ALICE, token, &SEL_TRANSFER[..3]).revert(), b"");
    assert_eq!(h.call(ALICE, token, &call_data(SEL_TRANSFER, &[a(BOB)])).revert(), b"");
    // A dirty address word is a malformed encoding, not a value.
    let mut dirty = call_data(SEL_BALANCE_OF, &[a(BOB)]);
    dirty[4] = 1;
    assert_eq!(h.call(ALICE, token, &dirty).revert(), b"");
    // A token address nothing created: the existence check reverts empty.
    let ghost = derive_address(VARIANT_ASSET, BOB, w(99));
    assert_eq!(h.call(ALICE, ghost, &call_data(SEL_NAME, &[])).revert(), b"");
}

#[test]
fn out_of_gas_halts_and_typed_reverts_do_not() {
    let mut h = Harness::new();
    let token = h.create(ALICE, VARIANT_ASSET, 4, ALICE, &[]);
    let out = h.call_opts(ALICE, token, &call_data(SEL_NAME, &[]), 100, false, true, U256::ZERO);
    assert_eq!(out.exit, Exit::OutOfGas);
    assert_eq!(out.used, 100, "an exhausted frame has consumed everything");
    // Below the EIP-2200 sentry no write proceeds.
    let out = h.call_opts(
        ALICE,
        token,
        &call_data(SEL_APPROVE, &[a(BOB), w(1)]),
        2_300,
        false,
        true,
        U256::ZERO,
    );
    assert_eq!(out.exit, Exit::OutOfGas);
}

#[test]
fn internal_dispatch_is_charged_per_entry() {
    // Mirrors go-bsc's TestCAS20InternalDispatchIsCharged: one more constant getter
    // in the bundle costs its dispatch (100 + 6) plus its three outer calldata words.
    let mut h = Harness::new();
    let getter = call_data(SEL_DEFAULT_ADMIN_ROLE, &[]);
    // Warm the shared slots first so the two measured creations start alike.
    h.create(ALICE, VARIANT_ASSET, 10, ALICE, &[]);
    let base = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(VARIANT_ASSET, w(11), ALICE, &vec![getter.clone(); 3]),
    );
    let more = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(VARIANT_ASSET, w(12), ALICE, &vec![getter.clone(); 4]),
    );
    base.ret();
    more.ret();
    assert_eq!(more.used - base.used, 100 + 6 + 3 * 6);

    // A bundle the budget cannot finish is out of gas, not silently truncated.
    let out = h.call_opts(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(VARIANT_ASSET, w(13), ALICE, &vec![getter; 200]),
        base.used,
        false,
        true,
        U256::ZERO,
    );
    assert_eq!(out.exit, Exit::OutOfGas);
    assert_eq!(
        h.call(
            BOB,
            FACTORY_ADDRESS,
            &call_data(SEL_IS_CAS20_INITIALIZED, &[a(derive_address(VARIANT_ASSET, ALICE, w(13)))])
        )
        .ret(),
        enc_bool(false)
    );
}

#[test]
fn init_calls_are_dispatched_by_the_variant_and_failures_are_indexed() {
    let mut h = Harness::new();
    // An Asset bundle can set its multiplier at creation.
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        20,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_OPERATOR, a(ALICE)]),
            call_data(SEL_UPDATE_MULTIPLIER, &[w(2_000_000_000_000_000_000)]),
        ],
    );
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_MULTIPLIER, &[])).u256(),
        U256::from(2_000_000_000_000_000_000u64)
    );
    // The bootstrap frame is privileged: a mint needs no role there, but the
    // receiver checks still apply. The failing entry is named; the whole creation is undone.
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(
            VARIANT_ASSET,
            w(21),
            ALICE,
            &[
                call_data(SEL_MINT, &[a(ALICE), w(1)]),
                call_data(SEL_MINT, &[a(Address::ZERO), w(1)]),
            ],
        ),
    );
    assert_rev(out.revert(), ERR_INIT_CALL_FAILED, &[w(1)]);
    assert!(h.st.code_hash_of(derive_address(VARIANT_ASSET, ALICE, w(21))).is_none());
    // A too-short entry is malformed.
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(VARIANT_ASSET, w(22), ALICE, &[vec![1, 2, 3]]),
    );
    assert_sel(out.revert(), ERR_INTERNAL_CALL_MALFORMED);
    // Without an initial admin the bootstrap may still configure roles, but not after
    // renouncing inside the same bundle.
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(
            VARIANT_ASSET,
            w(23),
            ALICE,
            &[
                call_data(SEL_RENOUNCE_LAST_ADMIN, &[]),
                call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            ],
        ),
    );
    assert_rev(out.revert(), ERR_INIT_CALL_FAILED, &[w(1)]);
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create(
            VARIANT_ASSET,
            w(24),
            Address::ZERO,
            &[call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(BOB)])],
        ),
    );
    out.ret();
}

#[test]
fn stablecoins_have_fixed_decimals_and_a_currency() {
    let mut h = Harness::new();
    let token = h.create(ALICE, VARIANT_STABLECOIN, 30, ALICE, &[]);
    assert_eq!(token[10], VARIANT_STABLECOIN);
    assert_eq!(h.call(BOB, token, &call_data(SEL_DECIMALS, &[])).u256(), U256::from(6));
    assert_eq!(h.call(BOB, token, &call_data(SEL_CURRENCY, &[])).ret(), enc_string(b"USD"));
    assert_eq!(h.call(BOB, token, &call_data(SEL_NAME, &[])).ret(), enc_string(b"Test Stable"));
    // No multiplier surface.
    assert_eq!(h.call(BOB, token, &call_data(SEL_MULTIPLIER, &[])).revert(), b"");

    let bad = stablecoin_params(b"S", b"S", ALICE, b"usd");
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create_with_params(VARIANT_STABLECOIN, w(31), &bad, &[]),
    );
    assert_sel(out.revert(), ERR_INVALID_CURRENCY);
    let empty = stablecoin_params(b"S", b"S", ALICE, b"");
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create_with_params(VARIANT_STABLECOIN, w(32), &empty, &[]),
    );
    assert_sel(out.revert(), ERR_MISSING_REQUIRED_FIELD);
    let bad_decimals = asset_params(b"A", b"A", ALICE, 19);
    let out = h.call(
        ALICE,
        FACTORY_ADDRESS,
        &encode_create_with_params(VARIANT_ASSET, w(33), &bad_decimals, &[]),
    );
    assert_rev(out.revert(), ERR_INVALID_DECIMALS, &[w(19)]);
}

// --- activation ------------------------------------------------------------------

#[test]
fn activation_gates_creation_and_governance_appoints_the_admin() {
    let mut h = Harness::new();
    assert_eq!(
        h.call(BOB, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_ACTIVATION_ADMIN, &[])).word(),
        a(ADMIN)
    );
    let out =
        h.call(BOB, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_DEACTIVATE, &[FEATURE_ASSET]));
    assert_rev(out.revert(), ERR_UNAUTHORIZED_ADDR, &[a(BOB)]);

    let out =
        h.call(ADMIN, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_DEACTIVATE, &[FEATURE_ASSET]));
    out.ret();
    assert_eq!(out.logs[0].topics(), &[TOPIC_FEATURE_DEACTIVATED, FEATURE_ASSET, a(ADMIN)]);
    assert_eq!(
        h.call(BOB, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_IS_ACTIVATED, &[FEATURE_ASSET]))
            .ret(),
        enc_bool(false)
    );
    let out = h.call(ALICE, FACTORY_ADDRESS, &encode_create(VARIANT_ASSET, w(40), ALICE, &[]));
    assert_rev(out.revert(), ERR_FEATURE_NOT_ACTIVATED, &[FEATURE_ASSET]);
    // The other variant is unaffected.
    h.create(ALICE, VARIANT_STABLECOIN, 40, ALICE, &[]);
    let out =
        h.call(ADMIN, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_DEACTIVATE, &[FEATURE_ASSET]));
    assert_rev(out.revert(), ERR_FEATURE_NOT_ACTIVATED, &[FEATURE_ASSET]);
    h.call(ADMIN, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_ACTIVATE, &[FEATURE_ASSET])).ret();
    let out =
        h.call(ADMIN, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_ACTIVATE, &[FEATURE_ASSET]));
    assert_rev(out.revert(), ERR_ALREADY_ACTIVATED, &[FEATURE_ASSET]);

    // Only GovHub appoints, with the system-contract shape of error.
    let mut update = SEL_UPDATE_PARAM.to_vec();
    update.extend(encode_tuple(&[abi_string(b"admin"), abi_bytes(BOB.as_slice())]));
    let out = h.call(ADMIN, ACTIVATION_REGISTRY_ADDRESS, &update);
    assert_rev(out.revert(), ERR_UNAUTHORIZED_ADDR, &[a(ADMIN)]);
    let out = h.call(activation::GOV_HUB_ADDRESS, ACTIVATION_REGISTRY_ADDRESS, &update);
    out.ret();
    assert_eq!(out.logs.len(), 2);
    assert_eq!(
        out.logs[0].topics(),
        &[TOPIC_ADMIN_CHANGED, a(ADMIN), a(BOB), a(activation::GOV_HUB_ADDRESS)]
    );
    assert_eq!(out.logs[1].topics(), &[TOPIC_PARAM_CHANGE]);
    assert_eq!(
        h.call(BOB, ACTIVATION_REGISTRY_ADDRESS, &call_data(SEL_ACTIVATION_ADMIN, &[])).word(),
        a(BOB)
    );
    let mut unknown = SEL_UPDATE_PARAM.to_vec();
    unknown.extend(encode_tuple(&[abi_string(b"other"), abi_bytes(BOB.as_slice())]));
    let out = h.call(activation::GOV_HUB_ADDRESS, ACTIVATION_REGISTRY_ADDRESS, &unknown);
    assert_sel(out.revert(), ERR_UNKNOWN_PARAM);
    let mut short = SEL_UPDATE_PARAM.to_vec();
    short.extend(encode_tuple(&[abi_string(b"admin"), abi_bytes(&[1, 2, 3])]));
    let out = h.call(activation::GOV_HUB_ADDRESS, ACTIVATION_REGISTRY_ADDRESS, &short);
    assert_sel(out.revert(), ERR_INVALID_VALUE);
}

// --- policies -----------------------------------------------------------------------

#[test]
fn policies_bind_to_tokens_and_block_transfers() {
    let mut h = Harness::new();
    let reg = POLICY_REGISTRY_ADDRESS;
    let is_auth = |h: &mut Harness, id: u64, who: Address| -> bool {
        h.call(BOB, reg, &call_data(SEL_IS_AUTHORIZED, &[w(id), a(who)])).ret() == enc_bool(true)
    };
    assert!(is_auth(&mut h, ALWAYS_ALLOW, ALICE) && !is_auth(&mut h, ALWAYS_BLOCK, ALICE));

    let out =
        h.call(ADMIN, reg, &call_data(SEL_CREATE_POLICY, &[a(ADMIN), w(TYPE_BLOCKLIST as u64)]));
    let block_id = out.u256().to::<u64>();
    assert_eq!(block_id, 2, "the first user policy follows the two sentinels");
    assert_eq!(out.logs[0].topics(), &[TOPIC_POLICY_CREATED, w(block_id), a(ADMIN)]);
    // updateBlocklist(uint64,bool,address[]): head = id, bool, offset(0x60); tail = array.
    let mut add = SEL_UPDATE_BLOCKLIST.to_vec();
    add.extend(encode_tuple(&[abi_word(w(block_id)), abi_word(w(1)), abi_word_array(&[a(ALICE)])]));
    let out = h.call(BOB, reg, &add);
    assert_rev(out.revert(), ERR_UNAUTHORIZED, &[]);
    h.call(ADMIN, reg, &add).ret();
    assert!(!is_auth(&mut h, block_id, ALICE) && is_auth(&mut h, block_id, BOB));

    let allow_id = (TYPE_ALLOWLIST as u64) << 56 | 3;
    let mut create_allow = SEL_CREATE_POLICY_WITH_ACCOUNTS.to_vec();
    create_allow.extend(encode_tuple(&[
        abi_word(a(ADMIN)),
        abi_word(w(TYPE_ALLOWLIST as u64)),
        abi_word_array(&[a(BOB)]),
    ]));
    assert_eq!(h.call(ADMIN, reg, &create_allow).u256(), U256::from(allow_id));
    assert!(is_auth(&mut h, allow_id, BOB) && !is_auth(&mut h, allow_id, ALICE));

    // A union of the two authorizes whoever either does.
    let mut create_union = SEL_CREATE_COMPOSITE.to_vec();
    create_union.extend(encode_tuple(&[
        abi_word(a(ADMIN)),
        abi_word(w(TYPE_UNION as u64)),
        abi_word_array(&[w(block_id), w(allow_id)]),
    ]));
    let union_id = h.call(ADMIN, reg, &create_union).u256().to::<u64>();
    assert_eq!(union_id >> 56, TYPE_UNION as u64);
    // alice: blocked by the first, absent from the second; bob: admitted by both.
    assert!(!is_auth(&mut h, union_id, ALICE) && is_auth(&mut h, union_id, BOB));
    let kids = h.call(BOB, reg, &call_data(SEL_COMPOSITE_CHILD_IDS, &[w(union_id)])).ret().to_vec();
    assert_eq!(kids, encode_tuple(&[abi_word_array(&[w(block_id), w(allow_id)])]));
    // A sentinel or a composite cannot be a child; the count is checked first.
    let mut bad = SEL_CREATE_COMPOSITE.to_vec();
    bad.extend(encode_tuple(&[
        abi_word(a(ADMIN)),
        abi_word(w(TYPE_UNION as u64)),
        abi_word_array(&[w(block_id), w(union_id)]),
    ]));
    let out = h.call(ADMIN, reg, &bad);
    assert_rev(out.revert(), ERR_INVALID_CHILD_POLICY, &[w(union_id)]);
    let mut one = SEL_CREATE_COMPOSITE.to_vec();
    one.extend(encode_tuple(&[
        abi_word(a(ADMIN)),
        abi_word(w(TYPE_UNION as u64)),
        abi_word_array(&[w(block_id)]),
    ]));
    assert_rev(h.call(ADMIN, reg, &one).revert(), ERR_CHILD_POLICIES_OUTSIDE_OF_RANGE, &[]);
}

#[test]
fn a_bound_policy_forbids_a_sender() {
    let mut h = Harness::new();
    let reg = POLICY_REGISTRY_ADDRESS;
    let block_id = h
        .call(ADMIN, reg, &call_data(SEL_CREATE_POLICY, &[a(ADMIN), w(TYPE_BLOCKLIST as u64)]))
        .u256()
        .to::<u64>();
    let mut add = SEL_UPDATE_BLOCKLIST.to_vec();
    add.extend(encode_tuple(&[abi_word(w(block_id)), abi_word(w(1)), abi_word_array(&[a(ALICE)])]));
    h.call(ADMIN, reg, &add).ret();

    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        50,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_MINT, &[a(ALICE), w(10)]),
            call_data(SEL_MINT, &[a(BOB), w(10)]),
        ],
    );
    // A never-created id cannot be bound; an unknown scope is reported first.
    let out = h.call(ALICE, token, &call_data(SEL_UPDATE_POLICY, &[SCOPE_TRANSFER_SENDER, w(7)]));
    assert_rev(out.revert(), ERR_POLICY_NOT_FOUND_ID, &[w(7)]);
    let out = h.call(ALICE, token, &call_data(SEL_UPDATE_POLICY, &[B256::repeat_byte(1), w(7)]));
    assert_rev(out.revert(), ERR_UNSUPPORTED_SCOPE, &[B256::repeat_byte(1)]);
    let out =
        h.call(ALICE, token, &call_data(SEL_UPDATE_POLICY, &[SCOPE_TRANSFER_SENDER, w(block_id)]));
    out.ret();
    assert_eq!(out.logs[0].topics(), &[TOPIC_POLICY_UPDATED, SCOPE_TRANSFER_SENDER]);
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_POLICY_ID, &[SCOPE_TRANSFER_SENDER])).u256(),
        U256::from(block_id)
    );

    let out = h.call(ALICE, token, &call_data(SEL_TRANSFER, &[a(BOB), w(1)]));
    assert_rev(out.revert(), ERR_POLICY_FORBIDS, &[SCOPE_TRANSFER_SENDER, w(block_id)]);
    h.call(BOB, token, &call_data(SEL_TRANSFER, &[a(ALICE), w(1)])).ret();
    // Pausing transfers is reported before any policy.
    h.call(ALICE, token, &call_data(SEL_GRANT_ROLE, &[ROLE_PAUSE, a(ALICE)])).ret();
    let out = h.call(ALICE, token, &u8_array_call(SEL_PAUSE, &[PAUSE_TRANSFER]));
    out.ret();
    assert_eq!(out.logs[0].topics(), &[TOPIC_PAUSED, a(ALICE)]);
    let out = h.call(BOB, token, &call_data(SEL_TRANSFER, &[a(ALICE), w(1)]));
    assert_rev(out.revert(), ERR_CONTRACT_PAUSED, &[w(PAUSE_TRANSFER as u64)]);
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_PAUSED_FEATURES, &[])).ret(),
        encode_tuple(&[abi_word_array(&[w(0)])])
    );
}

// --- permit, memo, multiplier ---------------------------------------------------------

#[test]
fn permit_sets_an_allowance_from_a_signature() {
    use k256::ecdsa::{signature::hazmat::PrehashSigner, SigningKey};
    let mut h = Harness::new();
    let token = h.create(ALICE, VARIANT_ASSET, 60, ALICE, &[]);
    let key = SigningKey::from_slice(&[7u8; 32]).unwrap();
    let pubkey = key.verifying_key().to_encoded_point(false);
    let owner = Address::from_slice(&keccak256(&pubkey.as_bytes()[1..])[12..]);

    let dom = domain_separator(b"Test Token", 714, token);
    assert_eq!(h.call(BOB, token, &call_data(SEL_DOMAIN_SEPARATOR, &[])).word(), dom);
    let (value, deadline, nonce) = (U256::from(55), U256::from(NOW + 10), U256::ZERO);
    let mut sh = PERMIT_TYPEHASH.to_vec();
    for x in [a(owner), a(BOB), B256::from(value), B256::from(nonce), B256::from(deadline)] {
        sh.extend_from_slice(x.as_slice());
    }
    let mut pre = vec![0x19, 0x01];
    pre.extend_from_slice(dom.as_slice());
    pre.extend_from_slice(keccak256(&sh).as_slice());
    let digest = keccak256(pre);
    let (sig, recid): (k256::ecdsa::Signature, k256::ecdsa::RecoveryId) =
        key.sign_prehash(digest.as_slice()).unwrap();
    let (r, s) = (B256::from_slice(&sig.r().to_bytes()), B256::from_slice(&sig.s().to_bytes()));
    let v = 27 + recid.to_byte();
    assert_eq!(ecrecover_address(digest, v, r, s), Some(owner));

    let permit = call_data(
        SEL_PERMIT,
        &[a(owner), a(BOB), B256::from(value), B256::from(deadline), w(v as u64), r, s],
    );
    let out = h.call(BOB, token, &permit);
    out.ret();
    assert_eq!(out.logs[0].topics(), &[TOPIC_APPROVAL, a(owner), a(BOB)]);
    assert_eq!(h.call(BOB, token, &call_data(SEL_ALLOWANCE, &[a(owner), a(BOB)])).u256(), value);
    assert_eq!(h.call(BOB, token, &call_data(SEL_NONCES, &[a(owner)])).u256(), U256::from(1));
    // A replay names the recovered stranger and the owner.
    let out = h.call(BOB, token, &permit);
    assert_sel(out.revert(), ERR_INVALID_SIGNER);
    assert_eq!(&out.revert()[36..], a(owner).as_slice());
    // Past the deadline, before any recovery.
    h.st.time = NOW + 11;
    let out = h.call(BOB, token, &permit);
    assert_rev(out.revert(), ERR_EXPIRED_SIGNATURE, &[B256::from(deadline)]);
}

#[test]
fn memo_format_is_declared_after_the_memo() {
    let mut h = Harness::new();
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        70,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_MINT, &[a(ALICE), w(10)]),
        ],
    );
    let (memo, format) = (B256::repeat_byte(0xaa), B256::repeat_byte(0xbb));
    let out = h.call(
        ALICE,
        token,
        &call_data(SEL_TRANSFER_WITH_MEMO_FORMAT, &[a(BOB), w(1), memo, format]),
    );
    assert_eq!(out.ret(), enc_bool(true));
    let topics: Vec<B256> = out.logs.iter().map(|l| l.topics()[0]).collect();
    assert_eq!(topics, vec![TOPIC_TRANSFER, TOPIC_MEMO, TOPIC_MEMO_FORMAT_DECLARED]);
    assert_eq!(out.logs[2].topics(), &[TOPIC_MEMO_FORMAT_DECLARED, a(ALICE), memo, format]);
    let out = h.call(
        ALICE,
        token,
        &call_data(SEL_TRANSFER_WITH_MEMO_FORMAT, &[a(BOB), w(1), memo, B256::ZERO]),
    );
    assert_rev(out.revert(), ERR_INVALID_FORMAT_ID, &[]);
    // The plain memo form declares nothing.
    let out = h.call(ALICE, token, &call_data(SEL_TRANSFER_WITH_MEMO, &[a(BOB), w(1), memo]));
    assert_eq!(out.logs.len(), 2);
    assert_eq!(out.logs[1].topics(), &[TOPIC_MEMO, a(ALICE), memo]);
    // transferFromWithMemoFormat always spends the allowance, self-transfers included.
    let out = h.call(
        ALICE,
        token,
        &call_data(SEL_TRANSFER_FROM_WITH_MEMO_FORMAT, &[a(ALICE), a(BOB), w(1), memo, format]),
    );
    assert_rev(out.revert(), ERR_INSUFFICIENT_ALLOWANCE, &[a(ALICE), w(0), w(1)]);
}

#[test]
fn a_scheduled_multiplier_takes_effect_by_the_clock() {
    let mut h = Harness::new();
    let wad = U256::from(1_000_000_000_000_000_000u64);
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        80,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_OPERATOR, a(ALICE)]),
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_MINT, &[a(BOB), w(1000)]),
        ],
    );
    let two = wad * U256::from(2);
    let at = NOW + 3600;
    let out = h.call(BOB, token, &call_data(SEL_UPDATE_UI_MULTIPLIER, &[B256::from(two), w(at)]));
    assert_rev(out.revert(), ERR_AC_UNAUTHORIZED, &[a(BOB), ROLE_OPERATOR]);
    let out =
        h.call(ALICE, token, &call_data(SEL_UPDATE_UI_MULTIPLIER, &[B256::from(two), w(NOW)]));
    assert_rev(out.revert(), ERR_EFFECTIVE_AT_IN_PAST, &[w(NOW)]);
    h.call(ALICE, token, &call_data(SEL_UPDATE_UI_MULTIPLIER, &[B256::from(two), w(at)])).ret();
    let out =
        h.call(ALICE, token, &call_data(SEL_UPDATE_UI_MULTIPLIER, &[B256::from(two), w(at + 1)]));
    assert_rev(out.revert(), ERR_UI_MUL_EXISTS, &[w(at)]);

    assert_eq!(h.call(BOB, token, &call_data(SEL_MULTIPLIER, &[])).u256(), wad);
    assert_eq!(h.call(BOB, token, &call_data(SEL_NEW_UI_MULTIPLIER, &[])).u256(), two);
    assert_eq!(h.call(BOB, token, &call_data(SEL_EFFECTIVE_AT, &[])).u256(), U256::from(at));
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_BALANCE_OF_UI, &[a(BOB)])).u256(),
        U256::from(1000)
    );

    // Past the schedule, with no transaction in between.
    h.st.time = at;
    assert_eq!(h.call(BOB, token, &call_data(SEL_UI_MULTIPLIER, &[])).u256(), two);
    assert_eq!(h.call(BOB, token, &call_data(SEL_NEW_UI_MULTIPLIER, &[])).u256(), two);
    assert_eq!(
        h.call(BOB, token, &call_data(SEL_BALANCE_OF_UI, &[a(BOB)])).u256(),
        U256::from(2000)
    );
    assert_eq!(h.call(BOB, token, &call_data(SEL_TOTAL_SUPPLY_UI, &[])).u256(), U256::from(2000));
    let out = h.call(ALICE, token, &call_data(SEL_CANCEL_UI_MULTIPLIER, &[]));
    assert_rev(out.revert(), ERR_UI_MUL_MISSING, &[]);
    // The instant setter folds the matured schedule away quietly.
    let out = h.call(ALICE, token, &call_data(SEL_UPDATE_MULTIPLIER, &[B256::from(wad)]));
    out.ret();
    let topics: Vec<B256> = out.logs.iter().map(|l| l.topics()[0]).collect();
    assert_eq!(topics, vec![TOPIC_MULTIPLIER_UPDATED, TOPIC_UI_MULTIPLIER_UPDATED]);
    assert_eq!(h.call(BOB, token, &call_data(SEL_MULTIPLIER, &[])).u256(), wad);
    assert_eq!(
        h.call(
            BOB,
            token,
            &call_data(
                SEL_SUPPORTS_INTERFACE,
                &[B256::from_slice(&{
                    let mut b = [0u8; 32];
                    b[..4].copy_from_slice(&[0x01, 0xff, 0xc9, 0xa7]);
                    b
                })]
            )
        )
        .ret(),
        enc_bool(true)
    );
}

#[test]
fn seize_and_roles() {
    let mut h = Harness::new();
    let reg = POLICY_REGISTRY_ADDRESS;
    let token = h.create(
        ALICE,
        VARIANT_ASSET,
        90,
        ALICE,
        &[
            call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
            call_data(SEL_GRANT_ROLE, &[ROLE_SEIZE, a(ALICE)]),
            call_data(SEL_MINT, &[a(BOB), w(10)]),
        ],
    );
    // Only a disallowed holder is seizable: with no policy bound every holder is allowed.
    let out = h.call(
        ALICE,
        token,
        &call_data(SEL_SEIZE_WITH_MEMO, &[a(BOB), a(ALICE), w(5), B256::ZERO]),
    );
    assert_rev(out.revert(), ERR_ACCOUNT_NOT_SEIZABLE, &[a(BOB)]);
    let block_id = h
        .call(ADMIN, reg, &call_data(SEL_CREATE_POLICY, &[a(ADMIN), w(TYPE_BLOCKLIST as u64)]))
        .u256()
        .to::<u64>();
    let mut add = SEL_UPDATE_BLOCKLIST.to_vec();
    add.extend(encode_tuple(&[abi_word(w(block_id)), abi_word(w(1)), abi_word_array(&[a(BOB)])]));
    h.call(ADMIN, reg, &add).ret();
    h.call(ALICE, token, &call_data(SEL_UPDATE_POLICY, &[SCOPE_SEIZE_HOLDER, w(block_id)])).ret();
    let out = h.call(
        ALICE,
        token,
        &call_data(SEL_SEIZE_WITH_MEMO, &[a(BOB), a(ALICE), w(5), B256::repeat_byte(9)]),
    );
    assert_eq!(out.ret(), enc_bool(true));
    let topics: Vec<B256> = out.logs.iter().map(|l| l.topics()[0]).collect();
    assert_eq!(topics, vec![TOPIC_TRANSFER, TOPIC_MEMO, TOPIC_SEIZED]);
    assert_eq!(h.call(BOB, token, &call_data(SEL_BALANCE_OF, &[a(ALICE)])).u256(), U256::from(5));

    // The last admin cannot be revoked or renounced through the ordinary paths.
    let out = h.call(ALICE, token, &call_data(SEL_REVOKE_ROLE, &[ROLE_DEFAULT_ADMIN, a(ALICE)]));
    assert_rev(out.revert(), ERR_LAST_ADMIN_CANNOT_RENOUNCE, &[]);
    let out = h.call(ALICE, token, &call_data(SEL_RENOUNCE_ROLE, &[ROLE_DEFAULT_ADMIN, a(BOB)]));
    assert_rev(out.revert(), ERR_AC_BAD_CONFIRMATION, &[]);
    let out = h.call(BOB, token, &call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(BOB)]));
    assert_rev(out.revert(), ERR_AC_UNAUTHORIZED, &[a(BOB), ROLE_DEFAULT_ADMIN]);
    let out = h.call(ALICE, token, &call_data(SEL_RENOUNCE_LAST_ADMIN, &[]));
    out.ret();
    assert_eq!(out.logs[1].topics(), &[TOPIC_LAST_ADMIN_RENOUNCED, a(ALICE)]);
    // Frozen: nobody can mutate roles any more.
    let out = h.call(ALICE, token, &call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(BOB)]));
    assert_rev(out.revert(), ERR_AC_UNAUTHORIZED, &[a(ALICE), ROLE_DEFAULT_ADMIN]);
}

// --- through the EVM ------------------------------------------------------------------

mod evm {
    use super::super::{sigs::*, *};
    use super::{a, call_data, encode_create, w, ADMIN, ALICE, BOB};
    use crate::{
        evm::{
            api::BscEvm,
            precompiles::cas20::activation::{act_slot, SLOT_ADMIN, SLOT_FEATURES},
            transaction::BscTxEnv,
        },
        hardforks::bsc::BscHardfork,
    };
    use alloy_evm::precompiles::PrecompileLookup as _;
    use alloy_primitives::{Address, Bytes, B256, U256};
    use reth_evm::EvmEnv;
    use revm::{
        context::{
            result::{ExecutionResult, Output},
            BlockEnv, CfgEnv, TxEnv,
        },
        database::InMemoryDB,
        inspector::NoOpInspector,
        primitives::TxKind,
        state::{AccountInfo, Bytecode},
        ExecuteCommitEvm, ExecuteEvm,
    };

    fn evm_at(spec: BscHardfork) -> BscEvm<InMemoryDB, NoOpInspector> {
        let mut cfg = CfgEnv::new_with_spec(spec).with_chain_id(714);
        cfg.disable_nonce_check = true;
        cfg.disable_balance_check = true;
        let block = BlockEnv { timestamp: U256::from(1_800_000_000u64), ..Default::default() };
        let mut db = InMemoryDB::default();
        for who in [ADMIN, ALICE, BOB] {
            db.insert_account_info(
                who,
                AccountInfo { balance: U256::from(u64::MAX), ..Default::default() },
            );
        }
        // What the fork hook plants, plus a local admin with every feature open.
        for reg in [ACTIVATION_REGISTRY_ADDRESS, POLICY_REGISTRY_ADDRESS] {
            db.insert_account_info(reg, AccountInfo::default().with_code(marker_bytecode()));
        }
        db.insert_account_storage(
            ACTIVATION_REGISTRY_ADDRESS,
            act_slot(SLOT_ADMIN),
            U256::from_be_bytes(a(ADMIN).0),
        )
        .unwrap();
        for f in [FEATURE_ASSET, FEATURE_STABLECOIN, FEATURE_POLICY_REGISTRY] {
            db.insert_account_storage(
                ACTIVATION_REGISTRY_ADDRESS,
                storage::mapping_slot(act_slot(SLOT_FEATURES), f),
                U256::from(1),
            )
            .unwrap();
        }
        BscEvm::new(EvmEnv::new(cfg, block.into()), db, NoOpInspector, false, false)
    }

    fn tx(
        evm: &mut BscEvm<InMemoryDB, NoOpInspector>,
        from: Address,
        to: Address,
        input: Vec<u8>,
        gas: u64,
    ) -> ExecutionResult {
        let tx = TxEnv::builder()
            .caller(from)
            .chain_id(Some(714))
            .gas_limit(gas)
            .gas_price(0)
            .kind(TxKind::Call(to))
            .data(input.into())
            .build()
            .unwrap();
        let res = evm.transact_one(BscTxEnv::new(tx)).expect("tx executes");
        let state = evm.finalize();
        evm.commit(state);
        res
    }

    fn output(res: &ExecutionResult) -> Vec<u8> {
        match res {
            ExecutionResult::Success { output: Output::Call(b), .. } => b.to_vec(),
            other => panic!("expected success, got {other:?}"),
        }
    }

    /// A contract that forwards its calldata to `target` with one call opcode and
    /// bubbles the returndata and status up.
    fn forwarder(target: Address, op: u8) -> Bytecode {
        let mut c = vec![0x36, 0x60, 0x00, 0x60, 0x00, 0x37]; // CALLDATACOPY(0, 0, CALLDATASIZE)
        c.extend([0x60, 0x00, 0x60, 0x00, 0x36, 0x60, 0x00]); // retSize retOffset argsSize argsOffset
        if op == 0xF1 {
            c.extend([0x60, 0x00]); // value
        }
        c.push(0x73);
        c.extend_from_slice(target.as_slice());
        c.extend([0x5A, op]); // GAS <op>
        c.extend([0x3D, 0x60, 0x00, 0x60, 0x00, 0x3E]); // RETURNDATACOPY(0, 0, RETURNDATASIZE)
        let jumpdest = c.len() + 3 + 4;
        c.extend([0x60, jumpdest as u8, 0x57]); // JUMPI success
        c.extend([0x3D, 0x60, 0x00, 0xFD]); // REVERT(0, RETURNDATASIZE)
        c.extend([0x5B, 0x3D, 0x60, 0x00, 0xF3]); // JUMPDEST RETURN(0, RETURNDATASIZE)
        Bytecode::new_raw(Bytes::from(c))
    }

    /// An observer sees each finished call once, with the settled outcome and the
    /// work it did; it is told nothing for a spec without the family.
    #[test]
    fn an_observer_is_told_about_finished_calls() {
        use std::sync::{Arc, Mutex};
        #[derive(Clone, Default)]
        struct Recording(Arc<Mutex<Vec<CallRecord>>>);
        impl Cas20Observer for Recording {
            fn record_call(&self, call: &CallRecord) {
                self.0.lock().unwrap().push(*call);
            }
        }
        let observer = Recording::default();
        let mut evm = evm_at(BscHardfork::Jenner);
        evm.inner.precompiles.set_precompile_lookup(Cas20Lookup::with_observer(
            BscHardfork::Jenner,
            observer.clone(),
        ));

        let create = encode_create(
            VARIANT_ASSET,
            w(9),
            ALICE,
            &[
                call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
                call_data(SEL_MINT, &[a(ALICE), w(5)]),
            ],
        );
        let token = Address::from_word(B256::from_slice(&output(&tx(
            &mut evm,
            ALICE,
            FACTORY_ADDRESS,
            create,
            2_000_000,
        ))));
        tx(&mut evm, ALICE, token, call_data(SEL_TRANSFER, &[a(BOB), w(6)]), 200_000);
        tx(&mut evm, ALICE, token, call_data(SEL_NAME, &[]), 22_000);
        tx(&mut evm, ALICE, token, vec![1, 2], 100_000);

        let calls = observer.0.lock().unwrap().clone();
        let summary: Vec<(Kind, &str, CallStatus)> =
            calls.iter().map(|c| (c.kind, c.selector, c.status)).collect();
        assert_eq!(
            summary,
            vec![
                (Kind::Factory, "createCAS20", CallStatus::Return),
                (Kind::Asset, "transfer", CallStatus::Revert),
                (Kind::Asset, "name", CallStatus::OutOfGas),
                (Kind::Asset, "short", CallStatus::Revert),
            ]
        );
        let created = &calls[0];
        assert_eq!(created.stats.created, Some(VARIANT_ASSET));
        assert_eq!(created.stats.internal_calls, 2);
        assert!(created.stats.sstores > 0 && created.stats.sloads > 0 && created.stats.keccaks > 0);
        assert!(created.gas_used > 0 && created.elapsed.is_some());
        assert_eq!(
            calls[2].gas_used,
            22_000 - 21_000 - 4 * 16,
            "an exhausted frame reports its whole budget"
        );
        assert_eq!(sigs::selector_name([0xde, 0xad, 0xbe, 0xef]), "unknown");
    }

    #[test]
    fn the_family_is_routed_from_jenner_only() {
        let probe = call_data(SEL_IS_CAS20, &[a(FACTORY_ADDRESS)]);
        let mut evm = evm_at(BscHardfork::Jenner);
        assert_eq!(
            output(&tx(&mut evm, ALICE, FACTORY_ADDRESS, probe.clone(), 100_000)),
            abi::enc_bool(false)
        );
        // Before the fork the factory address is an ordinary empty account.
        let mut evm = evm_at(BscHardfork::Pasteur);
        assert_eq!(output(&tx(&mut evm, ALICE, FACTORY_ADDRESS, probe, 100_000)), Vec::<u8>::new());
    }

    #[test]
    fn a_token_is_created_and_used_through_transactions() {
        let mut evm = evm_at(BscHardfork::Jenner);
        let create = encode_create(
            VARIANT_ASSET,
            w(1),
            ALICE,
            &[
                call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
                call_data(SEL_MINT, &[a(ALICE), w(100)]),
            ],
        );
        let res = tx(&mut evm, ALICE, FACTORY_ADDRESS, create, 2_000_000);
        let token = Address::from_word(B256::from_slice(&output(&res)));
        assert_eq!(token, factory::derive_address(VARIANT_ASSET, ALICE, w(1)));
        let ExecutionResult::Success { logs, .. } = &res else { unreachable!() };
        assert_eq!(logs.last().unwrap().topics()[0], TOPIC_CAS20_CREATED);
        assert_eq!(logs.last().unwrap().address, FACTORY_ADDRESS);

        let res = tx(&mut evm, ALICE, token, call_data(SEL_TRANSFER, &[a(BOB), w(40)]), 200_000);
        assert_eq!(output(&res), abi::enc_bool(true));
        let ExecutionResult::Success { logs, .. } = &res else { unreachable!() };
        assert_eq!(logs[0].address, token);
        assert_eq!(logs[0].topics(), &[TOPIC_TRANSFER, a(ALICE), a(BOB)]);
        assert!(res.tx_gas_used() > 21_000 && res.tx_gas_used() < 200_000);
        let res = tx(&mut evm, BOB, token, call_data(SEL_BALANCE_OF, &[a(BOB)]), 100_000);
        assert_eq!(U256::from_be_slice(&output(&res)), U256::from(40));

        // A typed revert keeps the unused gas; running out consumes it all.
        let res = tx(&mut evm, BOB, token, call_data(SEL_TRANSFER, &[a(ALICE), w(41)]), 200_000);
        let ExecutionResult::Revert { output: data, .. } = &res else {
            panic!("expected revert: {res:?}")
        };
        assert_eq!(&data[..4], &ERR_INSUFFICIENT_BALANCE);
        assert!(res.tx_gas_used() < 200_000);
        let res = tx(&mut evm, BOB, token, call_data(SEL_TRANSFER, &[a(ALICE), w(1)]), 23_000);
        assert!(matches!(res, ExecutionResult::Halt { .. }), "expected halt: {res:?}");
        assert_eq!(res.tx_gas_used(), 23_000);
    }

    #[test]
    fn static_and_delegate_frames_are_refused_from_bytecode() {
        let mut evm = evm_at(BscHardfork::Jenner);
        let create = encode_create(
            VARIANT_ASSET,
            w(2),
            ALICE,
            &[
                call_data(SEL_GRANT_ROLE, &[ROLE_MINT, a(ALICE)]),
                call_data(SEL_MINT, &[a(ALICE), w(100)]),
            ],
        );
        let token = Address::from_word(B256::from_slice(&output(&tx(
            &mut evm,
            ALICE,
            FACTORY_ADDRESS,
            create,
            2_000_000,
        ))));
        let (via_call, via_static, via_delegate) =
            (Address::repeat_byte(0xc1), Address::repeat_byte(0xc2), Address::repeat_byte(0xc3));
        for (addr, op) in [(via_call, 0xF1), (via_static, 0xFA), (via_delegate, 0xF4)] {
            evm.ctx_mut()
                .journaled_state
                .database
                .insert_account_info(addr, AccountInfo::default().with_code(forwarder(token, op)));
        }
        let transfer = call_data(SEL_TRANSFER, &[a(BOB), w(1)]);
        // Through CALL the forwarder is msg.sender, with no balance.
        let res = tx(&mut evm, ALICE, via_call, transfer.clone(), 300_000);
        let ExecutionResult::Revert { output: data, .. } = res else { panic!("{res:?}") };
        assert_eq!(&data[..4], &ERR_INSUFFICIENT_BALANCE);
        assert_eq!(&data[4..36], a(via_call).as_slice());
        let res = tx(&mut evm, ALICE, via_static, transfer.clone(), 300_000);
        let ExecutionResult::Revert { output: data, .. } = res else { panic!("{res:?}") };
        assert_eq!(data.as_ref(), &ERR_STATIC_CALL_NOT_ALLOWED[..]);
        let res = tx(&mut evm, ALICE, via_delegate, transfer, 300_000);
        let ExecutionResult::Revert { output: data, .. } = res else { panic!("{res:?}") };
        assert_eq!(data.as_ref(), &ERR_DELEGATE_CALL_NOT_ALLOWED[..]);
        // A read through STATICCALL succeeds.
        let res = tx(&mut evm, ALICE, via_static, call_data(SEL_BALANCE_OF, &[a(ALICE)]), 300_000);
        assert_eq!(U256::from_be_slice(&output(&res)), U256::from(100));
    }
}

// --- the golden trace -------------------------------------------------------------------

/// testdata/cas20_golden.json is a scripted scenario recorded from go-bsc's
/// `core/vm` test harness (`TestRecordCAS20Golden`): 145 calls across every entry
/// point, each with its returndata, status, gas, refund and logs, and the state
/// root the harness ended on. Replaying it here holds this port to the reference
/// client call by call.
///
/// testdata/cas20_golden_footprint.json pins, per step, what this port did to get
/// there — gas, and the SLOAD, SSTORE and paid-keccak counts — so a drift in the
/// metering points at one operation. It is this port's own artifact, regenerated
/// with `BLESS_GOLDEN=1 cargo test --lib cas20::tests::golden`.
mod golden {
    use super::super::{
        errors::Outcome,
        test_host::{run_call, CallSpec, MockHost},
        *,
    };
    use alloy_primitives::{hex, Address, B256, KECCAK256_EMPTY, U256};
    use reth_trie_common::{
        root::{state_root_unhashed, storage_root_unhashed},
        TrieAccount,
    };
    use std::collections::BTreeMap;

    const FOOTPRINT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/evm/precompiles/cas20/testdata/cas20_golden_footprint.json"
    );

    fn addr(v: &serde_json::Value) -> Address {
        v.as_str().unwrap().parse().unwrap()
    }

    fn bytes(v: &serde_json::Value) -> Vec<u8> {
        hex::decode(v.as_str().unwrap()).unwrap()
    }

    fn status(outcome: &Outcome) -> &'static str {
        match outcome {
            Outcome::Return(_) => "ok",
            Outcome::Revert(_) => "revert",
            Outcome::OutOfGas => "oog",
            Outcome::Fatal(_) => "fatal",
        }
    }

    #[test]
    fn the_go_trace_replays_identically() {
        let trace: serde_json::Value =
            serde_json::from_str(include_str!("testdata/cas20_golden.json")).expect("valid trace");

        // go-bsc's harness: the fork's sentinels, the admin and the open features are
        // written into an uncommitted state, so every original value is zero.
        let mut host = MockHost::new(trace["chainId"].as_u64().unwrap(), 0);
        host.seed_activation(addr(&trace["admin"]));

        let mut failures = Vec::new();
        let mut footprint: Vec<serde_json::Value> = Vec::new();
        for (i, step) in trace["steps"].as_array().unwrap().iter().enumerate() {
            let name = step["name"].as_str().unwrap();
            let label = format!("#{i} {name}");
            host.time = step["time"].as_u64().unwrap();
            let kind = step["kind"].as_str().unwrap();
            let spec = CallSpec {
                caller: addr(&step["caller"]),
                to: addr(&step["to"]),
                gas: step["gas"].as_u64().unwrap(),
                is_static: kind == "static",
                direct: kind != "delegate",
                value: step["value"].as_str().unwrap().parse().unwrap(),
            };
            let r = run_call(&mut host, spec, &bytes(&step["input"]));
            let mut check = |what: &str, ok: bool, detail: String| {
                if !ok {
                    failures.push(format!("{label}: {what}: {detail}"));
                }
            };
            let want_status = step["status"].as_str().unwrap();
            check(
                "status",
                status(&r.outcome) == want_status,
                format!("got {:?}, want {want_status}", r.outcome),
            );
            let ret = match &r.outcome {
                Outcome::Return(b) | Outcome::Revert(b) => b.clone(),
                _ => Vec::new(),
            };
            let want_ret = bytes(&step["ret"]);
            check(
                "returndata",
                ret == want_ret,
                format!("got 0x{}, want 0x{}", hex::encode(&ret), hex::encode(&want_ret)),
            );
            let want_gas = step["gasUsed"].as_u64().unwrap();
            check("gas", r.used == want_gas, format!("got {}, want {want_gas}", r.used));
            let want_refund = step["refund"].as_i64().unwrap();
            check(
                "refund",
                r.refund == want_refund,
                format!("got {}, want {want_refund}", r.refund),
            );
            let want_logs = step["logs"].as_array().map(|l| l.len()).unwrap_or(0);
            check(
                "log count",
                r.logs.len() == want_logs,
                format!("got {}, want {want_logs}", r.logs.len()),
            );
            for (j, (got, want)) in
                r.logs.iter().zip(step["logs"].as_array().into_iter().flatten()).enumerate()
            {
                let topics: Vec<B256> = want["topics"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|t| t.as_str().unwrap().parse().unwrap())
                    .collect();
                check(
                    &format!("log {j} address"),
                    got.address == addr(&want["address"]),
                    format!("{:?}", got.address),
                );
                check(
                    &format!("log {j} topics"),
                    got.topics() == topics.as_slice(),
                    format!("{:?}", got.topics()),
                );
                check(
                    &format!("log {j} data"),
                    got.data.data.as_ref() == bytes(&want["data"]).as_slice(),
                    hex::encode(&got.data.data),
                );
            }
            footprint.push(serde_json::json!({
                "name": name,
                "gas": r.used,
                "sload": r.stats.sloads,
                "sstore": r.stats.sstores,
                "keccak": r.stats.keccaks,
            }));
        }

        // The state the harness ended on, as a root over every account the family touched.
        let mut storage: BTreeMap<Address, Vec<(B256, U256)>> = BTreeMap::new();
        for (at, slot, v) in host.storage() {
            storage.entry(at).or_default().push((B256::from(slot), v));
        }
        let mut accounts: BTreeMap<Address, TrieAccount> = BTreeMap::new();
        for a in host.coded_accounts().chain(storage.keys().copied()) {
            let code_hash = host.code_hash_of(a).unwrap_or(KECCAK256_EMPTY);
            let storage_root = storage_root_unhashed(storage.get(&a).cloned().unwrap_or_default());
            accounts
                .insert(a, TrieAccount { nonce: 0, balance: U256::ZERO, storage_root, code_hash });
        }
        for (a, want) in trace["codeHashes"].as_object().unwrap() {
            let a: Address = a.parse().unwrap();
            let want: B256 = want.as_str().unwrap().parse().unwrap();
            let got = accounts.get(&a).map(|acc| acc.code_hash);
            if got != Some(want) {
                failures.push(format!("code hash of {a:?}: got {got:?}, want {want:?}"));
            }
        }
        let root = state_root_unhashed(accounts);
        let want_root: B256 = trace["stateRoot"].as_str().unwrap().parse().unwrap();
        if root != want_root {
            failures.push(format!("state root: got {root:?}, want {want_root:?}"));
        }
        assert!(
            failures.is_empty(),
            "{} divergences from go-bsc:\n{}",
            failures.len(),
            failures.join("\n")
        );

        // This port's own footprint, step by step.
        let footprint = serde_json::Value::Array(footprint);
        if std::env::var("BLESS_GOLDEN").as_deref() == Ok("1") {
            std::fs::write(FOOTPRINT, serde_json::to_string_pretty(&footprint).unwrap()).unwrap();
            return;
        }
        let pinned: serde_json::Value =
            serde_json::from_str(include_str!("testdata/cas20_golden_footprint.json"))
                .expect("pinned footprint");
        let drift: Vec<String> = pinned
            .as_array()
            .unwrap()
            .iter()
            .zip(footprint.as_array().unwrap())
            .enumerate()
            .filter(|(_, (want, got))| want != got)
            .map(|(i, (want, got))| format!("#{i}: pinned {want}, got {got}"))
            .collect();
        assert!(
            drift.is_empty()
                && pinned.as_array().unwrap().len() == footprint.as_array().unwrap().len(),
            "storage-access footprint drifted (regenerate with BLESS_GOLDEN=1 if intended):\n{}",
            drift.join("\n")
        );
    }
}
