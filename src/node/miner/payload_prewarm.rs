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
//! # Two-phase design
//!
//! ## Phase 1 – EVM speculative execution (blocks until `cached_reads` is ready)
//!
//! Top-N pending txs are distributed round-robin across PREWARM_WORKERS threads.  Each thread:
//!   1. Opens its own read-only `StateProvider` (MDBX supports concurrent readers).
//!   2. Builds an EVM backed by `State<CachedReadsDbMut>` (bundle tracking enabled).
//!   3. Executes its tx slice with `disable_nonce_check = true` / `disable_base_fee = true`.
//!   4. Returns `(worker_cached, trie_hashed_state)`.
//!
//! After Phase 1, all `CachedReads` are merged and returned to the build loop.
//!
//! ## Phase 2 – Trie traversal (background, concurrent with build loop)
//!
//! Each worker's `TrieDBHashedPostState` is handed to a non-scoped background thread that
//! calls `intermediate_hashed_post_state` (no DiffLayer commit) to warm PathDB MokaCache.
//! All `get_global_triedb()` clones share `Arc<MokaCache>`, so warming from prewarm workers
//! directly benefits the real root calculation in `finish_with_difflayer()`.
//!
//! The caller receives a `Vec<JoinHandle<()>>` and **must join them before** calling
//! `finish_with_difflayer()` to guarantee the MokaCache is fully warm.
//!
//! # Correctness guarantees
//!
//! - No external state mutations: only `StateProvider` reads, `CachedReads` writes.
//! - Trie warming uses `intermediate_hashed_post_state` (read-only traversal, no commit).
//! - Speculative bundle states are discarded after extracting `TrieDBHashedPostState`.
//! - Build loop starts with a fresh `State`; `CachedReads` is a read cache only.

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
use rust_eth_triedb::TrieDBHashedPostState;
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

/// Pre-warms the miner's `CachedReads` and PathDB `MokaCache` in two concurrent phases.
///
/// **Phase 1** (blocking): speculative EVM execution across `PREWARM_WORKERS` threads populates
/// `cached_reads` and extracts per-worker `TrieDBHashedPostState`.
///
/// **Phase 2** (background): returns `Vec<JoinHandle<()>>` for trie traversal threads that warm
/// PathDB `MokaCache` concurrently with the build loop.
///
/// # Caller contract
///
/// The caller **must join all returned handles before calling `finish_with_difflayer()`**
/// to guarantee the MokaCache is warm when root hash is computed.
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
) -> Vec<std::thread::JoinHandle<()>>
where
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

    // ── 3. Phase 1: EVM speculative execution (scoped, blocks until cached_reads ready) ───────
    //
    // Each worker returns (worker_cached, Option<TrieDBHashedPostState>).
    // The trie state is extracted from the speculative bundle and passed to Phase 2 background
    // threads; it is NOT used to update real trie state.
    let pairs: Vec<(CachedReads, Option<TrieDBHashedPostState>)> =
        std::thread::scope(|s| {
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
                                return (CachedReads::default(), None);
                            }
                        };

                        let mut worker_cached = CachedReads::default();

                        // Worker-0: warm BSC system contracts via direct account reads.
                        if worker_id == 0 && prewarm_system {
                            for &addr in BSC_SYSTEM_CONTRACTS {
                                warm_account_and_code(&sp, &mut worker_cached, addr);
                            }
                        }

                        // Speculative EVM execution with bundle tracking.
                        // &mut state_db implements Database (revm: impl<DB: Database> Database
                        // for &mut DB), so evm_with_env accepts it while keeping state_db
                        // accessible after the EVM drops for trie state extraction.
                        let trie_state = {
                            let sp_db = StateProviderDatabase::new(&sp);
                            let cached_db = worker_cached.as_db_mut(sp_db);
                            let mut state_db = State::builder()
                                .with_database(cached_db)
                                .with_bundle_update()
                                .build();

                            let mut evm_env = evm_config.evm_env(parent_header);
                            evm_env.cfg_env.disable_nonce_check = true;
                            evm_env.cfg_env.disable_base_fee = true;

                            {
                                let mut evm =
                                    evm_config.evm_with_env(&mut state_db, evm_env);
                                for tx in txs {
                                    let _ = evm.transact(tx);
                                }
                                // evm drops → releases &mut state_db
                            }

                            // Extract TrieDBHashedPostState for Phase 2 background trie warming.
                            // The bundle state is consumed here; speculative writes are NOT
                            // propagated anywhere — only the hashed representation is kept.
                            if trie_active {
                                state_db.merge_transitions(BundleRetention::PlainState);
                                let hashed_state =
                                    sp.hashed_post_state(&state_db.bundle_state);
                                Some(hashed_state.to_triedb_hashed_post_state())
                            } else {
                                None
                            }
                            // state_db drops → releases worker_cached
                        };

                        (worker_cached, trie_state)
                    })
                })
                .collect();

            handles.into_iter().filter_map(|h| h.join().ok()).collect()
        });

    // ── 4. Merge per-worker CachedReads (cached_reads ready; build loop can start) ───────────
    let mut accounts_warmed = 0usize;
    let mut contracts_warmed = 0usize;
    let mut trie_states: Vec<TrieDBHashedPostState> = Vec::new();

    for (partial, trie_state) in pairs {
        accounts_warmed += partial.accounts.len();
        contracts_warmed += partial.contracts.len();
        for (addr, acc) in partial.accounts {
            cached_reads.accounts.entry(addr).or_insert(acc);
        }
        for (hash, code) in partial.contracts {
            cached_reads.contracts.entry(hash).or_insert(code);
        }
        if let Some(ts) = trie_state {
            trie_states.push(ts);
        }
    }

    debug!(
        target: "bsc::miner::prewarm",
        txs_scanned = tx_count,
        accounts_warmed,
        contracts_warmed,
        workers = PREWARM_WORKERS,
        trie_workers = trie_states.len(),
        "Phase 1 (EVM) complete; spawning Phase 2 (trie) background workers"
    );

    // ── 5. Phase 2: Trie traversal (background, concurrent with build loop) ─────────────────
    //
    // Each background thread calls intermediate_hashed_post_state (no commit) to warm the
    // shared PathDB MokaCache (Arc-shared across all get_global_triedb() clones).
    // The caller MUST join these handles before finish_with_difflayer() to guarantee
    // the MokaCache is fully warm when root hash is computed.
    if !trie_active {
        return Vec::new();
    }

    trie_states
        .into_iter()
        .map(|trie_state| {
            std::thread::spawn(move || {
                let mut triedb = rust_eth_triedb::get_global_triedb();
                let _ = triedb.intermediate_hashed_post_state(
                    parent_state_root,
                    None, // no difflayers for speculative prewarm
                    &trie_state,
                    None, // no prefetcher
                );
            })
        })
        .collect()
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
                        cr.contracts
                            .insert(code_hash, Bytecode::new_raw(bytes.original_bytes()));
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
            cr.accounts
                .insert(address, CachedAccount { info: None, storage: HashMap::default() });
        }
        Err(e) => trace!(
            target: "bsc::miner::prewarm",
            %address, err = %e,
            "Failed to prewarm account"
        ),
    }
}
