//! End-to-end test asserting cache-on and cache-off produce identical blocks.
//!
//! The full implementation depends on a BscMiner integration test harness
//! that does not exist in this repo at the time of writing. The scaffold
//! below documents the intended approach and is `#[ignore]`d so it does
//! not block CI. Production canary rollout (per
//! docs/superpowers/specs/2026-05-21-miner-cross-block-cache-design.md §13)
//! is the operational correctness validation path.
//!
//! # Why no harness yet?
//!
//! Spinning up a `BscMiner` + `BscNode` in a test requires:
//! - A temp datadir with synced BSC state (or a pre-baked fixture),
//! - A running Parlia consensus + snapshot provider,
//! - The full `NodeBuilder` pipeline (engine, network, RPC).
//!
//! None of this infrastructure exists as an in-process test helper in
//! reth-bsc today. Adding it is non-trivial and out of scope for the cache
//! implementation task. When the harness is added, this file serves as the
//! specification of what needs to be tested.
//!
//! # When to complete this test
//!
//! Remove the `#[ignore]` annotation and fill in the body once:
//! 1. A `TestBscNode` (or equivalent) helper exists that can produce
//!    payloads in-process.
//! 2. A pre-baked datadir or genesis fixture allows a deterministic
//!    block build without external network access.
//!
//! Until then, unit test `concurrent_reader_writer_no_disagreement` in
//! `src/node/miner/cache.rs` (Task 17) is the primary correctness validator:
//! it pits a concurrent reader+writer against a frozen oracle snapshot and
//! asserts no cache hit ever disagrees with the oracle.

#[tokio::test]
#[ignore = "Requires BscMiner integration test harness which does not exist yet (see Task 18 in the plan)"]
async fn cache_on_vs_cache_off_produce_identical_blocks() {
    // Procedure (when harness is available):
    //
    //   1. Spin up reth-bsc with a temp datadir:
    //      let node = TestBscNode::start_with_temp_datadir().await;
    //
    //   2. Sync some blocks OR use a pre-baked datadir / genesis fixture.
    //
    //   3. Build a payload via BscMiner with cache ENABLED (default):
    //      reth_bsc::node::miner::cache::set_cache_disabled(false);
    //      let result_on = node.build_payload(parent_hash).await;
    //
    //   4. Capture (block_hash, state_root, receipts_root) from result_on.
    //
    //   5. Disable the cache for the next build:
    //      reth_bsc::node::miner::cache::set_cache_disabled(true);
    //
    //   6. Build the same payload again (same parent, same txpool snapshot):
    //      let result_off = node.build_payload(parent_hash).await;
    //
    //   7. Assert all three roots match:
    //      assert_eq!(result_on.block_hash,     result_off.block_hash,     "block hash mismatch");
    //      assert_eq!(result_on.state_root,     result_off.state_root,     "state root mismatch");
    //      assert_eq!(result_on.receipts_root,  result_off.receipts_root,  "receipts root mismatch");
    //
    //   8. Restore cache for other tests:
    //      reth_bsc::node::miner::cache::set_cache_disabled(false);
    //
    // Currently the integration harness does not exist.
    // The `concurrent_reader_writer_no_disagreement` unit test in
    // src/node/miner/cache.rs (Task 17) is the primary correctness guarantee.
    // Production canary rollout (spec §13) detects divergence in real operation.
    panic!(
        "integration harness not yet available — \
         see module-level docs and Task 18 in \
         docs/superpowers/plans/2026-05-21-miner-cross-block-cache.md"
    );
}
