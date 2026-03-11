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

use alloy_consensus::{transaction::Recovered, Header as ConsensusHeader};
use alloy_evm::{Evm as EvmTrait, IntoTxEnv};
use alloy_primitives::map::HashMap;
use alloy_primitives::{Address, B256};
use reth::transaction_pool::{PoolTransaction, TransactionPool};
use reth_evm::{ConfigureEvm, TxEnvFor};
use reth_primitives::TransactionSigned;
use reth_primitives_traits::NodePrimitives;
use reth_provider::{StateProvider, StateProviderFactory};
use reth_revm::cached::{CachedAccount, CachedReads};
use reth_revm::database::StateProviderDatabase;
use revm::bytecode::Bytecode;
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
/// Must be called BEFORE `cached_reads` is consumed by `State::builder().as_db_mut()`.
pub fn prewarm_miner_evm_cache<Client, Pool, EvmConfig>(
    client: &Client,
    pool: &Pool,
    evm_config: &EvmConfig,
    parent_header: &ConsensusHeader,
    parent_hash: B256,
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

    // ── 3. Parallel speculative EVM execution ─────────────────────────────────────────────────
    //
    // Each worker opens its own StateProvider (MDBX supports concurrent readers), builds an
    // EVM with nonce/base-fee checks disabled, executes its tx slice, and returns the reads
    // it captured in a local CachedReads.
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

                    // Speculative EVM execution: all state reads populate worker_cached
                    // through CachedReadsDbMut.  The EVM's internal journal accumulates
                    // modified state across txs (simulating sequential execution); writes
                    // are discarded when the EVM is dropped at the end of this block.
                    {
                        let sp_db = StateProviderDatabase::new(&sp);
                        let db = worker_cached.as_db_mut(sp_db);

                        let mut evm_env = evm_config.evm_env(parent_header);
                        // Disable validation checks so txs execute even if nonce or
                        // base-fee would normally reject them.  Safe because we discard
                        // all execution results.
                        evm_env.cfg_env.disable_nonce_check = true;
                        evm_env.cfg_env.disable_base_fee = true;

                        let mut evm = evm_config.evm_with_env(db, evm_env);

                        for tx in txs {
                            // Ignore errors (revert / invalid tx / gas out): the reads
                            // that happened before the error are already in worker_cached.
                            let _ = evm.transact(tx);
                        }
                        // evm + db drop here → releases &mut worker_cached
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

