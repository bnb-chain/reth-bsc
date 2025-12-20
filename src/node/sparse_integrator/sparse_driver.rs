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
    sparse_trie::StateRootComputeOutcome,
    ExecutionEnv,
    PayloadHandle,
    PayloadProcessor,
    TreeConfig,
};
use reth_engine_primitives::ExecutableTxIterator;
use reth_evm::{execute::WithTxEnv, ConfigureEvm, OnStateHook, TxEnvFor};
use reth_primitives_traits::NodePrimitives;
use reth_provider::{providers::ConsistentDbView, BlockReader, DatabaseProviderFactory};
use reth_trie::TrieInput;
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::broadcast;
use tracing::{debug, warn};

/// Global, type-erased sparse driver.
static GLOBAL_SPARSE_DRIVER: OnceLock<Box<dyn SparseDriverDyn + Send + Sync>> = OnceLock::new();

/// Object-safe sparse driver API for a global singleton.
trait SparseDriverDyn {
    fn try_subscribe_trie_changes(&self);
}

/// Hook returned by [`SparseDriver::try_start_sparse_tree`].
///
/// This wraps the engine-tree payload handle and exposes:
/// - a state hook (to feed tx/state updates)
/// - access to the final state root computation result
pub struct SparseTreeHook<Tx, Err> {
    /// Hook to send state updates to the payload processor tasks.
    state_hook: Box<dyn OnStateHook + Send>,
    /// Underlying engine-tree handle.
    handle: PayloadHandle<Tx, Err>,
}

impl<Tx, Err> SparseTreeHook<Tx, Err> {
    /// Feed a state update into the sparse-tree pipeline.
    pub fn on_state(
        &mut self,
        source: alloy_evm::block::StateChangeSource,
        state: &reth_revm::state::EvmState,
    ) {
        self.state_hook.on_state(source, state)
    }

    /// Blocking wait for the computed state root.
    pub fn state_root(
        &mut self,
    ) -> Result<
        StateRootComputeOutcome,
        reth_trie_parallel::root::ParallelStateRootError,
    > {
        self.handle.state_root()
    }

    /// Get mutable access to the underlying payload handle.
    pub fn handle_mut(&mut self) -> &mut PayloadHandle<Tx, Err> {
        &mut self.handle
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

    /// Try to start the sparse-tree payload processor (PR36: Sparse v1).
    ///
    /// If starting succeeds, returns a [`SparseTreeHook`] that the outer code can use to:
    /// - stream state updates (via `on_state`)
    /// - await the final state root computation result
    ///
    /// Returns `None` if the given `TreeConfig` is not configured for
    /// `disable_caching_and_prewarming()` (because this helper uses
    /// `spawn_without_caching_and_prewarming`).
    pub fn try_start_sparse_tree<Evm, P, I>(
        evm_config: Evm,
        exec_env: ExecutionEnv<Evm>,
        transactions: I,
        consistent_view: ConsistentDbView<P>,
        trie_input: TrieInput,
        config: &TreeConfig,
    ) -> Option<SparseTreeHook<WithTxEnv<TxEnvFor<Evm>, I::Tx>, I::Error>>
    where
        Evm: ConfigureEvm<Primitives: NodePrimitives> + 'static,
        P: DatabaseProviderFactory<Provider: BlockReader> + Clone + 'static,
        I: ExecutableTxIterator<Evm>,
    {
        if !config.disable_caching_and_prewarming() {
            debug!(
                target: "bsc::sparse_integrator",
                "try_start_sparse_tree skipped: TreeConfig is not set to disable caching/prewarming"
            );
            return None
        }

        let mut processor = PayloadProcessor::new(
            WorkloadExecutor::default(),
            evm_config,
            config,
            PrecompileCacheMap::default(),
        );

        let handle = processor.spawn_without_caching_and_prewarming(
            exec_env,
            transactions,
            consistent_view,
            trie_input,
            config,
        );

        let state_hook: Box<dyn OnStateHook + Send> = Box::new(handle.state_hook());
        Some(SparseTreeHook { state_hook, handle })
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
