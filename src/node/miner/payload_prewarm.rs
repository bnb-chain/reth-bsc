//! EVM state cache pre-warming for the BSC miner.
//!
//! # Motivation
//!
//! The miner executes transactions without prior knowledge of the final tx-list (unlike
//! fullnode sync which prewarms the exact block).  Each tx therefore suffers cold EVM state
//! reads against MDBX/RocksDB.  This module pre-populates `CachedReads` before the build loop
//! to reduce those cold reads.
//!
//! Mirrors geth-bsc's `PrefetchMining()` explicit `reader.Account()` / `reader.Storage()` calls.
//!
//! # Phase 1 (implemented here)
//!
//! Top-N pending txs are collected and distributed round-robin across PREWARM_WORKERS threads.
//! Each thread opens its own read-only `StateProvider` (MDBX supports concurrent readers),
//! then processes its tx slice: reads sender / to / access_list slots, deduplicating within
//! its own local `CachedReads`.  The main thread merges all partial results.
//!
//! Also pre-warms the fixed BSC system contract addresses (called every block in
//! `apply_pre_execution_changes()`).
//!
//! Expected coverage: ~30-50% of cold reads (txs without explicit access_list won't have
//! implicit SLOAD paths covered).
//!
//! # Phase 2 (TODO – full speculative execution)
//!
//! Per-worker speculative EVM execution with `cfg_env.disable_nonce_check = true` (same flag
//! as reth's existing prewarm.rs:273) to capture implicit SLOAD paths.  Workers produce their
//! own `CachedReads` which are merged via `CachedReads::extend()`.  Expected coverage: ~80%.
//!
//! # Correctness guarantees
//!
//! - No state mutations: only `StateProvider` reads, `CachedReads` writes.
//! - System contracts: `apply_pre_execution_changes` writes to `State.bundle`, which shadows
//!   any stale values in `CachedReads` for those addresses.
//! - Build loop starts with a fresh `State`; the pre-populated `CachedReads` is a read cache
//!   only and does not affect execution semantics.

use alloy_consensus::Transaction;
use alloy_primitives::map::HashMap;
use alloy_primitives::{Address, StorageKey, B256, U256};
use reth::transaction_pool::{PoolTransaction, TransactionPool, ValidPoolTransaction};
use reth_provider::{StateProvider, StateProviderFactory};
use reth_revm::cached::{CachedAccount, CachedReads};
use revm::bytecode::Bytecode;
use revm::state::AccountInfo;
use std::sync::Arc;
use tracing::{debug, trace, warn};

/// Number of parallel worker threads for prewarm DB reads.
const PREWARM_WORKERS: usize = 5;

/// BSC system contract addresses called every block in pre/post execution.
/// Pre-warming them reduces cold reads during `apply_pre_execution_changes()`.
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
    /// Whether to also pre-warm known BSC system contract accounts.
    pub prewarm_system_contracts: bool,
}

impl Default for MinerPrewarmConfig {
    fn default() -> Self {
        // 3000 TPS × 0.45s/block = ~1350 txs/block; use 2000 for buffer.
        Self { tx_limit: 2000, prewarm_system_contracts: true }
    }
}

/// Pre-warms the miner's `CachedReads` using PREWARM_WORKERS parallel threads.
///
/// Txs are distributed round-robin across workers; each worker handles its own
/// sender/to/access_list reads against an independently-opened `StateProvider`.
///
/// Must be called BEFORE `cached_reads` is consumed by `State::builder().as_db_mut()`.
pub fn prewarm_miner_evm_cache<Client, Pool>(
    client: &Client,
    pool: &Pool,
    parent_hash: B256,
    base_fee: u64,
    cached_reads: &mut CachedReads,
    config: &MinerPrewarmConfig,
) where
    Client: StateProviderFactory,
    Pool: TransactionPool,
{
    // ── 1. Collect top-N txs from the pool ────────────────────────────────────────────────────
    let attrs = reth::transaction_pool::BestTransactionsAttributes::new(base_fee, None);
    let txs: Vec<Arc<ValidPoolTransaction<Pool::Transaction>>> =
        pool.best_transactions_with_attributes(attrs).take(config.tx_limit).collect();

    if txs.is_empty() && !config.prewarm_system_contracts {
        return;
    }

    let tx_count = txs.len();

    // ── 2. Distribute txs round-robin into PREWARM_WORKERS buckets ───────────────────────────
    let mut buckets: [Vec<Arc<ValidPoolTransaction<Pool::Transaction>>>; PREWARM_WORKERS] =
        std::array::from_fn(|_| Vec::new());
    for (i, tx) in txs.into_iter().enumerate() {
        buckets[i % PREWARM_WORKERS].push(tx);
    }

    // Worker 0 also handles BSC system contracts.
    let prewarm_system = config.prewarm_system_contracts;

    // ── 3. Spawn PREWARM_WORKERS threads; each opens its own StateProvider ────────────────────
    let partial_results: Vec<CachedReads> = std::thread::scope(|s| {
        let handles: Vec<_> = buckets
            .into_iter()
            .enumerate()
            .map(|(worker_id, chunk)| {
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
                    let mut cr = CachedReads::default();

                    // Worker 0: also prewarm BSC system contracts
                    if worker_id == 0 && prewarm_system {
                        for &addr in BSC_SYSTEM_CONTRACTS {
                            warm_account_and_code(&sp, &mut cr, addr);
                        }
                    }

                    for arc_tx in &chunk {
                        let tx = &arc_tx.transaction;
                        warm_account_and_code(&sp, &mut cr, tx.sender());
                        if let Some(to) = tx.to() {
                            warm_account_and_code(&sp, &mut cr, to);
                        }
                        if let Some(access_list) = tx.access_list() {
                            for item in access_list.iter() {
                                warm_account_and_code(&sp, &mut cr, item.address);
                                for slot in &item.storage_keys {
                                    warm_storage_slot(&sp, &mut cr, item.address, *slot);
                                }
                            }
                        }
                    }

                    cr
                })
            })
            .collect();

        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });

    // ── 4. Merge partial results into caller's CachedReads ────────────────────────────────────
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
        "Miner EVM prewarm (Phase 1) complete"
    );

    // TODO Phase 2: parallel full speculative execution
    //
    // For K workers (matching geth's prefetchMiningThread=3), each with own StateProviderDatabase
    // + CachedReads + State + EVM:
    //
    //   let mut evm_env = next_block_evm_env.clone();
    //   evm_env.cfg_env.disable_nonce_check = true;  // prewarm.rs:273
    //   let mut evm = evm_config.evm_with_env(CachedReadsDbMut<StateProviderDatabase>, evm_env);
    //   for tx in chunk {
    //       let _ = evm.transact(tx.to_tx_env());
    //       evm.db_mut().merge_transitions(BundleRetention::PlainState);
    //   }
    //   // drop evm/state; return worker_cached
    //
    // Requires: Client: Clone + Send, Evm: ConfigureEvm + Clone + Send,
    //           Pool::Transaction: ToTxEnv<TxEnvFor<Evm>>
    // Estimated coverage improvement: 30-50% → ~80%.
}

// ── Internal helpers ──────────────────────────────────────────────────────────────────────────

/// Reads account info + contract bytecode into `CachedReads`.
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

            // Load contract bytecode
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
            // Non-existent account: cache the absence so CachedReadsDbMut::basic() doesn't hit DB.
            cr.accounts.insert(address, CachedAccount { info: None, storage: HashMap::default() });
        }
        Err(e) => trace!(
            target: "bsc::miner::prewarm",
            %address, err = %e,
            "Failed to prewarm account"
        ),
    }
}

/// Reads a storage slot into `CachedReads`.
fn warm_storage_slot<SP>(sp: &SP, cr: &mut CachedReads, address: Address, slot: StorageKey)
where
    SP: StateProvider,
{
    let key = U256::from_be_bytes(slot.0);

    if cr.accounts.get(&address).is_some_and(|a| a.storage.contains_key(&key)) {
        return;
    }

    match sp.storage(address, slot) {
        Ok(value) => {
            let value = value.unwrap_or(U256::ZERO);
            match cr.accounts.get_mut(&address) {
                Some(acc) => {
                    acc.storage.insert(key, value);
                }
                None => {
                    let mut storage = HashMap::default();
                    storage.insert(key, value);
                    cr.accounts.insert(address, CachedAccount { info: None, storage });
                }
            }
        }
        Err(e) => trace!(
            target: "bsc::miner::prewarm",
            %address, %slot, err = %e,
            "Failed to prewarm storage slot"
        ),
    }
}
