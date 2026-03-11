//! EVM state cache pre-warming for the BSC miner.
//!
//! # Motivation
//!
//! The miner executes transactions without prior knowledge of the final tx-list (unlike
//! fullnode sync which prewarms the exact block).  Each tx therefore suffers cold EVM state
//! reads against MDBX/RocksDB.  This module pre-populates `CachedReads` before the build loop
//! to reduce those cold reads.
//!
//! Mirrors geth-bsc's `PrefetchMining()` full `ApplyMessage()` speculative execution approach.
//!
//! # Implementation
//!
//! Top-N pending txs are distributed round-robin across PREWARM_WORKERS threads.  Each thread:
//!   1. Opens its own read-only `StateProvider` (MDBX supports concurrent readers).
//!   2. Builds an EVM with `disable_nonce_check = true` and `disable_base_fee = true`.
//!   3. Executes its tx slice speculatively; all DB reads are cached in a local `CachedReads`.
//!   4. If triedb is active, computes trie hash (without committing) to warm PathDB MokaCache.
//!
//! Worker-0 also pre-warms fixed BSC system contract accounts (called every block in
//! `apply_pre_execution_changes()`).
//!
//! The main thread merges all per-worker `CachedReads` into the caller's `cached_reads`.
//!
//! # Correctness guarantees
//!
//! - No external state mutations: only `StateProvider` reads, `CachedReads` writes.
//! - Speculative results are discarded; only the read-path cache is kept.
//! - System contract writes go into `State.bundle` (which shadows `CachedReads`) in the real
//!   build loop, so stale values here are harmless.
//! - `disable_nonce_check` / `disable_base_fee` maximise tx execution coverage.
//!   These flags are safe for prewarm because we throw away execution results.
//! - Build loop starts with a fresh `State`; `CachedReads` is a read cache only.
//! - Trie hash computation uses `intermediate_hashed_post_state` (no DiffLayer commit),
//!   so PathDB MokaCache is warmed without mutating the global trie state.
//!   PathDB clones share `Arc<MokaCache>`, so warming in prewarm workers benefits the
//!   real root calculation in `finish()`.

use alloy_consensus::{transaction::Recovered, Header as ConsensusHeader};
use alloy_evm::{Evm as EvmTrait, IntoTxEnv};
use alloy_primitives::map::HashMap;
use alloy_primitives::{Address, B256};
use reth::transaction_pool::{PoolTransaction, TransactionPool};
use reth_evm::{ConfigureEvm, TxEnvFor};
use reth_primitives::TransactionSigned;
use reth_primitives_traits::NodePrimitives;
use reth_provider::{HashedPostStateProvider, StateProvider, StateProviderFactory};
use reth_revm::cached::{CachedAccount, CachedReads};
use reth_revm::database::StateProviderDatabase;
use revm::bytecode::Bytecode;
use revm::database::{states::bundle_state::BundleRetention, State};
use revm::state::AccountInfo;
use tracing::{debug, trace, warn};

/// Number of parallel worker threads for prewarm DB reads.
const PREWARM_WORKERS: usize = 5;

/// BSC system contract addresses called every block in pre/post execution.
/// Worker-0 pre-warms these to reduce cold reads during `apply_pre_execution_changes()`.
const BSC_SYSTEM_CONTRACTS: &[Address] = &[
    alloy_primitives::address!("0000000000000000000000000000000000001000"), // ValidatorSet
    alloy_primitives::address!("0000000000000000000000000000000000001001"), // SlashIndicator
    alloy_primitives::address!("0000000000000000000000000000000000001002"), // SystemReward
    alloy_primitives::address!("0000000000000000000000000000000000001003"), // LightClient
    alloy_primitives::address!("0000000000000000000000000000000000001004"), // TokenHub
    alloy_primitives::address!("0000000000000000000000000000000000001005"), // RelayerHub
    alloy_primitives::address!("0000000000000000000000000000000000001006"), // GovHub
    alloy_primitives::address!("0000000000000000000000000000000000002000"), // StakeHub (BEP294)
    alloy_primitives::address!("0000000000000000000000000000000000002001"), // GovernorBNB (BEP294)
    alloy_primitives::address!("0000000000000000000000000000000000002002"), // GovToken (BEP294)
    alloy_primitives::address!("0000000000000000000000000000000000002003"), // Timelock (BEP294)
    alloy_primitives::address!("0000000000000000000000000000000000002004"), // TokenRecoverPortal (BEP294)
];

/// Configuration for miner EVM prewarm.
#[derive(Debug, Clone)]
pub struct MinerPrewarmConfig {
    /// Maximum number of pending transactions to consider for prewarm.
    pub tx_limit: usize,
    /// Whether worker-0 should also pre-warm known BSC system contract accounts.
    pub prewarm_system_contracts: bool,
}

impl Default for MinerPrewarmConfig {
    fn default() -> Self {
        // 3000 TPS × 0.45s/block = ~1350 txs/block; use 2000 for buffer.
        Self { tx_limit: 2000, prewarm_system_contracts: true }
    }
}

/// Pre-warms the miner's `CachedReads` via full speculative EVM execution across
/// `PREWARM_WORKERS` parallel threads.
///
/// If triedb is active, each worker also traverses the trie (without committing) to warm
/// the shared PathDB `MokaCache`, reducing root-hash latency during the real build.
///
/// Must be called BEFORE `cached_reads` is consumed by `State::builder().as_db_mut()`.
pub fn prewarm_miner_evm_cache<Client, Pool, EvmConfig>(
    client: &Client,
    pool: &Pool,
    evm_config: &EvmConfig,
    parent_header: &ConsensusHeader,
    parent_hash: B256,
    parent_state_root: B256,
    base_fee: u64,
    cached_reads: &mut CachedReads,
    config: &MinerPrewarmConfig,
) where
    Client: StateProviderFactory,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>>,
    EvmConfig: ConfigureEvm + Sync,
    <EvmConfig as ConfigureEvm>::Primitives:
        NodePrimitives<BlockHeader = ConsensusHeader>,
    Recovered<TransactionSigned>: IntoTxEnv<TxEnvFor<EvmConfig>>,
{
    // ── 1. Collect top-N txs as Recovered<TransactionSigned> ─────────────────────────────────
    let attrs = reth::transaction_pool::BestTransactionsAttributes::new(base_fee, None);
    let txs: Vec<Recovered<TransactionSigned>> = pool
        .best_transactions_with_attributes(attrs)
        .take(config.tx_limit)
        .map(|arc_tx| arc_tx.transaction.clone_into_consensus())
        .collect();

    let tx_count = txs.len();

    // ── 2. Distribute txs round-robin into PREWARM_WORKERS buckets ───────────────────────────
    let mut buckets: [Vec<Recovered<TransactionSigned>>; PREWARM_WORKERS] =
        std::array::from_fn(|_| Vec::new());
    for (i, tx) in txs.into_iter().enumerate() {
        buckets[i % PREWARM_WORKERS].push(tx);
    }

    let prewarm_system = config.prewarm_system_contracts;
    let trie_active = rust_eth_triedb::triedb_manager::is_triedb_active();

    // ── 3. Parallel speculative EVM execution + trie prewarm ──────────────────────────────────
    //
    // Each worker opens its own StateProvider (MDBX supports concurrent readers), builds an
    // EVM backed by State<CachedReadsDbMut> with bundle tracking, executes its tx slice, and:
    //   a) Returns the reads captured in a local CachedReads (EVM state cache prewarm).
    //   b) If triedb is active: calls intermediate_hashed_post_state (no commit) to warm the
    //      PathDB MokaCache.  All PathDB clones share Arc<MokaCache>, so this benefits the
    //      real root calculation in finish().
    let partial_results: Vec<CachedReads> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .enumerate()
            .map(|(worker_id, txs)| {
                s.spawn(move || {
                    let sp = match client.state_by_block_hash(parent_hash) {
                        Ok(sp) => sp,
                        Err(e) => {
                            warn!(
                                target: "bsc::miner::prewarm",
                                worker = worker_id, err = %e,
                                "Worker failed to open state provider"
                            );
                            return CachedReads::default();
                        }
                    };

                    let mut worker_cached = CachedReads::default();

                    // Worker-0: warm BSC system contracts via direct account reads.
                    // These are not in the pending pool but are touched every block.
                    if worker_id == 0 && prewarm_system {
                        for &addr in BSC_SYSTEM_CONTRACTS {
                            warm_account_and_code(&sp, &mut worker_cached, addr);
                        }
                    }

                    // Speculative EVM execution with bundle state tracking.
                    //
                    // We wrap CachedReadsDbMut in State<..> so we can:
                    //   - Warm worker_cached (via CachedReadsDbMut read fallthrough).
                    //   - Collect bundle_state for trie traversal afterward.
                    //
                    // `&mut state_db` implements Database (revm: impl<DB: Database> Database
                    // for &mut DB), so evm_with_env accepts it while keeping state_db accessible
                    // after the EVM is dropped.
                    {
                        let sp_db = StateProviderDatabase::new(&sp);
                        let cached_db = worker_cached.as_db_mut(sp_db);
                        let mut state_db = State::builder()
                            .with_database(cached_db)
                            .with_bundle_update()
                            .build();

                        let mut evm_env = evm_config.evm_env(parent_header);
                        // Disable validation so txs execute regardless of nonce/base-fee.
                        // Safe because we discard all execution results.
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;

                        {
                            let mut evm = evm_config.evm_with_env(&mut state_db, evm_env);
                            for tx in txs {
                                // Ignore errors: reads that happened before any error are
                                // already captured in worker_cached.
                                let _ = evm.transact(tx);
                            }
                            // evm + &mut borrow of state_db drop here
                        }

                        // ── Trie prewarm: warm PathDB MokaCache via trie traversal ──────────
                        //
                        // intermediate_hashed_post_state traverses trie nodes for the changed
                        // accounts/storage without creating a DiffLayer or committing.
                        // The PathDB MokaCache (Arc-shared across all clones) is populated,
                        // which reduces cold-node latency for the real root calculation.
                        if trie_active {
                            state_db.merge_transitions(BundleRetention::PlainState);
                            let hashed_state = sp.hashed_post_state(&state_db.bundle_state);
                            let trie_hashed_state =
                                hashed_state.to_triedb_hashed_post_state();
                            let mut triedb = rust_eth_triedb::get_global_triedb();
                            let _ = triedb.intermediate_hashed_post_state(
                                parent_state_root,
                                None, // no difflayers for speculative prewarm
                                &trie_hashed_state,
                                None, // no prefetcher
                            );
                        }
                        // state_db drops here → releases &mut worker_cached
                    }

                    worker_cached
                })
            })
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    // ── 4. Merge per-worker CachedReads into the caller's cached_reads ────────────────────────
    let mut accounts_warmed = 0usize;
    let mut contracts_warmed = 0usize;
    for partial in partial_results {
        accounts_warmed += partial.accounts.len();
        contracts_warmed += partial.contracts.len();
        for (addr, acc) in partial.accounts {
            cached_reads.accounts.entry(addr).or_insert(acc);
        }
        for (hash, code) in partial.contracts {
            cached_reads.contracts.entry(hash).or_insert(code);
        }
    }

    debug!(
        target: "bsc::miner::prewarm",
        txs_scanned = tx_count,
        accounts_warmed,
        contracts_warmed,
        workers = PREWARM_WORKERS,
        trie_prewarm = trie_active,
        "Miner EVM prewarm (Phase 2 speculative) complete"
    );
}

// ── Internal helpers ──────────────────────────────────────────────────────────────────────────

/// Reads account info + contract bytecode into `CachedReads` (used for system contracts).
fn warm_account_and_code<SP>(sp: &SP, cr: &mut CachedReads, address: Address)
where
    SP: StateProvider,
{
    if cr.accounts.contains_key(&address) {
        return;
    }

    match sp.basic_account(&address) {
        Ok(Some(acc)) => {
            let code_hash = acc.bytecode_hash.unwrap_or(revm::primitives::KECCAK_EMPTY);
            let info = AccountInfo {
                balance: acc.balance,
                nonce: acc.nonce,
                code_hash,
                code: None,
            };
            cr.insert_account(address, info, HashMap::default());

            if code_hash != revm::primitives::KECCAK_EMPTY
                && !cr.contracts.contains_key(&code_hash)
            {
                match sp.bytecode_by_hash(&code_hash) {
                    Ok(Some(bytes)) => {
                        cr.contracts.insert(code_hash, Bytecode::new_raw(bytes.original_bytes()));
                    }
                    Ok(None) => {}
                    Err(e) => trace!(
                        target: "bsc::miner::prewarm",
                        %address, %code_hash, err = %e,
                        "Failed to prewarm bytecode"
                    ),
                }
            }
        }
        Ok(None) => {
            cr.accounts.insert(address, CachedAccount { info: None, storage: HashMap::default() });
        }
        Err(e) => trace!(
            target: "bsc::miner::prewarm",
            %address, err = %e,
            "Failed to prewarm account"
        ),
    }
}
