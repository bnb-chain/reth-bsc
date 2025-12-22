//! Sparse-trie driver utilities.
//!
//! This module provides wiring helpers to maintain a small overlay cache (`TRIE_OVERLAY`)
//! containing per-block hashed state + trie updates. This overlay can be used to bridge gaps when
//! the DB tip lags behind the chain head.
//!
//! Unlike an always-on background listener, the driver is designed to be **passively triggered**
//! from the component that already receives canonical state notifications (e.g. miner loop).

use super::trie_overlay::TrieOverlayCache;
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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Global, type-erased sparse driver.
static GLOBAL_SPARSE_DRIVER: OnceLock<Box<dyn SparseDriverDyn + Send + Sync>> = OnceLock::new();

/// Global unique task id generator.
static SPARSE_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// Object-safe sparse driver API for a global singleton.
trait SparseDriverDyn {
    fn try_subscribe_trie_changes(&self);
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

/// Key that uniquely identifies a sparse-tree payload processor instance.
///
/// This is designed for debugging and observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SparseTaskKey {
    /// Parent hash that this payload build is based on.
    pub parent_hash: alloy_primitives::B256,
    /// Number of the block being built.
    pub block_number: u64,
    /// Payload build trace id.
    pub trace_id: u64,
    /// Global monotonically-increasing id.
    pub id: u64,
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

    /// Try to start the sparse-tree payload processor (PR36: Sparse v1).
    ///
    /// If starting succeeds, returns:
    /// - a [`SparseTaskKey`] (for tracking/debugging)
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
    ) -> Option<(SparseTaskKey, Box<dyn OnStateHook>, SparseTrieRootWaiterHandle)>
    where
        Evm: ConfigureEvm<Primitives: NodePrimitives> + 'static,
        P: DatabaseProviderFactory<Provider: BlockReader + BlockNumReader + HeaderProvider>
            + Clone
            + 'static,
    {
        // Generate a globally unique key for this sparse task.
        let id = SPARSE_TASK_ID.fetch_add(1, Ordering::Relaxed);
        let key = SparseTaskKey { parent_hash, block_number, trace_id, id };

        // Build a consistent DB view at the latest tip.
        let consistent_view = match ConsistentDbView::new_with_latest_tip(provider_factory.clone()) {
            Ok(view) => view,
            Err(err) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?key,
                    %err,
                    "try_start_sparse_tree skipped: failed to create ConsistentDbView"
                );
                return None
            }
        };

        // Build the EVM env for the next block and wrap it into engine-tree's execution env.
        let evm_env = match evm_config.next_evm_env(parent_header, next_env_attributes) {
            Ok(env) => env,
            Err(_) => {
                debug!(
                    target: "bsc::sparse_integrator",
                    ?key,
                    "try_start_sparse_tree skipped: failed to build next evm env"
                );
                return None
            }
        };

        let exec_env = ExecutionEnv { evm_env, hash: Default::default(), parent_hash };

        // In miner integration we only need the state-hook channel; transactions are executed by
        // the block builder itself.
        type EmptyErr = std::convert::Infallible;
        let transactions = std::iter::empty::<Result<
            WithTxEnv<TxEnvFor<Evm>, Recovered<TxTy<Evm::Primitives>>>,
            EmptyErr,
        >>();

        let trie_input = TrieInput::default();
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

        Some((key, state_hook, waiter))
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
}
