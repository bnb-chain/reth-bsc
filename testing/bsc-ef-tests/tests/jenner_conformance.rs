//! Cross-client conformance for BEP-703 against a chain go-bsc actually produced.
//!
//! The fixtures under `fixtures/jenner/` come from go-bsc's own
//! `TestPaymentLaneRoundTripsThroughAGeneratedChain` on the `feat-payment-lane` branch
//! (bnb-chain/bsc#3793, head `03d96818`), dumped by a local test-only patch. Its chain deliberately
//! walks all four quota regimes: bootstrap at the floor, an expansion that lands strictly inside
//! `(floor, ceiling)`, a hold in the hysteresis band, and a shrink. Nothing else this repo has
//! covers the expansion — reaching the 80% trigger takes ~2000 transactions per block, which the
//! Rust chain harness skips.
//!
//! What is asserted is the accumulator: for every block, the quota reth-bsc derives from the
//! parent must be the quota go-bsc committed. That is the one quantity a disagreement never
//! recovers from — the recurrence reads the parent's *committed* value, so one divergent block
//! splits the chain permanently.
//!
//! Parlia is out of scope here: go-bsc generated these blocks with an ethash faker, so they carry
//! no valid seal and full header validation would reject them for reasons unrelated to BEP-703.

use alloy_consensus::Header;
use alloy_rlp::Decodable;
use reth_bsc::consensus::payment_lane::{
    rules::bounds, Commitment, Signal, DEFAULT_PARAMS, SYSTEM_TXS_GAS_HARD_LIMIT,
};

/// `DEFAULT_PARAMS` is what this fixture's `0x2007` returns: its storage is untouched, so the
/// contract applies its own fallbacks. Wrong values here and every quota below would mismatch.
fn header(n: u64) -> Header {
    let hex = match n {
        1 => include_str!("fixtures/jenner/header_1.hex"),
        2 => include_str!("fixtures/jenner/header_2.hex"),
        3 => include_str!("fixtures/jenner/header_3.hex"),
        4 => include_str!("fixtures/jenner/header_4.hex"),
        5 => include_str!("fixtures/jenner/header_5.hex"),
        6 => include_str!("fixtures/jenner/header_6.hex"),
        _ => unreachable!(),
    };
    let raw = alloy_primitives::hex::decode(hex.trim()).expect("fixture is hex");
    Header::decode(&mut raw.as_slice()).expect("go-bsc header decodes as an alloy header")
}

#[test]
fn jenner_quota_recurrence_agrees_with_go_bsc() {
    // Straight from go-bsc's own assertion table. Blocks 1 and 2 are outside the mechanism:
    // `jennerTime` is 15, so block 2 is the activation block and block 3 is the first block the
    // lane applies to.
    const EXPECTED: [(u64, u64); 4] = [
        (3, 2_000_000), // bootstrap: the zero signal maps to the floor
        (4, 3_100_000), // expand by 200 * 55M / 10000, unclamped — so not the ceiling
        (5, 3_100_000), // hold: neither branch taken, and the two would differ
        (6, 2_825_000), // shrink by 50 * 55M / 10000, neither floor nor ceiling
    ];

    for (number, want_quota) in EXPECTED {
        let parent = header(number - 1);
        let child = header(number);

        // The codec, against bytes a different client wrote.
        let committed = Commitment::decode(child.ommers_hash)
            .unwrap_or_else(|e| panic!("block {number} commitment from go-bsc: {e}"));
        assert_eq!(committed.quota, want_quota, "block {number}: go-bsc's own expectation");

        // The accumulator. `next_lane_quota` reads only the parent header and this block's gas
        // limit, so this is exactly the derivation an importing node performs.
        let derived = Signal::from_parent(&parent)
            .unwrap_or_else(|e| panic!("block {number} parent signal: {e}"))
            .next_lane_quota(&DEFAULT_PARAMS, child.gas_limit);
        assert_eq!(
            derived, committed.quota,
            "block {number}: reth-bsc derives {derived}, go-bsc committed {}",
            committed.quota
        );

        // And the four header checks, on a header this client did not write.
        committed
            .check_header_bounds(child.gas_used, child.gas_limit)
            .unwrap_or_else(|e| panic!("block {number} header bounds: {e}"));
    }
}

#[test]
fn jenner_activation_block_carries_no_commitment() {
    // Blocks 1 and 2 predate the mechanism for their parent, so go-bsc leaves the empty uncle
    // root in place. Block 2 is the activation block: post-Jenner by its own timestamp, pre-Jenner
    // by its parent's. If reth-bsc ever accepted a commitment there it would fork.
    for n in [1u64, 2] {
        assert_eq!(
            header(n).ommers_hash,
            alloy_consensus::EMPTY_OMMER_ROOT_HASH,
            "block {n} must still carry the empty ommers root"
        );
        assert!(Commitment::decode(header(n).ommers_hash).is_err());
    }
    // And the first block that does carry one decodes.
    assert!(Commitment::decode(header(3).ommers_hash).is_ok());
}

#[test]
fn jenner_payment_gas_split_agrees_with_go_bsc() {
    // go-bsc's table again, this time the classification: block 5 holds three payment transfers
    // and nothing else, blocks 3, 4 and 6 hold no payment gas at all. The general side is the
    // residual, which is what ties the producer's split to what the header claims.
    let split = |number: u64| {
        let h = header(number);
        let c = Commitment::decode(h.ommers_hash).expect("commitment");
        (h.gas_used - c.payment_gas_used, c.payment_gas_used)
    };
    for number in [3u64, 4, 6] {
        let (general, payment) = split(number);
        assert_eq!(payment, 0, "block {number} books no payment gas");
        assert_eq!(general, header(number).gas_used, "block {number} is general-only");
    }
    // Block 3 is the one that drives the expansion: 80.02% of the 55M gas limit, all general,
    // which is what puts the signal over the 80% trigger.
    let (general_3, _) = split(3);
    assert_eq!(general_3, 44_012_800);
    assert!(general_3 as u128 * 10_000 >= DEFAULT_PARAMS.expand_trigger as u128 * 55_000_000);

    let (general_5, payment_5) = split(5);
    assert_eq!(general_5, 0, "block 5 is payment-only");
    assert!(payment_5 > 0, "block 5 carries three payment transfers");
}

#[test]
fn jenner_bounds_match_the_fixture_gas_limit() {
    // 55M, the same gas limit go-bsc's fixture uses. Pins that the expansion in block 4 is
    // genuinely unclamped: 3.1M sits strictly between the floor and the ceiling, so a client that
    // got either bound wrong would still have to get the step right to match.
    let gl = header(3).gas_limit;
    assert_eq!(gl, 55_000_000);
    let b = bounds(&DEFAULT_PARAMS, gl);
    assert_eq!((b.floor, b.ceiling), (2_000_000, 4_400_000));
    assert_eq!(b.reserve_cap, gl - SYSTEM_TXS_GAS_HARD_LIMIT);
    assert!(b.floor < 3_100_000 && 3_100_000 < b.ceiling);
}

#[test]
fn go_bsc_block_rlp_decodes_as_a_bsc_block() {
    // go-bsc emits the three-field `[header, txs, ommers]` block. `BscBlock`'s trailing optional
    // `withdrawals`/`sidecars` are what make that decode without a bespoke decoder — the premise
    // any future full-block conformance fixture rests on.
    use reth_bsc::node::primitives::BscBlock;
    let raw = alloy_primitives::hex::decode(
        include_str!("fixtures/jenner/block_5.hex").trim(),
    )
    .expect("fixture is hex");
    let block = BscBlock::decode(&mut raw.as_slice()).expect("go-bsc block decodes as BscBlock");
    assert_eq!(block.header.number, 5);
    assert!(block.body.inner.ommers.is_empty(), "the ommers list stays empty");
    assert_eq!(block.body.inner.transactions.len(), 3, "three payment transfers");
}
