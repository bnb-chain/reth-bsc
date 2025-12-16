//! Trie root acceleration module.
//!
//! This module centralizes all trie root optimization complexity (overlay cache, subscriptions,
//! parallel/sparse computation) behind a small interface so that callers can easily fall back to
//! serial computation if needed.

pub mod root_debugger;
pub mod trie_overlay;

pub use root_debugger::{
    insert_payload_processor_hook_drop, take_payload_processor_hook_drop,
    insert_payload_processor_state_root, take_payload_processor_state_root, PayloadProcessorKey,
    PayloadProcessorStateRootResult, RootDebugger,
    RootDebuggerUpdater,
};
pub use root_debugger::StateRootCompareState;
pub use trie_overlay::{TrieOverlayCache, TrieOverlayEntry, init_trie_overlay_cache, trie_overlay_cache};

