//! Sparse-trie integration helpers for the BSC node.
//!
//! This module is intended to host glue logic that helps integrate engine-tree sparse state-root
//! computation into the miner flow.
//!
//! Note: The trie overlay cache is used to bridge gaps when the persisted DB tip lags behind the
//! parent block we're building on, by providing per-block `HashedPostState` + `TrieUpdates` as a
//! `TrieInput` overlay.

pub mod trie_overlay;
pub mod sparse_driver;

pub use trie_overlay::{TrieOverlayCache, TrieOverlayEntry};
// Note: the overlay cache is no longer a global OnceLock; it is owned by the global SparseDriver.
pub use sparse_driver::{SparseDriver, SparseTaskKey, SparseTrieRootWaiterHandle};


