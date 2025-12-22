//! Sparse-trie driver utilities.
//!
//! This module provides wiring helpers to maintain a small overlay cache (`TRIE_OVERLAY`)
//! containing per-block hashed state + trie updates. This overlay can be used to bridge gaps when
//! the DB tip lags behind the chain head.
//!
//! Unlike an always-on background listener, the driver is designed to be **passively triggered**
//! from the component that already receives canonical state notifications (e.g. miner loop).

use super::trie_overlay::TrieOverlayCache;
use super::trie_overlay::TrieOverlayEntry;
use alloy_consensus::BlockHeader;
use parking_lot::RwLock;
use parking_lot::Mutex;
use reth_chain_state::{NewCanonicalChain, NewCanonicalChainSubscriptions};
use reth_engine_tree::tree::{
    executor::WorkloadExecutor,
    precompile_cache::PrecompileCacheMap,
    ExecutionEnv,
    PayloadProcessor,
    TreeConfig,
};
use reth_evm::{execute::WithTxEnv, ConfigureEvm, OnStateHook, TxEnvFor};
use reth_primitives_traits::{NodePrimitives, Recovered, TxTy};
use reth_provider::{providers::ConsistentDbView, BlockNumReader, BlockReader, DatabaseProviderFactory, HeaderProvider};
use reth_trie::TrieInput;
use std::sync::Arc;
use std::sync::OnceLock;
use std::ops::RangeInclusive;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Global, type-erased sparse driver.
static GLOBAL_SPARSE_DRIVER: OnceLock<Box<dyn SparseDriverDyn + Send + Sync>> = OnceLock::new();

/// Object-safe sparse driver API for a global singleton.
trait SparseDriverDyn {
    fn try_subscribe_trie_changes(&self);
    fn overlay_entries(&self, range: RangeInclusive<u64>) -> Vec<TrieOverlayEntry>;
}

/// A cloneable handle that allows waiting for the sparse trie state root result in `finish()`.
pub type SparseTrieRootWaiterHandle = Arc<Mutex<Box<dyn SparseTrieRootWaiter>>>;

/// Type-erased waiter for sparse trie state root computation.
pub trait SparseTrieRootWaiter: Send {
    fn wait(&mut self) -> Result<reth_engine_tree::tree::sparse_trie::StateRootComputeOutcome, reth_trie_parallel::root::ParallelStateRootError>;
}

struct SparseTrieRootWaiterImpl<Tx, Err> {
    handle: reth_engine_tree::tree::PayloadHandle<Tx, Err>,
}

impl<Tx, Err> SparseTrieRootWaiter for SparseTrieRootWaiterImpl<Tx, Err>
where
    Tx: Send + 'static,
    Err: Send + 'static,
{
    fn wait(
        &mut self,
    ) -> Result<reth_engine_tree::tree::sparse_trie::StateRootComputeOutcome, reth_trie_parallel::root::ParallelStateRootError>
    {
        self.handle.state_root()
    }
}

/// Public facade for the global sparse driver.
///
/// This is intentionally **not** generic: we store a type-erased implementation internally so that
/// callers don't need to carry provider type parameters around.
#[derive(Debug, Clone, Copy)]
pub struct SparseDriver;

impl SparseDriver {
    /// Initialize the global sparse driver once.
    ///
    /// Returns `true` if initialized by this call, `false` if it was already initialized.
    pub fn init_global<P>(provider: P, overlay_cache_capacity: usize) -> bool
    where
        P: NewCanonicalChainSubscriptions + Clone + Send + Sync + 'static,
        P::Primitives: NodePrimitives + 'static,
    {
        let driver = SparseDriverImpl::new(provider, overlay_cache_capacity);
        GLOBAL_SPARSE_DRIVER.set(Box::new(driver)).is_ok()
    }

    /// Passively process any pending canonical notifications and update `TRIE_OVERLAY`.
    ///
    /// This is **non-blocking**: it drains whatever is available via `try_recv` and returns.
    pub fn try_subscribe_trie_changes() {
        if let Some(driver) = GLOBAL_SPARSE_DRIVER.get() {
            driver.try_subscribe_trie_changes();
        }
    }

    /// Try to start the sparse-tree payload processor.
    ///
    /// If starting succeeds, returns:
    /// - a state hook to be installed into the block executor (`set_state_hook`)
    /// - a waiter handle that `finish()` can use to block for the final trie root result
    ///
    /// Note: This helper always uses `spawn_without_caching_and_prewarming` and therefore expects
    /// the caller to decide *when* it's appropriate to start this task (e.g. miner mode only).
    pub fn try_start_sparse_tree<Evm, P>(
        parent_hash: alloy_primitives::B256,
        block_number: u64,
        trace_id: u64,
        parent_header: &<Evm::Primitives as NodePrimitives>::BlockHeader,
        next_env_attributes: &Evm::NextBlockEnvCtx,
        provider_factory: P,
        evm_config: Evm,
    ) -> Option<(Box<dyn OnStateHook>, SparseTrieRootWaiterHandle)>
    where
        Evm: ConfigureEvm<Primitives: NodePrimitives> + 'static,
        P: DatabaseProviderFactory<Provider: BlockReader + BlockNumReader + HeaderProvider>
            + Clone
            + 'static,
    {
        // - Only start sparse pipeline if DB tip matches the parent header OR we have a complete
        //   overlay (hashed_state + trie_updates) for all missing blocks up to parent.
        //
        // This prevents producing invalid blocks due to misaligned base state.
        let parent_number = parent_header.number();
        let db_provider = match provider_factory.database_provider_ro() {
            Ok(p) => p,
            Err(err) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    %err,
                    "try_start_sparse_tree skipped: failed to open db provider"
                );
                return None
            }
        };
        // use canonical best block number as the DB tip reference.
        let db_last = match db_provider.best_block_number() {
            Ok(n) => n,
            Err(err) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    %err,
                    "try_start_sparse_tree skipped: failed to read db best_block_number"
                );
                return None
            }
        };
        // If DB tip is ahead of the parent, we cannot safely build a sparse root for the parent
        // without carefully pinning state to the parent.
        if db_last > parent_number {
            debug!(
                target: "bsc::sparse_integrator",
                ?parent_hash,
                block_number,
                trace_id,
                db_last,
                parent_number,
                "try_start_sparse_tree skipped: db tip ahead of parent"
            );
            return None
        }
        let db_tip = match db_provider.sealed_header(db_last) {
            Ok(h) => h,
            Err(err) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    %err,
                    "try_start_sparse_tree skipped: failed to read db sealed_header"
                );
                return None
            }
        };
        let Some(db_tip) = db_tip else {
            debug!(
                target: "bsc::sparse_integrator",
                ?parent_hash,
                block_number,
                trace_id,
                db_last,
                "try_start_sparse_tree skipped: db tip header missing"
            );
            return None
        };

        // If DB is at parent height but hash differs, parent isn't the db tip -> unsafe.
        if db_last == parent_number && db_tip.hash() != parent_hash {
            debug!(
                target: "bsc::sparse_integrator",
                ?parent_hash,
                block_number,
                trace_id,
                db_last,
                parent_number,
                db_tip_hash = ?db_tip.hash(),
                "try_start_sparse_tree skipped: db tip hash mismatch at parent height"
            );
            return None
        }

        // Prepare trie input overlay if DB tip lags behind parent.
        let mut trie_input = TrieInput::default();
        if db_last < parent_number {
            let Some(driver) = GLOBAL_SPARSE_DRIVER.get() else {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    db_last,
                    parent_number,
                    "try_start_sparse_tree skipped: no global sparse driver for overlay coverage"
                );
                return None
            };

            let needed_range: RangeInclusive<u64> = (db_last + 1)..=parent_number;
            let overlays = driver.overlay_entries(needed_range.clone());

            // Require full coverage.
            if overlays.len() != (parent_number - db_last) as usize {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    db_last,
                    parent_number,
                    overlays_len = overlays.len(),
                    "try_start_sparse_tree skipped: trie overlay cache does not fully cover needed range"
                );
                return None
            }

            // Validate hash chain endpoint and require trie_updates for each block.
            for entry in overlays.iter() {
                if entry.number == parent_number && entry.hash != parent_hash {
                    debug!(
                        target: "bsc::sparse_integrator",
                        ?parent_hash,
                        block_number,
                        trace_id,
                        entry_number = entry.number,
                        entry_hash = ?entry.hash,
                        "try_start_sparse_tree skipped: overlay hash mismatch at parent"
                    );
                    return None
                }
                let Some(nodes) = entry.trie_updates.as_deref() else {
                    debug!(
                        target: "bsc::sparse_integrator",
                        ?parent_hash,
                        block_number,
                        trace_id,
                        entry_number = entry.number,
                        entry_hash = ?entry.hash,
                        "try_start_sparse_tree skipped: missing trie_updates in overlay entry"
                    );
                    return None
                };
                trie_input.append_cached_ref(nodes, &entry.hashed_state);
            }
        }

        // Build a consistent DB view pinned to a safe tip.
        //
        // - If parent is on-disk and matches the parent hash, pin to parent directly.
        // - Otherwise (db tip behind parent), pin to db_last and rely on `trie_input` overlays.
        let consistent_view = if db_last == parent_number && db_tip.hash() == parent_hash {
            ConsistentDbView::new(provider_factory.clone(), Some((parent_hash, parent_number)))
        } else {
            ConsistentDbView::new(provider_factory.clone(), Some((db_tip.hash(), db_last)))
        };

        // Build the EVM env for the next block and wrap it into engine-tree's execution env.
        let evm_env = match evm_config.next_evm_env(parent_header, next_env_attributes) {
            Ok(env) => env,
            Err(_) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?parent_hash,
                    block_number,
                    trace_id,
                    "try_start_sparse_tree skipped: failed to build next evm env"
                );
                return None
            }
        };

        let exec_env = ExecutionEnv { evm_env, hash: parent_hash, parent_hash };

        // In miner integration we only need the state-hook channel; transactions are executed by
        // the block builder itself.
        type EmptyErr = std::convert::Infallible;
        let transactions = std::iter::empty::<Result<
            WithTxEnv<TxEnvFor<Evm>, Recovered<TxTy<Evm::Primitives>>>,
            EmptyErr,
        >>();

        // `spawn_without_caching_and_prewarming` requires this flag to be set (see debug_assert).
        let config = TreeConfig::default().without_caching_and_prewarming(true);

        let mut processor = PayloadProcessor::new(
            WorkloadExecutor::default(),
            evm_config,
            &config,
            PrecompileCacheMap::default(),
        );

        let handle = processor.spawn_without_caching_and_prewarming(
            exec_env,
            transactions,
            consistent_view,
            trie_input,
            &config,
        );

        // Create a state hook that can be installed into the executor. This captures a clone of
        // the underlying sender so it can live independently of the waiter.
        let state_hook: Box<dyn OnStateHook> = Box::new(handle.state_hook());

        // Bind the payload processor instance to the task key by storing it in a waiter.
        let waiter: SparseTrieRootWaiterHandle = Arc::new(Mutex::new(Box::new(
            SparseTrieRootWaiterImpl { handle },
        )));

        Some((state_hook, waiter))
    }
}

#[derive(Debug)]
struct SparseDriverImpl<P>
where
    P: NewCanonicalChainSubscriptions + Clone + Send + Sync + 'static,
    P::Primitives: NodePrimitives + 'static,
{
    overlay: Arc<RwLock<TrieOverlayCache>>,
    rx: Mutex<broadcast::Receiver<NewCanonicalChain<P::Primitives>>>,
    _provider: P,
}

impl<P> SparseDriverImpl<P>
where
    P: NewCanonicalChainSubscriptions + Clone + Send + Sync + 'static,
    P::Primitives: NodePrimitives + 'static,
{
    fn new(provider: P, overlay_cache_capacity: usize) -> Self {
        let overlay = Arc::new(RwLock::new(TrieOverlayCache::new(overlay_cache_capacity)));
        let rx = provider.subscribe_to_new_canonical_chain();
        Self { overlay, rx: Mutex::new(rx), _provider: provider }
    }

    fn handle_update(&self, update: &NewCanonicalChain<P::Primitives>) {
        match update {
            NewCanonicalChain::Commit { new } => {
                for block in new {
                    self.overlay.write().insert_from_executed(block);
                }
            }
            NewCanonicalChain::Reorg { new, old } => {
                // evict old segment
                for old_block in old {
                    let number = old_block.recovered_block().number() as u64;
                    self.overlay.write().remove_range(number..=number);
                }
                // insert new segment
                for block in new {
                    self.overlay.write().insert_from_executed(block);
                }
            }
        }
    }

    fn drain(&self) {
        let mut rx = self.rx.lock();
        loop {
            match rx.try_recv() {
                Ok(update) => self.handle_update(&update),
                Err(broadcast::error::TryRecvError::Empty) => break,
                Err(broadcast::error::TryRecvError::Closed) => break,
                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                    warn!(
                        target: "bsc::sparse_integrator",
                        lagged = n,
                        "new canonical chain subscription lagged; trie overlay cache may be incomplete"
                    );
                    continue;
                }
            }
        }
    }
}

impl<P> SparseDriverDyn for SparseDriverImpl<P>
where
    P: NewCanonicalChainSubscriptions + Clone + Send + Sync + 'static,
    P::Primitives: NodePrimitives + 'static,
{
    fn try_subscribe_trie_changes(&self) {
        self.drain();
    }

    fn overlay_entries(&self, range: RangeInclusive<u64>) -> Vec<TrieOverlayEntry> {
        self.overlay.read().get_range(range)
    }
}
