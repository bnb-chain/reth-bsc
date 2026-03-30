use alloy_primitives::B256;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// Hardcoded validator private keys (from local node-deploy keystores)
const VALIDATOR_KEY_0: &str = "937f86f4a49cafcf81a2595c5e7afd08b875b42bf05a18aa5ebc64a0af584000";
const VALIDATOR_KEY_1: &str = "ac24b6aeb63fc825b2866a5ad628c42c1c5222c56c1c9f2cedfffd95d96c75a0";
const VALIDATOR_KEY_2: &str = "c73e6841e8e422048a8eafb0e8a2e62059b5d4fe9195b87d49e9b6c1c635549f";

/// Hardcoded genesis-funded deployer account private key
const DEPLOYER_KEY: &str = "59ba8068eb256d520179e903f43dacf6d8d57d72bd306e1bd603fdb8c8da10e8";

/// Default genesis path (local copy in testing/)
const DEFAULT_GENESIS: &str = "testing/genesis_local.json";

#[derive(Parser, Debug)]
#[command(name = "miner-bench", about = "BSC execution/state-root microbenchmark")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Run the block-execution microbenchmark (direct pipeline)
    Run(RunArgs),
    /// Reserved payload-job benchmark command (currently unavailable on this branch)
    PayloadJobRun(PayloadJobRunArgs),
    /// Compare two benchmark CSV outputs
    Compare(CompareArgs),
}

#[derive(Parser, Debug)]
pub struct RunArgs {
    /// Path to genesis JSON file
    #[arg(long, default_value = DEFAULT_GENESIS)]
    pub genesis: PathBuf,

    /// Number of blocks to mine
    #[arg(long, default_value = "100")]
    pub num_blocks: usize,

    /// Number of transactions per block
    #[arg(long, default_value = "200")]
    pub txs_per_block: usize,

    /// Number of funded accounts for tx generation
    #[arg(long, default_value = "500")]
    pub funded_accounts: usize,

    /// Number of extra "background" accounts to inflate the state trie (no txs, just state bulk)
    #[arg(long, default_value = "0")]
    pub background_accounts: usize,

    /// Number of storage slots per background account (simulates token balances, approvals, etc.)
    #[arg(long, default_value = "5")]
    pub storage_slots_per_account: usize,

    /// Enable difflayer chain (unsupported on this branch; ignored by the direct benchmark)
    #[arg(long, default_value = "false")]
    pub chain_difflayers: bool,

    /// Enable trieDB for state root calculation
    #[arg(long, default_value = "false")]
    pub triedb: bool,

    /// Output CSV file path
    #[arg(long, default_value = "benchmark.csv")]
    pub output: PathBuf,

    /// Label for this benchmark run
    #[arg(long, default_value = "default")]
    pub label: String,
}

#[derive(Parser, Debug)]
pub struct PayloadJobRunArgs {
    /// Path to genesis JSON file
    #[arg(long, default_value = DEFAULT_GENESIS)]
    pub genesis: PathBuf,

    /// Number of payload-job iterations to run
    #[arg(long, default_value = "20")]
    pub iterations: usize,

    /// Number of transactions to populate in the pool per iteration
    #[arg(long, default_value = "200")]
    pub txs_per_iteration: usize,

    /// Number of funded accounts for tx generation
    #[arg(long, default_value = "500")]
    pub funded_accounts: usize,

    /// Number of extra background accounts to inflate the state trie
    #[arg(long, default_value = "0")]
    pub background_accounts: usize,

    /// Number of storage slots per background account
    #[arg(long, default_value = "5")]
    pub storage_slots_per_account: usize,

    /// Enable difflayer chaining between payload-job iterations
    #[arg(long, default_value = "false")]
    pub chain_difflayers: bool,

    /// Enable trieDB for payload finalization
    #[arg(long, default_value = "false")]
    pub triedb: bool,

    // --- Wait-Slice Parameters ---
    /// DELAY_LEFT_OVER: ms reserved for finalization (default: 120)
    #[arg(long, default_value = "120")]
    pub delay_left_over_ms: u64,

    /// TIME_MULTIPLIER: retry threshold multiplier (default: 2)
    #[arg(long, default_value = "2")]
    pub time_multiplier: u32,

    /// Grace period past expected_end_timestamp_ms in ms (default: 150)
    #[arg(long, default_value = "150")]
    pub grace_period_ms: u128,

    /// Max wait-slice duration per iteration in ms (default: 50)
    #[arg(long, default_value = "50")]
    pub max_wait_slice_ms: u64,

    /// Override mining delay in ms (default: use parlia computation).
    /// Set to e.g. 330 to simulate a realistic BSC block period.
    #[arg(long)]
    pub mining_delay_ms: Option<u64>,

    /// Percentage of txs to pre-load before starting the job (0-100, default: 100).
    /// Remaining txs are drip-fed mid-job to trigger retries and concurrent builds.
    #[arg(long, default_value = "100")]
    pub initial_tx_pct: u32,

    /// Delay in ms before starting to drip-feed remaining txs (default: 50).
    #[arg(long, default_value = "50")]
    pub tx_drip_delay_ms: u64,

    /// Interval in ms between drip-feed batches (default: 20).
    #[arg(long, default_value = "20")]
    pub tx_drip_interval_ms: u64,

    /// Output CSV file
    #[arg(long, default_value = "payload_job_benchmark.csv")]
    pub output: PathBuf,

    /// Label for this run
    #[arg(long, default_value = "default")]
    pub label: String,
}

#[derive(Parser, Debug)]
pub struct CompareArgs {
    /// Baseline CSV file
    #[arg(long)]
    pub baseline: PathBuf,

    /// Optimized CSV file
    #[arg(long)]
    pub optimized: PathBuf,
}

pub struct BenchConfig {
    pub genesis_path: PathBuf,
    pub private_keys: Vec<B256>,
    pub deployer_key: B256,
    pub num_blocks: usize,
    pub txs_per_block: usize,
    pub funded_accounts: usize,
    pub background_accounts: usize,
    pub storage_slots_per_account: usize,
    pub chain_difflayers: bool,
    pub triedb: bool,
    pub output_csv: PathBuf,
    pub label: String,
}

fn parse_key(hex: &str) -> B256 {
    hex.parse::<B256>().expect("hardcoded key must be valid")
}

impl BenchConfig {
    pub fn from_run_args(args: RunArgs) -> eyre::Result<Self> {
        Ok(Self {
            genesis_path: args.genesis,
            private_keys: vec![
                parse_key(VALIDATOR_KEY_0),
                parse_key(VALIDATOR_KEY_1),
                parse_key(VALIDATOR_KEY_2),
            ],
            deployer_key: parse_key(DEPLOYER_KEY),
            num_blocks: args.num_blocks,
            txs_per_block: args.txs_per_block,
            funded_accounts: args.funded_accounts,
            background_accounts: args.background_accounts,
            storage_slots_per_account: args.storage_slots_per_account,
            chain_difflayers: args.chain_difflayers,
            triedb: args.triedb,
            output_csv: args.output,
            label: args.label,
        })
    }
}
