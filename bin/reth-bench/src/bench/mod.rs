//! `reth benchmark` command. Collection of various benchmarking routines.

use clap::{Parser, Subcommand};
use reth_cli_runner::CliContext;
use reth_node_core::args::LogArgs;
use reth_tracing::FileWorkerGuard;

mod context;
mod forkchoice_only;
mod new_payload_fcu;
mod output;

/// `reth bench` command
#[derive(Debug, Parser)]
pub struct BenchmarkCommand {
    #[command(subcommand)]
    command: Subcommands,

    #[command(flatten)]
    logs: LogArgs,
}

/// `reth benchmark` subcommands
#[derive(Subcommand, Debug)]
pub enum Subcommands {
    /// Benchmark which only calls subsequent `forkchoiceUpdated` calls.
    ///
    /// The node must obtain each block itself over p2p, so this requires a
    /// peered node. Use `new-payload-fcu` if the node cannot peer.
    ForkchoiceOnly(forkchoice_only::Command),
    /// Benchmark which pushes each block with `newPayload`, then calls
    /// `forkchoiceUpdated`.
    ///
    /// Requires a node built with the `bench-test` feature. Needs no peers:
    /// blocks are fetched from `--rpc-url` and handed to the node directly.
    NewPayloadFcu(new_payload_fcu::Command),
}

impl BenchmarkCommand {
    /// Execute `benchmark` command
    pub async fn execute(self, ctx: CliContext) -> eyre::Result<()> {
        // Initialize tracing
        let _guard = self.init_tracing()?;

        match self.command {
            Subcommands::ForkchoiceOnly(command) => command.execute(ctx).await,
            Subcommands::NewPayloadFcu(command) => command.execute(ctx).await,
        }
    }

    /// Initializes tracing with the configured options.
    ///
    /// If file logging is enabled, this function returns a guard that must be kept alive to ensure
    /// that all logs are flushed to disk.
    pub fn init_tracing(&self) -> eyre::Result<Option<FileWorkerGuard>> {
        let guard = self.logs.init_tracing()?;
        Ok(guard)
    }
}
