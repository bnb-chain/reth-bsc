pub mod bid_block;
pub(crate) mod bid_block_evidence;
pub mod bid_block_permission;
pub mod bid_simulator;
pub mod block_mev_info;
pub mod bsc_miner;
pub mod build_control;
pub mod config;
pub(crate) mod overlay;
pub(crate) mod parent_state;
pub mod payload;
pub(crate) mod pipeline;
pub(crate) mod proof_provider;
pub(crate) mod root_reuse;
pub mod signer;
pub mod util;

pub use bsc_miner::BscMiner;
pub use config::{keystore, MiningConfig};

pub(crate) mod build_diagnostics;

pub(crate) mod startup_diagnostics;

pub(crate) mod prewarm;
pub(crate) mod state_read_diagnostics;

pub(crate) mod perf_tuning;
