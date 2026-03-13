use crate::bench::config::{BenchConfig, Cli, Commands};
use crate::bench::report;
use crate::bench::runner;
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
        Commands::Compare(args) => {
            report::compare(&args.baseline, &args.optimized)?;
        }
    }

    Ok(())
}
