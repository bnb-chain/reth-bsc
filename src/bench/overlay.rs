// Re-export overlay types from the canonical (non-feature-gated) location so
// that bench code can continue to import from `crate::bench::overlay`.
pub use crate::node::evm::overlay::{BundleStateOverlay, MaybeOverlay};
