//! Mining-time speculative execution over a shared, parent-scoped read cache.
//!
//! Execution journals stay private to each EVM. Only successful reads of the original parent
//! state enter this cache, so a miner can consume reads produced after its attempt started.

use alloy_consensus::Transaction;
use alloy_primitives::{
    map::{AddressMap, B256Map, HashMap},
    Address, B256, U256,
};
use parking_lot::{Mutex, RwLock};
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::{ConfigureEvm, Evm, EvmEnvFor};
use reth_provider::StateProviderFactory;
use reth_revm::{cancelled::ManualCancel, database::StateProviderDatabase, db::State};
use reth_transaction_pool::{
    BestTransactions, BestTransactionsAttributes, PoolTransaction, TransactionPool,
};
use revm::{bytecode::Bytecode, state::AccountInfo, Database, DatabaseCommit, DatabaseRef};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, OnceLock,
};

mod proof_hints;
pub(crate) use proof_hints::ProofHints;

const SHARDS: usize = 16;
// Entry count is not a byte limit. Code has a separate byte admission budget; map/allocator
// overhead is not included. Reaching either cap only stops admissions, never state reads.
const MAX_ENTRIES: usize = 131_072;
const MAX_CODE_BYTES: usize = 32 * 1024 * 1024;
static ENABLED: AtomicBool = AtomicBool::new(false);

/// One immutable parent identity, with concurrently populated original-state reads.
#[derive(Debug)]
pub(crate) struct SharedStateCache {
    parent: B256,
    shards: [RwLock<Shard>; SHARDS],
    entries: AtomicUsize,
    code_bytes: AtomicUsize,
    selected: RwLock<AddressMap<u64>>,
}

#[derive(Debug, Default)]
struct Shard {
    accounts: AddressMap<Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
    code: B256Map<Bytecode>,
}

impl SharedStateCache {
    pub(crate) fn new(parent: B256) -> Arc<Self> {
        Arc::new(Self {
            parent,
            shards: Default::default(),
            entries: AtomicUsize::new(0),
            code_bytes: AtomicUsize::new(0),
            selected: Default::default(),
        })
    }

    pub(crate) const fn parent(&self) -> B256 {
        self.parent
    }

    fn mark_selected(&self, sender: Address, nonce: u64) {
        let mut selected = self.selected.write();
        if let Some(previous) = selected.get_mut(&sender) {
            *previous = (*previous).max(nonce);
        } else if selected.len() < MAX_ENTRIES {
            selected.insert(sender, nonce);
        }
    }

    fn already_selected(&self, sender: Address, nonce: u64) -> bool {
        self.selected.read().get(&sender).is_some_and(|last| nonce <= *last)
    }

    pub(crate) fn account(&self, address: Address) -> Option<Option<AccountInfo>> {
        self.shards[address[0] as usize % SHARDS].read().accounts.get(&address).cloned()
    }

    pub(crate) fn storage(&self, address: Address, slot: U256) -> Option<U256> {
        self.shards[address[0] as usize % SHARDS].read().storage.get(&(address, slot)).copied()
    }

    pub(crate) fn code(&self, hash: B256) -> Option<Bytecode> {
        self.shards[hash[0] as usize % SHARDS].read().code.get(&hash).cloned()
    }

    fn reserve_entry(&self) -> bool {
        reserve(&self.entries, 1, MAX_ENTRIES)
    }

    pub(crate) fn insert_account(&self, address: Address, value: Option<AccountInfo>) {
        let mut shard = self.shards[address[0] as usize % SHARDS].write();
        if !shard.accounts.contains_key(&address) && self.reserve_entry() {
            shard.accounts.insert(address, value);
        }
    }

    pub(crate) fn insert_storage(&self, address: Address, slot: U256, value: U256) {
        let mut shard = self.shards[address[0] as usize % SHARDS].write();
        if !shard.storage.contains_key(&(address, slot)) && self.reserve_entry() {
            shard.storage.insert((address, slot), value);
        }
    }

    pub(crate) fn insert_code(&self, hash: B256, value: Bytecode) {
        let mut shard = self.shards[hash[0] as usize % SHARDS].write();
        if shard.code.contains_key(&hash) {
            return
        }
        let bytes = value.original_bytes().len();
        if !reserve(&self.code_bytes, bytes, MAX_CODE_BYTES) {
            return
        }
        if self.reserve_entry() {
            shard.code.insert(hash, value);
        } else {
            self.code_bytes.fetch_sub(bytes, Ordering::Relaxed);
        }
    }
}

fn reserve(counter: &AtomicUsize, amount: usize, limit: usize) -> bool {
    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(amount).filter(|next| *next <= limit)
        })
        .is_ok()
}

/// Read-through adapter used by speculative workers. It never receives execution writes.
#[derive(Debug)]
pub(crate) struct SharedStateDb<DB> {
    pub(crate) inner: DB,
    pub(crate) cache: Arc<SharedStateCache>,
}

impl<DB: DatabaseRef> DatabaseRef for SharedStateDb<DB> {
    type Error = DB::Error;
    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        if let Some(value) = self.cache.account(address) {
            return Ok(value)
        }
        let value = self.inner.basic_ref(address)?;
        self.cache.insert_account(address, value.clone());
        Ok(value)
    }
    fn storage_ref(&self, address: Address, slot: U256) -> Result<U256, Self::Error> {
        if let Some(value) = self.cache.storage(address, slot) {
            return Ok(value)
        }
        let value = self.inner.storage_ref(address, slot)?;
        self.cache.insert_storage(address, slot, value);
        Ok(value)
    }
    fn code_by_hash_ref(&self, hash: B256) -> Result<Bytecode, Self::Error> {
        if let Some(value) = self.cache.code(hash) {
            return Ok(value)
        }
        let value = self.inner.code_by_hash_ref(hash)?;
        self.cache.insert_code(hash, value.clone());
        Ok(value)
    }
    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.block_hash_ref(number)
    }
}

pub(crate) fn configure(enabled: bool) {
    // Allocate OS threads during node setup, never on the first miner execution path.
    let enabled = enabled && workers().is_some();
    if enabled {
        reth_engine_tree::tree::state_root_strategy::initialize_isolated_proof_prefetch();
    }
    ENABLED.store(enabled, Ordering::Relaxed);
}
pub(crate) fn cache_for_attempt(parent: B256) -> Option<Arc<SharedStateCache>> {
    ENABLED.load(Ordering::Relaxed).then(|| SharedStateCache::new(parent))
}

/// Dropping an attempt stops future speculative transactions without joining a slow EVM call.
pub(crate) struct PrewarmGuard(Arc<AtomicBool>, Arc<SharedStateCache>, Option<Arc<ProofHints>>);
impl PrewarmGuard {
    pub(crate) fn selected(&self, sender: Address, nonce: u64) {
        self.1.mark_selected(sender, nonce);
    }
}
impl Drop for PrewarmGuard {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
        if let Some(hints) = &self.2 {
            hints.close();
        }
    }
}

struct Workers {
    pool: rayon::ThreadPool,
    active: AtomicUsize,
}
struct WorkerPermit(&'static Workers);
impl Drop for WorkerPermit {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!("bsc.miner_prewarm.active_workers").decrement(1.0);
    }
}

fn worker_count(mev: bool, cpus: usize) -> usize {
    if mev {
        3
    } else {
        3.max(cpus.saturating_mul(3) / 5)
    }
}

fn workers() -> Option<&'static Workers> {
    static WORKERS: OnceLock<Option<Workers>> = OnceLock::new();
    WORKERS.get_or_init(|| {
        let mev = super::config::get_global_mining_config()
            .is_some_and(|cfg| cfg.get_bid_block_enabled());
        let count = worker_count(mev, std::thread::available_parallelism().map_or(1, usize::from));
        match rayon::ThreadPoolBuilder::new().num_threads(count)
            .thread_name(|i| format!("miner-prewarm-{i}")).build() {
            Ok(pool) => Some(Workers { pool, active: AtomicUsize::new(0) }),
            Err(error) => {
                tracing::warn!(target: "bsc::miner", %error, "Unable to start mining prewarm workers");
                None
            }
        }
    }).as_ref()
}

/// Starts the go-bsc-style mining prefetch alongside the actual transaction loop.
///
/// A shared iterator assigns each candidate once. Unlike a dispatcher with a buffered queue,
/// at most one candidate per worker is outstanding. Every worker owns its provider and EVM.
/// Process-wide permits prevent old, slow attempts from accumulating queued worker jobs.
#[allow(clippy::too_many_arguments)]
pub(crate) fn start<Pool, Client, EvmConfig>(
    cache: Option<Arc<SharedStateCache>>,
    proof_hints: Option<Arc<ProofHints>>,
    pool: Pool,
    client: Client,
    evm_config: EvmConfig,
    mut env: EvmEnvFor<EvmConfig>,
    base_fee: u64,
    blob_fee: Option<u64>,
    min_tip: u128,
    cancel: ManualCancel,
    control: Option<super::build_control::BuildControl>,
) -> Option<PrewarmGuard>
where
    Client: StateProviderFactory + Clone + 'static,
    EvmConfig: ConfigureEvm + 'static,
    EvmConfig::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
    Pool: TransactionPool<Transaction: PoolTransaction<Consensus = TransactionSigned>> + 'static,
{
    let cache = cache?;
    let workers = workers()?;
    let mut best =
        pool.best_transactions_with_attributes(BestTransactionsAttributes::new(base_fee, blob_fee));
    best.skip_blobs();
    // Match go-bsc's initial Peek: an idle pool must not open one provider per worker.
    let first = best.next()?;
    let candidates = Arc::new(Mutex::new(std::iter::once(first).chain(best)));
    let stopped = Arc::new(AtomicBool::new(false));
    env.cfg_env.disable_nonce_check = true;
    for _ in 0..workers.pool.current_num_threads() {
        if !reserve(&workers.active, 1, workers.pool.current_num_threads()) {
            metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "busy").increment(1);
            break
        }
        let permit = WorkerPermit(workers);
        metrics::gauge!("bsc.miner_prewarm.active_workers").increment(1.0);
        let (cache, client, config, env, candidates, stopped, cancel, control) = (
            cache.clone(),
            client.clone(),
            evm_config.clone(),
            env.clone(),
            candidates.clone(),
            stopped.clone(),
            cancel.clone(),
            control.clone(),
        );
        let proof_hints = proof_hints.clone();
        workers.pool.spawn(move || {
            let _permit = permit;
            let should_stop = || stopped.load(Ordering::Relaxed) || cancel.is_cancelled() ||
                control.as_ref().is_some_and(|c| c.stop_transactions(std::time::Instant::now()));
            if should_stop() { return }
            let provider = match client.state_by_block_hash(cache.parent()) {
                Ok(provider) => provider,
                Err(_) => {
                    metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "provider_error").increment(1);
                    return
                }
            };
            metrics::counter!("bsc.miner_prewarm.workers_total", "result" => "started").increment(1);
            let db = SharedStateDb { inner: StateProviderDatabase::new(provider), cache };
            let progress = db.cache.clone();
            let next = || loop {
                if should_stop() { return None }
                let tx = candidates.lock().next()?;
                // go-bsc periodically forwards its prefetch iterator to the miner's cursor.
                // Skip consumed sender nonces without changing the actual pool or its ordering.
                if progress.already_selected(tx.sender(), tx.nonce()) { continue }
                if super::super::pool::tip::meets_tip_floor(tx.effective_tip_per_gas(base_fee), min_tip) {
                    return Some(tx.transaction.clone_into_consensus());
                }
            };
            let (completed, failed) = warm_transactions(db, config, env, next, should_stop, proof_hints);
            metrics::counter!("bsc.miner_prewarm.transactions_total", "result" => "completed").increment(completed);
            metrics::counter!("bsc.miner_prewarm.transactions_total", "result" => "error").increment(failed);
        });
    }
    Some(PrewarmGuard(stopped, cache, proof_hints))
}

/// Executes against private journals while caching only the backing parent's reads.
fn warm_transactions<DB, Config>(
    db: SharedStateDb<DB>,
    config: Config,
    env: EvmEnvFor<Config>,
    mut next: impl FnMut() -> Option<reth_primitives_traits::Recovered<TransactionSigned>>,
    should_stop: impl Fn() -> bool,
    proof_hints: Option<Arc<ProofHints>>,
) -> (u64, u64)
where
    DB: DatabaseRef<Error: std::error::Error + Send + Sync + 'static> + std::fmt::Debug,
    Config: ConfigureEvm,
    Config::Primitives: reth_primitives_traits::NodePrimitives<SignedTx = TransactionSigned>,
{
    let parent = db.cache.parent();
    let mut state = State::builder().with_database_ref(db).build();
    let mut evm = config.evm_with_env(&mut state, env);
    let (mut completed, mut failed) = (0, 0);
    while !should_stop() {
        let Some(tx) = next() else { break };
        // Read explicit touches even when nonce/fee validation prevents execution.
        let _ = evm.db_mut().basic(tx.signer());
        if let Some(to) = tx.to() {
            if let Ok(Some(account)) = evm.db_mut().basic(to) {
                let _ = evm.db_mut().code_by_hash(account.code_hash);
            }
        }
        if let Some(list) = tx.access_list() {
            for item in &list.0 {
                if should_stop() {
                    return (completed, failed)
                }
                let _ = evm.db_mut().basic(item.address);
                for slot in &item.storage_keys {
                    if should_stop() {
                        return (completed, failed)
                    }
                    let _ = evm.db_mut().storage(item.address, U256::from_be_bytes(slot.0));
                }
            }
        }
        if should_stop() {
            break
        }
        match evm.transact(tx) {
            Ok(result) => {
                if !should_stop() {
                    if let Some(hints) = &proof_hints {
                        hints.record(parent, &result.state);
                    }
                }
                // Like go-bsc's worker-local StateDB, preserve speculative transitions only in
                // this worker's private State. Its backing read cache still contains parent
                // values; no write reaches that cache, the provider or the actual miner.
                evm.db_mut().commit(result.state);
                completed += 1;
            }
            Err(_) => failed += 1,
        }
    }
    if !should_stop() {
        if let Some(hints) = &proof_hints {
            hints.flush();
        }
    }
    (completed, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::{Signed, TxLegacy};
    use alloy_primitives::{Bytes, Signature, TxKind};
    use reth_primitives_traits::Recovered;
    use std::{convert::Infallible, sync::atomic::AtomicU64};

    const SENDER: Address = Address::repeat_byte(0x11);
    const CONTRACT: Address = Address::repeat_byte(0x22);

    #[derive(Debug, Clone)]
    struct ParentDb {
        reads: Arc<AtomicU64>,
        code: Bytecode,
    }
    impl Default for ParentDb {
        fn default() -> Self {
            // Write slot zero, read it, and return it. The speculative write must never be
            // cached as the parent's slot value or alter the subsequent real execution.
            Self {
                reads: Default::default(),
                code: Bytecode::new_raw(Bytes::from_static(&[
                    0x60, 0x07, 0x60, 0x00, 0x55, 0x60, 0x00, 0x54, 0x60, 0x00, 0x52, 0x60, 0x20,
                    0x60, 0x00, 0xf3,
                ])),
            }
        }
    }
    impl DatabaseRef for ParentDb {
        type Error = Infallible;
        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(if address == SENDER {
                Some(AccountInfo { balance: U256::from(1_000_000_000u64), ..Default::default() })
            } else if address == CONTRACT {
                Some(AccountInfo::default().with_code(self.code.clone()))
            } else {
                None
            })
        }
        fn storage_ref(&self, _: Address, _: U256) -> Result<U256, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(U256::from(3))
        }
        fn code_by_hash_ref(&self, _: B256) -> Result<Bytecode, Infallible> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(self.code.clone())
        }
        fn block_hash_ref(&self, _: u64) -> Result<B256, Infallible> {
            Ok(B256::ZERO)
        }
    }

    fn config_and_env(
    ) -> (crate::node::evm::BscEvmConfig, EvmEnvFor<crate::node::evm::BscEvmConfig>) {
        let cfg = crate::node::evm::BscEvmConfig::bsc(Arc::new(
            crate::chainspec::BscChainSpec::from(crate::chainspec::bsc::bsc_mainnet()),
        ));
        let env = cfg
            .evm_env(&alloy_consensus::Header {
                number: 50_000_000,
                timestamp: 1_780_000_000,
                gas_limit: 200_000_000,
                excess_blob_gas: Some(0),
                base_fee_per_gas: Some(0),
                ..Default::default()
            })
            .unwrap();
        (cfg, env)
    }

    fn transaction(nonce: u64) -> Recovered<TransactionSigned> {
        let tx = TxLegacy {
            chain_id: Some(56),
            nonce,
            gas_limit: 100_000,
            gas_price: 1,
            to: TxKind::Call(CONTRACT),
            ..Default::default()
        };
        Recovered::new_unchecked(
            TransactionSigned::Legacy(Signed::new_unchecked(
                tx,
                Signature::test_signature(),
                B256::repeat_byte(nonce as u8),
            )),
            SENDER,
        )
    }

    fn execute_block_transaction(
        db: impl DatabaseRef<Error = Infallible> + std::fmt::Debug,
    ) -> (u64, String, Vec<reth_ethereum_primitives::Receipt>, Option<AccountInfo>, U256) {
        use reth_evm::block::BlockExecutor;
        use reth_primitives_traits::SealedBlock;
        let (cfg, _) = config_and_env();
        let block = SealedBlock::seal_slow(crate::BscBlock {
            header: alloy_consensus::Header {
                number: 50_000_000,
                timestamp: 1_780_000_000,
                gas_limit: 200_000_000,
                excess_blob_gas: Some(0),
                base_fee_per_gas: Some(0),
                ..Default::default()
            },
            body: Default::default(),
        });
        let mut state = State::builder().with_database_ref(db).build();
        let mut executor = cfg.executor_for_block(&mut state, &block).unwrap();
        let output = executor.execute_transaction_without_commit(transaction(0)).unwrap();
        let gas = executor.commit_transaction(output).tx_gas_used();
        let error = executor
            .execute_transaction_without_commit(transaction(9))
            .err()
            .expect("nonce mismatch must fail")
            .to_string();
        let receipts = executor.receipts().to_vec();
        drop(executor);
        (
            gas,
            error,
            receipts,
            state.basic(SENDER).unwrap(),
            state.storage(CONTRACT, U256::ZERO).unwrap(),
        )
    }

    #[test]
    fn speculative_sstore_does_not_change_parent_or_real_execution_result() {
        let parent = ParentDb::default();
        let cache = SharedStateCache::new(B256::repeat_byte(1));
        let (hints, sent) = ProofHints::captured(cache.parent());
        let (cfg, env) = config_and_env();
        let mut speculative_env = env.clone();
        speculative_env.cfg_env.disable_nonce_check = true;
        let mut txs = vec![transaction(7)].into_iter();
        assert_eq!(
            warm_transactions(
                SharedStateDb { inner: parent.clone(), cache: cache.clone() },
                cfg.clone(),
                speculative_env,
                || txs.next(),
                || false,
                Some(hints)
            ),
            (1, 0)
        );
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        assert_eq!(cache.account(SENDER).unwrap().unwrap().nonce, 0);
        assert!(sent.lock().iter().any(|hint| hint
            .storages
            .get(&alloy_primitives::keccak256(CONTRACT))
            .is_some_and(|slots| slots.contains(&alloy_primitives::keccak256(B256::ZERO)))));

        let mut cold = State::builder().with_database_ref(parent.clone()).build();
        let cold_result =
            cfg.evm_with_env(&mut cold, env.clone()).transact(transaction(0)).unwrap();
        let mut local = reth_revm::cached::CachedReads::default();
        let (observed, _) = super::super::state_read_diagnostics::ObservedStateDb::new(
            &mut local,
            parent.clone(),
            super::super::build_diagnostics::AttemptKind::Regular,
            true,
        );
        let observed = observed.with_shared_prewarm(cache.parent(), Some(cache.clone()));
        let mut warm = State::builder().with_database(observed).build();
        let warm_result =
            cfg.evm_with_env(&mut warm, env.clone()).transact(transaction(0)).unwrap();
        assert_eq!(cold_result.result, warm_result.result, "status, gas, output and logs");
        assert_eq!(cold_result.state, warm_result.state, "account/storage writes");
        let cold_error =
            cfg.evm_with_env(&mut cold, env.clone()).transact(transaction(9)).unwrap_err();
        let warm_error = cfg.evm_with_env(&mut warm, env).transact(transaction(9)).unwrap_err();
        assert_eq!(cold_error.to_string(), warm_error.to_string());
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        assert_eq!(parent.storage_ref(CONTRACT, U256::ZERO).unwrap(), U256::from(3));
        let baseline = execute_block_transaction(parent.clone());
        let prewarmed = execute_block_transaction(SharedStateDb { inner: parent, cache });
        assert_eq!(
            baseline, prewarmed,
            "executor gas, receipt, validation error and committed state"
        );
        assert_eq!(prewarmed.3.unwrap().nonce, 1);
        assert_eq!(prewarmed.4, U256::from(7));
    }

    #[test]
    fn concurrent_workers_share_reads_without_changing_parent_identity() {
        let cache = SharedStateCache::new(B256::repeat_byte(1));
        let next_parent = SharedStateCache::new(B256::repeat_byte(2));
        let parent = ParentDb::default();
        let (cfg, mut env) = config_and_env();
        env.cfg_env.disable_nonce_check = true;
        let barrier = std::sync::Barrier::new(3);
        std::thread::scope(|scope| {
            for i in 0..3 {
                let (cache, parent) = (cache.clone(), parent.clone());
                let (cfg, env, barrier) = (cfg.clone(), env.clone(), &barrier);
                scope.spawn(move || {
                    let db = SharedStateDb { inner: parent, cache };
                    barrier.wait();
                    let mut transactions = vec![transaction(i as u64)].into_iter();
                    assert_eq!(
                        warm_transactions(
                            SharedStateDb { inner: db.inner.clone(), cache: db.cache.clone() },
                            cfg,
                            env,
                            || transactions.next(),
                            || false,
                            None
                        ),
                        (1, 0)
                    );
                    for j in 0..100 {
                        let address = Address::repeat_byte((j + i) as u8);
                        assert_eq!(db.basic_ref(address).unwrap(), db.basic_ref(address).unwrap());
                        assert_eq!(db.storage_ref(address, U256::ZERO).unwrap(), U256::from(3));
                    }
                });
            }
        });
        assert_eq!(cache.parent(), B256::repeat_byte(1));
        assert_eq!(cache.account(Address::ZERO), Some(None));
        assert_eq!(next_parent.account(Address::ZERO), None);
        assert_eq!(next_parent.storage(Address::ZERO, U256::ZERO), None);
    }

    #[test]
    fn speculative_revert_and_halt_preserve_formal_receipts_and_errors() {
        for code in [Bytes::from_static(&[0x60, 0, 0x60, 0, 0xfd]), Bytes::from_static(&[0xfe])] {
            let parent = ParentDb { code: Bytecode::new_raw(code), ..Default::default() };
            let cache = SharedStateCache::new(B256::ZERO);
            let (cfg, env) = config_and_env();
            let mut transactions = vec![transaction(0)].into_iter();
            assert_eq!(
                warm_transactions(
                    SharedStateDb { inner: parent.clone(), cache: cache.clone() },
                    cfg,
                    env,
                    || transactions.next(),
                    || false,
                    None
                ),
                (1, 0)
            );
            let cold = execute_block_transaction(parent.clone());
            let warm = execute_block_transaction(SharedStateDb { inner: parent, cache });
            assert_eq!(cold, warm);
        }
    }

    #[test]
    fn admission_caps_fall_back_to_reads_without_fabricating_values() {
        let cache = SharedStateCache::new(B256::ZERO);
        cache.entries.store(MAX_ENTRIES, Ordering::Relaxed);
        let db = SharedStateDb { inner: ParentDb::default(), cache: cache.clone() };
        assert!(db.basic_ref(SENDER).unwrap().is_some());
        assert_eq!(db.storage_ref(CONTRACT, U256::ZERO).unwrap(), U256::from(3));
        assert_eq!(cache.account(SENDER), None);
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), None);
        cache.entries.store(0, Ordering::Relaxed);
        cache.code_bytes.store(MAX_CODE_BYTES, Ordering::Relaxed);
        assert!(db.code_by_hash_ref(B256::ZERO).is_ok());
        assert_eq!(cache.code(B256::ZERO), None);
        assert_eq!(cache.entries.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dropping_guard_stops_before_selecting_another_transaction() {
        let stop = Arc::new(AtomicBool::new(false));
        let guard = PrewarmGuard(stop.clone(), SharedStateCache::new(B256::ZERO), None);
        drop(guard);
        let (cfg, env) = config_and_env();
        let result = warm_transactions(
            SharedStateDb { inner: ParentDb::default(), cache: SharedStateCache::new(B256::ZERO) },
            cfg,
            env,
            || panic!("cancelled worker selected a transaction"),
            || stop.load(Ordering::Relaxed),
            None,
        );
        assert_eq!(result, (0, 0));
    }

    #[test]
    fn worker_count_matches_go_bsc_mev_policy() {
        assert_eq!(worker_count(true, 64), 3);
        assert_eq!(worker_count(false, 64), 38);
        assert_eq!(worker_count(false, 1), 3);
    }

    #[test]
    fn mining_cursor_skips_consumed_nonces_without_skipping_future_transactions() {
        let cache = SharedStateCache::new(B256::ZERO);
        cache.mark_selected(SENDER, 3);
        cache.mark_selected(SENDER, 1);
        assert!(cache.already_selected(SENDER, 0));
        assert!(cache.already_selected(SENDER, 3));
        assert!(!cache.already_selected(SENDER, 4));
        assert!(!cache.already_selected(CONTRACT, 0));
        assert_eq!(cache.account(SENDER), None, "cursor is not execution state");
    }

    #[tokio::test]
    async fn mining_pool_workers_warm_live_cache_and_release_their_permits() {
        use reth_provider::test_utils::{ExtendedAccount, MockEthProvider};
        use reth_transaction_pool::{
            blobstore::InMemoryBlobStore, noop::MockTransactionValidator, BlockInfo,
            CoinbaseTipOrdering, EthPooledTransaction, Pool, PoolConfig, TransactionOrigin,
            TransactionPoolExt,
        };
        use std::time::Duration;

        let parent_hash = B256::repeat_byte(1);
        let provider = MockEthProvider::default();
        provider.add_account(SENDER, ExtendedAccount::new(0, U256::from(1_000_000_000u64)));
        provider.add_account(
            CONTRACT,
            ExtendedAccount::new(0, U256::ZERO)
                .with_bytecode(ParentDb::default().code.original_bytes())
                .extend_storage([(B256::ZERO, U256::from(3))]),
        );
        let pool = Pool::new(
            MockTransactionValidator::<EthPooledTransaction>::default(),
            CoinbaseTipOrdering::default(),
            InMemoryBlobStore::default(),
            PoolConfig::default().with_disabled_protocol_base_fee(),
        );
        let (cfg, env) = config_and_env();
        assert!(
            start(
                Some(SharedStateCache::new(parent_hash)),
                None,
                pool.clone(),
                provider.clone(),
                cfg.clone(),
                env.clone(),
                0,
                None,
                0,
                ManualCancel::default(),
                None
            )
            .is_none(),
            "empty pool must not schedule workers"
        );
        pool.set_block_info(BlockInfo {
            last_seen_block_hash: parent_hash,
            block_gas_limit: 200_000_000,
            pending_basefee: 0,
            ..Default::default()
        });
        pool.add_transaction(
            TransactionOrigin::External,
            EthPooledTransaction::new(transaction(0), 200),
        )
        .await
        .unwrap();
        let cache = SharedStateCache::new(parent_hash);
        let (hints, sent) = ProofHints::captured(parent_hash);
        let guard = start(
            Some(cache.clone()),
            Some(hints),
            pool,
            provider.clone(),
            cfg,
            env,
            0,
            None,
            0,
            ManualCancel::default(),
            None,
        )
        .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while cache.storage(CONTRACT, U256::ZERO).is_none() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual worker did not warm the contract storage");
        assert_eq!(cache.storage(CONTRACT, U256::ZERO), Some(U256::from(3)));
        tokio::time::timeout(Duration::from_secs(5), async {
            while sent.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("actual worker did not prefetch the modified storage path");
        assert!(sent.lock().iter().any(|hint| hint
            .storages
            .get(&alloy_primitives::keccak256(CONTRACT))
            .is_some_and(|slots| slots.contains(&alloy_primitives::keccak256(B256::ZERO)))));
        drop(guard);
        tokio::time::timeout(Duration::from_secs(5), async {
            while workers().unwrap().active.load(Ordering::Relaxed) != 0 {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("worker permits leaked");
        use reth_provider::AccountReader;
        assert_eq!(provider.basic_account(&SENDER).unwrap().unwrap().nonce, 0);
    }
}
