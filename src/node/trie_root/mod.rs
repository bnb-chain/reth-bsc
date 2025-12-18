//! Trie root acceleration module.
//!
//! This module centralizes all trie root optimization complexity (overlay cache, subscriptions,
//! parallel/sparse computation) behind a small interface so that callers can easily fall back to
//! serial computation if needed.

pub mod root_debugger;
pub mod trie_overlay;

pub use root_debugger::{
    insert_payload_processor_hook_drop, take_payload_processor_hook_drop,
    insert_payload_processor_state_root, take_payload_processor_state_root,
    insert_payload_processor_state_root_error,
    mark_payload_processor_started, take_payload_processor_started,
    wait_take_payload_processor_state_root,
    wait_take_payload_processor_state_root_blocking,
    current_payload_build_attempt, current_payload_build_trace_id,
    payload_build_attempt_scope, payload_build_trace_id_scope,
    PayloadProcessorKey,
    PayloadProcessorStateRootResult, RootDebugger,
    RootDebuggerUpdater,
};
pub use root_debugger::StateRootCompareState;
pub use trie_overlay::{TrieOverlayCache, TrieOverlayEntry, init_trie_overlay_cache, trie_overlay_cache};

