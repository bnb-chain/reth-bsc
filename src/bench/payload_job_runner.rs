use crate::bench::config::BenchConfig;

/// Configuration for drip-feeding transactions mid-job to trigger retries.
#[derive(Debug, Clone, Copy)]
pub struct TxDripConfig {
    /// Percentage of txs to pre-load before starting the job (0-100).
    pub initial_tx_pct: u32,
    /// Delay in ms before starting to drip-feed remaining txs.
    pub drip_delay_ms: u64,
    /// Interval in ms between drip-feed batches.
    pub drip_interval_ms: u64,
}

/// Timing record for a single payload-job iteration.
#[derive(Debug, Clone)]
pub struct PayloadJobTiming {
    pub iteration: usize,
    pub block_number: u64,
    /// Time from job start to result received (microseconds).
    pub job_duration_us: u128,
    /// Transaction count in the winning payload.
    pub tx_count: usize,
    /// Gas used in the winning payload.
    pub gas_used: u64,
    /// Build kind of the winning payload.
    pub build_kind: String,
    /// exec_duration from BscBuiltPayload (microseconds).
    pub exec_duration_us: u128,
    /// trie_root_duration from BscBuiltPayload (microseconds).
    pub trie_root_duration_us: u128,
}

/// Run the payload-job benchmark.
///
/// NOTE: This subcommand is not available on the `bench-miner-opt-split` branch
/// because the upstream refactor removed `BscPayloadJob`, `WaitSliceConfig`,
/// and related scheduling APIs. Use the `run` subcommand instead.
pub async fn run_payload_job_benchmark(
    _config: BenchConfig,
    _drip_config: TxDripConfig,
) -> eyre::Result<Vec<PayloadJobTiming>> {
    Err(eyre::eyre!(
        "PayloadJobRun is not available on this branch. \
         Use the `run` subcommand instead."
    ))
}
