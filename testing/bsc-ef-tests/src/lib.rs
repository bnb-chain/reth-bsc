//! BSC Execution Spec Tests runner.
//!
//! This crate provides test infrastructure for running Ethereum execution spec tests
//! against the BSC EVM implementation.

#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod case;
pub mod cases;
pub mod models;
pub mod result;
pub mod suite;

pub use case::{Case, Cases};
pub use result::{CaseResult, Error};
pub use suite::Suite;
