//! Hot paths of the CAS20 family, driven through `BscEvm` at Jenner: a plain
//! transfer, a transferFrom, a policy lookup through a four-child union, and an
//! announce carrying two internal calls. Each iteration runs one transaction on
//! the same pre-state (the journal is discarded, never committed), so the numbers
//! track the precompile's own work plus the fixed transaction overhead. Fees are
//! zero and the balance and nonce checks are off, so the transaction overhead is a
//! little lighter than a paid transaction's; no metrics recorder is installed, so
//! the production observer's handles are no-ops here.
//!
//! `cargo bench --bench cas20`

use alloy_primitives::{keccak256, Address, B256, U256};
use criterion::{criterion_group, criterion_main, Criterion};
use reth_bsc::{
    evm::{
        api::BscEvm,
        precompiles::cas20::{
            marker_bytecode, ACTIVATION_REGISTRY_ADDRESS, FACTORY_ADDRESS, POLICY_REGISTRY_ADDRESS,
        },
        transaction::BscTxEnv,
    },
    hardforks::bsc::BscHardfork,
};
use reth_evm::EvmEnv;
use revm::{
    context::{
        result::{ExecutionResult, Output},
        BlockEnv, CfgEnv, TxEnv,
    },
    database::InMemoryDB,
    inspector::NoOpInspector,
    primitives::TxKind,
    state::AccountInfo,
    ExecuteCommitEvm, ExecuteEvm,
};

const ADMIN: Address =
    Address::new([0x60, 0xfe, 0xed, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
const ALICE: Address =
    Address::new([0xa1, 0x1c, 0xe0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
const BOB: Address =
    Address::new([0xb0, 0xb0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);

fn sel(sig: &str) -> [u8; 4] {
    keccak256(sig.as_bytes())[..4].try_into().unwrap()
}

fn word(n: u64) -> B256 {
    B256::from(U256::from(n))
}

fn call(sig: &str, words: &[B256]) -> Vec<u8> {
    let mut out = sel(sig).to_vec();
    for w in words {
        out.extend_from_slice(w.as_slice());
    }
    out
}

/// abi-encodes a tuple of static words and dynamic parts, Solidity style.
fn tuple(parts: &[Part]) -> Vec<u8> {
    let mut head = Vec::new();
    let mut tail = Vec::new();
    let tail_start = 32 * parts.len();
    for p in parts {
        match p {
            Part::Word(w) => head.extend_from_slice(w.as_slice()),
            Part::Dyn(t) => {
                head.extend_from_slice(word((tail_start + tail.len()) as u64).as_slice());
                tail.extend_from_slice(t);
            }
        }
    }
    head.extend(tail);
    head
}

enum Part {
    Word(B256),
    Dyn(Vec<u8>),
}

fn bytes_part(b: &[u8]) -> Part {
    let mut t = word(b.len() as u64).to_vec();
    t.extend_from_slice(b);
    t.resize(32 + b.len().div_ceil(32) * 32, 0);
    Part::Dyn(t)
}

fn words_part(ws: &[B256]) -> Part {
    let mut t = word(ws.len() as u64).to_vec();
    for w in ws {
        t.extend_from_slice(w.as_slice());
    }
    Part::Dyn(t)
}

fn bytes_array_part(items: &[Vec<u8>]) -> Part {
    let elems: Vec<Vec<u8>> = items
        .iter()
        .map(|c| match bytes_part(c) {
            Part::Dyn(t) => t,
            _ => unreachable!(),
        })
        .collect();
    let mut arr = word(items.len() as u64).to_vec();
    let mut cur = 32 * items.len();
    for e in &elems {
        arr.extend_from_slice(word(cur as u64).as_slice());
        cur += e.len();
    }
    for e in &elems {
        arr.extend_from_slice(e);
    }
    Part::Dyn(arr)
}

fn create_call(admin: Address, init_calls: &[Vec<u8>]) -> Vec<u8> {
    let params = tuple(&[Part::Dyn(tuple(&[
        Part::Word(word(1)),
        bytes_part(b"Bench Token"),
        bytes_part(b"BT"),
        Part::Word(admin.into_word()),
        Part::Word(word(18)),
    ]))]);
    let mut out = sel("createCAS20(uint8,bytes32,bytes,bytes[])").to_vec();
    out.extend(tuple(&[
        Part::Word(word(0)),
        Part::Word(word(1)),
        bytes_part(&params),
        bytes_array_part(init_calls),
    ]));
    out
}

type Evm = BscEvm<InMemoryDB, NoOpInspector>;

fn evm() -> Evm {
    let mut cfg = CfgEnv::new_with_spec(BscHardfork::Jenner).with_chain_id(714);
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
    for reg in [ACTIVATION_REGISTRY_ADDRESS, POLICY_REGISTRY_ADDRESS] {
        db.insert_account_info(reg, AccountInfo::default().with_code(marker_bytecode()));
    }
    // ActivationRegistry: admin at root+1, every feature open (bsc.activation_registry layout).
    let root = U256::from_be_bytes(erc7201("bsc.activation_registry").0);
    db.insert_account_storage(
        ACTIVATION_REGISTRY_ADDRESS,
        root + U256::from(1),
        U256::from_be_bytes(ADMIN.into_word().0),
    )
    .unwrap();
    for f in ["bsc.cas20_asset", "bsc.cas20_stablecoin", "bsc.policy_registry"] {
        let mut pre = [0u8; 64];
        pre[..32].copy_from_slice(keccak256(f).as_slice());
        pre[32..].copy_from_slice(&root.to_be_bytes::<32>());
        db.insert_account_storage(
            ACTIVATION_REGISTRY_ADDRESS,
            U256::from_be_bytes(keccak256(pre).0),
            U256::from(1),
        )
        .unwrap();
    }
    BscEvm::new(EvmEnv::new(cfg, block.into()), db, NoOpInspector, false, false)
}

fn erc7201(ns: &str) -> B256 {
    let inner = U256::from_be_bytes(keccak256(ns).0) - U256::from(1);
    let mut root = keccak256(inner.to_be_bytes::<32>());
    root.0[31] = 0;
    root
}

fn tx(from: Address, to: Address, input: Vec<u8>) -> BscTxEnv {
    BscTxEnv::new(
        TxEnv::builder()
            .caller(from)
            .chain_id(Some(714))
            .gas_limit(5_000_000)
            .gas_price(0)
            .kind(TxKind::Call(to))
            .data(input.into())
            .build()
            .unwrap(),
    )
}

fn commit(evm: &mut Evm, t: BscTxEnv) -> Vec<u8> {
    let res = evm.transact_one(t).expect("tx executes");
    let out = match &res {
        ExecutionResult::Success { output: Output::Call(b), .. } => b.to_vec(),
        other => panic!("setup call failed: {other:?}"),
    };
    let state = evm.finalize();
    evm.commit(state);
    out
}

/// Runs `t` once and discards its effects, panicking unless it succeeded.
fn probe(evm: &mut Evm, t: &BscTxEnv) {
    let res = evm.transact_one(t.clone()).expect("tx executes");
    assert!(res.is_success(), "{res:?}");
    let _ = evm.finalize();
}

fn bench(c: &mut Criterion) {
    let mut evm = evm();
    let role_mint = keccak256("MINT_ROLE");
    let role_operator = keccak256("OPERATOR_ROLE");
    let token = Address::from_word(B256::from_slice(&commit(
        &mut evm,
        tx(
            ALICE,
            FACTORY_ADDRESS,
            create_call(
                ALICE,
                &[
                    call("grantRole(bytes32,address)", &[role_mint, ALICE.into_word()]),
                    call("grantRole(bytes32,address)", &[role_operator, ALICE.into_word()]),
                    call("mint(address,uint256)", &[ALICE.into_word(), word(1_000_000)]),
                ],
            ),
        ),
    )));
    commit(
        &mut evm,
        tx(ALICE, token, call("approve(address,uint256)", &[BOB.into_word(), word(1_000_000)])),
    );

    // Four blocklists under one union, the account blocked by the first three: the
    // deepest lookup a transfer can trigger, and one that walks every child.
    let reg = POLICY_REGISTRY_ADDRESS;
    let mut kids = Vec::new();
    for i in 0..4 {
        let id = commit(
            &mut evm,
            tx(ADMIN, reg, call("createPolicy(address,uint8)", &[ADMIN.into_word(), word(0)])),
        );
        let id = B256::from_slice(&id);
        if i < 3 {
            let mut block = sel("updateBlocklist(uint64,bool,address[])").to_vec();
            block.extend(tuple(&[
                Part::Word(id),
                Part::Word(word(1)),
                words_part(&[BOB.into_word()]),
            ]));
            commit(&mut evm, tx(ADMIN, reg, block));
        }
        kids.push(id);
    }
    let mut create_union = sel("createCompositePolicy(address,uint8,uint64[])").to_vec();
    create_union.extend(tuple(&[
        Part::Word(ADMIN.into_word()),
        Part::Word(word(2)),
        words_part(&kids),
    ]));
    let union = U256::from_be_slice(&commit(&mut evm, tx(ADMIN, reg, create_union)));

    let transfer = tx(ALICE, token, call("transfer(address,uint256)", &[BOB.into_word(), word(1)]));
    let transfer_from = tx(
        BOB,
        token,
        call(
            "transferFrom(address,address,uint256)",
            &[ALICE.into_word(), BOB.into_word(), word(1)],
        ),
    );
    let is_authorized =
        tx(BOB, reg, call("isAuthorized(uint64,address)", &[B256::from(union), BOB.into_word()]));
    let mut announce = sel("announce(bytes[],string,string,string)").to_vec();
    announce.extend(tuple(&[
        bytes_array_part(&[
            call("DEFAULT_ADMIN_ROLE()", &[]),
            call("mint(address,uint256)", &[BOB.into_word(), word(1)]),
        ]),
        bytes_part(b"A1"),
        bytes_part(b"bench announcement"),
        bytes_part(b"https://example.com/a1"),
    ]));
    let announce = tx(ALICE, token, announce);
    for t in [&transfer, &transfer_from, &is_authorized, &announce] {
        probe(&mut evm, t);
    }

    let mut g = c.benchmark_group("cas20");
    g.bench_function("transfer", |b| b.iter(|| probe(&mut evm, &transfer)));
    g.bench_function("transferFrom", |b| b.iter(|| probe(&mut evm, &transfer_from)));
    g.bench_function("isAuthorized(union of 4)", |b| b.iter(|| probe(&mut evm, &is_authorized)));
    g.bench_function("announce(2 internal calls)", |b| b.iter(|| probe(&mut evm, &announce)));
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
