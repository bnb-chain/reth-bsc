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

#[derive(Parser, Debug, Clone)]
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

    /// Directory where reusable benchmark DB snapshots are stored
    #[arg(long)]
    pub cache_dir: Option<PathBuf>,

    /// Reuse a cached post-genesis benchmark DB if present, otherwise create and cache it
    #[arg(long, default_value = "false")]
    pub reuse_genesis_db: bool,

    /// Reuse a cached post-setup benchmark DB if present, otherwise create and cache it
    #[arg(long, default_value = "false")]
    pub reuse_post_setup_db: bool,
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

#[derive(Debug, Clone)]
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
    pub cache_dir: Option<PathBuf>,
    pub reuse_genesis_db: bool,
    pub reuse_post_setup_db: bool,
}

fn parse_key(hex: &str) -> B256 {
    hex.parse::<B256>().expect("hardcoded key must be valid")
}

impl BenchConfig {
    pub fn from_run_args(args: RunArgs) -> eyre::Result<Self> {
        if (args.reuse_genesis_db || args.reuse_post_setup_db) && args.cache_dir.is_none() {
            eyre::bail!(
                "--cache-dir is required when using --reuse-genesis-db or --reuse-post-setup-db"
            );
        }

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
            cache_dir: args.cache_dir,
            reuse_genesis_db: args.reuse_genesis_db,
            reuse_post_setup_db: args.reuse_post_setup_db,
        })
    }

    pub fn wants_genesis_cache(&self) -> bool {
        self.reuse_genesis_db || self.reuse_post_setup_db
    }

    pub fn wants_post_setup_cache(&self) -> bool {
        self.reuse_post_setup_db
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::cache::state_cache_key;
    use clap::Parser;

    fn parse_run_args(args: &[&str]) -> RunArgs {
        let cli = Cli::try_parse_from(args).expect("cli args should parse");
        match cli.command {
            Commands::Run(run_args) => run_args,
            _ => panic!("expected run command"),
        }
    }

    #[test]
    fn run_args_parse_cache_flags() {
        let args = parse_run_args(&[
            "miner-bench",
            "run",
            "--cache-dir",
            "/tmp/bench-cache",
            "--reuse-genesis-db",
            "--reuse-post-setup-db",
        ]);

        assert_eq!(args.cache_dir, Some(PathBuf::from("/tmp/bench-cache")));
        assert!(args.reuse_genesis_db);
        assert!(args.reuse_post_setup_db);
    }

    #[test]
    fn from_run_args_requires_cache_dir_when_reuse_is_enabled() {
        let args = RunArgs {
            genesis: PathBuf::from(DEFAULT_GENESIS),
            num_blocks: 100,
            txs_per_block: 200,
            funded_accounts: 500,
            background_accounts: 0,
            storage_slots_per_account: 5,
            chain_difflayers: false,
            triedb: false,
            output: PathBuf::from("benchmark.csv"),
            label: "default".to_string(),
            cache_dir: None,
            reuse_genesis_db: true,
            reuse_post_setup_db: false,
        };

        let err = BenchConfig::from_run_args(args).expect_err("cache-dir should be required");
        assert!(err.to_string().contains("--cache-dir"));
    }

    #[test]
    fn state_cache_key_ignores_run_length_and_output_fields() {
        let config_a = BenchConfig {
            genesis_path: PathBuf::from(DEFAULT_GENESIS),
            private_keys: vec![],
            deployer_key: B256::ZERO,
            num_blocks: 100,
            txs_per_block: 6000,
            funded_accounts: 5_000,
            background_accounts: 10_000_000,
            storage_slots_per_account: 1,
            chain_difflayers: false,
            triedb: true,
            output_csv: PathBuf::from("first.csv"),
            label: "first".to_string(),
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            reuse_genesis_db: true,
            reuse_post_setup_db: true,
        };

        let config_b = BenchConfig {
            num_blocks: 1,
            txs_per_block: 1,
            output_csv: PathBuf::from("second.csv"),
            label: "second".to_string(),
            ..config_a.clone()
        };

        let genesis_json =
            "{\"alloc\":{},\"config\":{},\"gasLimit\":\"0x1\",\"difficulty\":\"0x1\"}";

        assert_eq!(
            state_cache_key(&config_a, genesis_json),
            state_cache_key(&config_b, genesis_json)
        );
    }

    #[test]
    fn state_cache_key_changes_when_state_shape_changes() {
        let base = BenchConfig {
            genesis_path: PathBuf::from(DEFAULT_GENESIS),
            private_keys: vec![],
            deployer_key: B256::ZERO,
            num_blocks: 100,
            txs_per_block: 6000,
            funded_accounts: 5_000,
            background_accounts: 1_000_000,
            storage_slots_per_account: 1,
            chain_difflayers: false,
            triedb: true,
            output_csv: PathBuf::from("out.csv"),
            label: "label".to_string(),
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            reuse_genesis_db: true,
            reuse_post_setup_db: false,
        };

        let changed = BenchConfig { background_accounts: 10_000_000, ..base.clone() };
        let genesis_json =
            "{\"alloc\":{},\"config\":{},\"gasLimit\":\"0x1\",\"difficulty\":\"0x1\"}";

        assert_ne!(state_cache_key(&base, genesis_json), state_cache_key(&changed, genesis_json));
    }
}
