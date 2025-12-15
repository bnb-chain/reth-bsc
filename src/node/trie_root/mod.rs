//! Trie root acceleration module.
//!
//! This module centralizes all trie root optimization complexity (overlay cache, subscriptions,
//! parallel/sparse computation) behind a small interface so that callers can easily fall back to
//! serial computation if needed.

pub mod root_speeder;
pub mod trie_overlay;

pub use root_speeder::{RootSpeeder, RootSpeederMode, RootSpeederPrefetch, RootSpeederUpdater};
pub use root_speeder::StateRootCompareState;
pub use trie_overlay::{TrieOverlayCache, TrieOverlayEntry, init_trie_overlay_cache, trie_overlay_cache};

