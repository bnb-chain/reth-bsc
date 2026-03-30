use crate::bench::config::{BenchConfig, Cli, Commands};
use crate::bench::payload_job_report;
use crate::bench::payload_job_runner;
use crate::bench::report;
use crate::bench::runner;
use alloy_primitives::B256;
use clap::Parser;

pub fn main() -> eyre::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Run(args) => {
            let output_path = args.output.clone();
            let label = args.label.clone();
            let config = BenchConfig::from_run_args(args)?;

            // Run inside a multi-threaded tokio runtime.
            // This is needed because BscBlockBuilder::finish() calls
            // tokio::runtime::Handle::try_current() for difflayer requests,
            // and the real miner pipeline runs inside tokio.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| eyre::eyre!("Failed to create tokio runtime: {}", e))?;

            let timings = rt.block_on(async {
                // spawn_blocking so the synchronous mining loop doesn't
                // block the async executor threads
                tokio::task::spawn_blocking(move || runner::run_benchmark(config))
                    .await
                    .map_err(|e| eyre::eyre!("Task join error: {}", e))?
            })?;

            report::write_csv(&timings, &output_path, &label)?;
            report::print_summary(&timings, &label);
        }
        Commands::PayloadJobRun(args) => {
            let output_path = args.output.clone();
            let label = args.label.clone();

            let drip_config = crate::bench::payload_job_runner::TxDripConfig {
                initial_tx_pct: args.initial_tx_pct,
                drip_delay_ms: args.tx_drip_delay_ms,
                drip_interval_ms: args.tx_drip_interval_ms,
            };

            let config = BenchConfig {
                genesis_path: args.genesis,
                private_keys: vec![
                    parse_key("937f86f4a49cafcf81a2595c5e7afd08b875b42bf05a18aa5ebc64a0af584000"),
                    parse_key("ac24b6aeb63fc825b2866a5ad628c42c1c5222c56c1c9f2cedfffd95d96c75a0"),
                    parse_key("c73e6841e8e422048a8eafb0e8a2e62059b5d4fe9195b87d49e9b6c1c635549f"),
                ],
                deployer_key: parse_key(
                    "59ba8068eb256d520179e903f43dacf6d8d57d72bd306e1bd603fdb8c8da10e8",
                ),
                num_blocks: args.iterations,
                txs_per_block: args.txs_per_iteration,
                funded_accounts: args.funded_accounts,
                background_accounts: args.background_accounts,
                storage_slots_per_account: args.storage_slots_per_account,
                chain_difflayers: args.chain_difflayers,
                triedb: args.triedb,
                output_csv: output_path.clone(),
                label: label.clone(),
            };

            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| eyre::eyre!("Failed to create tokio runtime: {}", e))?;

            let timings = rt.block_on(async {
                payload_job_runner::run_payload_job_benchmark(config, drip_config).await
            })?;

            payload_job_report::write_csv(&timings, &output_path, &label)?;
            payload_job_report::print_summary(&timings, &label);
        }
        Commands::Compare(args) => {
            report::compare(&args.baseline, &args.optimized)?;
        }
    }

    Ok(())
}

fn parse_key(hex: &str) -> B256 {
    hex.parse::<B256>().expect("hardcoded key must be valid")
}
