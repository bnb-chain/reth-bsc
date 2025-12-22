//! Sparse trie root acceleration for miner block production.
//!
//! This module hosts the BSC-specific glue that integrates reth's sparse/parallel state-root
//! computation into the miner block building flow.
//!
//! The goal is **performance**: while producing blocks, we try to compute the final state root
//! (and corresponding trie updates) using the engine-tree sparse pipeline, so that `builder.finish()`
//! can avoid slow serial trie traversal whenever possible.
//!
//! It also maintains a small per-block overlay cache (hashed post-state + optional trie updates)
//! to bridge short gaps when the persisted DB tip lags behind the canonical head.

pub mod trie_overlay;
pub mod sparse_driver;

pub use trie_overlay::{TrieOverlayCache, TrieOverlayEntry};
pub use sparse_driver::{SparseDriver, SparseTrieRootWaiterHandle};


