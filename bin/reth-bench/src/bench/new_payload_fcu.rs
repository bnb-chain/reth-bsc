//! Runs the `reth bench` command, pushing each block to the node with
//! `engine_newPayloadBscV1` and then calling `forkchoiceUpdated`.
//!
//! This differs from `forkchoice-only` in where the block comes from. That mode
//! sends only the forkchoice update, so the node has to obtain the block itself
//! over p2p - which makes the benchmark depend on peering, and fails outright on
//! binaries that cannot handshake with the current network. Here the driver
//! fetches the block from `--rpc-url` and hands it to the node directly, so the
//! node needs no peers at all.
//!
//! The timed window is the `newPayload` call, i.e. execution plus state root.
//! Block fetching happens ahead of it in the producer task, so RPC latency does
//! not inflate the measurement the way peer download time does under
//! `forkchoice-only`.

use crate::{
    bench::{
        context::BenchContext,
        output::{
            ForkchoiceResult, TotalGasOutput, TotalGasRow, FORKCHOICE_OUTPUT_SUFFIX,
            GAS_OUTPUT_SUFFIX,
        },
    },
    valid_payload::call_forkchoice_updated,
};
use alloy_consensus::TxEnvelope;
use alloy_provider::{network::AnyRpcBlock, Provider};
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatus};
use clap::Parser;
use csv::Writer;
use reth_cli_runner::CliContext;
use reth_node_api::EngineApiMessageVersion;
use reth_node_core::args::BenchmarkArgs;
use std::time::Instant;
use tracing::{debug, info};

/// The engine method the node exposes under its `bench-test` feature.
///
/// Deliberately not `engine_newPayloadV1`: the parameter is an RLP-encoded
/// consensus block rather than a spec `ExecutionPayloadV1`. This crate is a
/// separate workspace on a different reth revision and cannot depend on
/// reth-bsc types, so it sends an Ethereum-shaped block and the node attaches
/// the BSC wrapper.
const NEW_PAYLOAD_METHOD: &str = "engine_newPayloadBscV1";

/// `reth benchmark new-payload-fcu` command
#[derive(Debug, Parser)]
pub struct Command {
    /// The RPC url to use for getting data.
    #[arg(long, value_name = "RPC_URL", verbatim_doc_comment)]
    rpc_url: String,

    #[command(flatten)]
    benchmark: BenchmarkArgs,
}

/// Converts a block fetched over RPC into the RLP the node expects.
///
/// `AnyRpcBlock` carries RPC-only decoration (block hash, size, total
/// difficulty) that is not part of consensus encoding, so it is first reduced to
/// a consensus block. The node re-derives the block hash from the header, which
/// means a faithful round trip here is what makes the pushed block acceptable -
/// any lost header field shows up as a rejected payload, not a silent mismatch.
fn block_to_rlp_hex(block: AnyRpcBlock) -> eyre::Result<String> {
    let consensus = block
        .into_inner()
        .map_header(|header| header.map(|h| h.into_header_with_defaults()))
        .try_map_transactions(|tx| -> eyre::Result<TxEnvelope> {
            tx.try_into().map_err(|_| eyre::eyre!("unsupported transaction type"))
        })?
        .into_consensus();

    Ok(format!("0x{}", alloy_primitives::hex::encode(alloy_rlp::encode(&consensus))))
}

impl Command {
    /// Execute `benchmark new-payload-fcu` command
    pub async fn execute(self, _ctx: CliContext) -> eyre::Result<()> {
        let BenchContext {
            benchmark_mode,
            block_provider,
            auth_provider,
            mut next_block,
            is_optimism: _,
            chain_id: _,
        } = BenchContext::new(&self.benchmark, self.rpc_url).await?;

        let (sender, mut receiver) = tokio::sync::mpsc::channel(1000);
        tokio::task::spawn(async move {
            while benchmark_mode.contains(next_block) {
                let block = match block_provider.get_block_by_number(next_block.into()).full().await
                {
                    Ok(Some(block)) => block,
                    Ok(None) => {
                        tracing::error!(number = next_block, "block not found on the source RPC");
                        break
                    }
                    Err(err) => {
                        tracing::error!(number = next_block, %err, "failed to fetch block");
                        break
                    }
                };
                let header = block.header.clone();
                let head_block_hash = header.hash;

                // Encode before the block leaves the producer task: this is pure
                // CPU work and keeping it off the measured path is the point of
                // prefetching.
                let block_rlp = match block_to_rlp_hex(block) {
                    Ok(rlp) => rlp,
                    Err(err) => {
                        tracing::error!(number = next_block, %err, "failed to encode block");
                        break
                    }
                };

                let safe = block_provider.get_block_by_number(header.number.saturating_sub(32).into());
                let finalized =
                    block_provider.get_block_by_number(header.number.saturating_sub(64).into());
                let (safe, finalized) = tokio::join!(safe, finalized);

                let safe_block_hash = match safe {
                    Ok(Some(b)) => b.header.hash,
                    _ => {
                        tracing::error!("safe block not available");
                        break
                    }
                };
                let finalized_block_hash = match finalized {
                    Ok(Some(b)) => b.header.hash,
                    _ => {
                        tracing::error!("finalized block not available");
                        break
                    }
                };

                next_block += 1;
                if sender
                    .send((header, block_rlp, head_block_hash, safe_block_hash, finalized_block_hash))
                    .await
                    .is_err()
                {
                    tracing::info!("Receiver closed, stopping block producer task");
                    break
                }
            }
        });

        let mut results = Vec::new();
        let total_benchmark_duration = Instant::now();
        let mut total_wait_time = std::time::Duration::ZERO;

        while let Some((header, block_rlp, head, safe, finalized)) = {
            let wait_start = Instant::now();
            let result = receiver.recv().await;
            total_wait_time += wait_start.elapsed();
            result
        } {
            let gas_used = header.gas_used;
            let block_number = header.number;

            debug!(target: "reth-bench", number=?block_number, "Sending newPayload to engine");

            // Timed: the node decodes, executes and computes the state root here.
            let start = Instant::now();
            let status: PayloadStatus = auth_provider
                .client()
                .request(NEW_PAYLOAD_METHOD, (block_rlp,))
                .await
                .map_err(|err| eyre::eyre!("{NEW_PAYLOAD_METHOD} failed at block {block_number}: {err}"))?;
            let latency = start.elapsed();

            // The node returns the status verbatim rather than erroring, so a
            // non-VALID payload has to be caught here or the run would silently
            // record timings for blocks that were never accepted.
            if !status.is_valid() {
                eyre::bail!("block {block_number} was not accepted: {status:?}");
            }

            let forkchoice_state = ForkchoiceState {
                head_block_hash: head,
                safe_block_hash: safe,
                finalized_block_hash: finalized,
            };
            call_forkchoice_updated(
                &auth_provider,
                EngineApiMessageVersion::V1,
                forkchoice_state,
                None,
            )
            .await?;

            let forkchoice_result = ForkchoiceResult { gas_used, latency };
            info!(%forkchoice_result);

            let current_duration = total_benchmark_duration.elapsed() - total_wait_time;
            results.push((TotalGasRow { block_number, gas_used, time: current_duration }, forkchoice_result));
        }

        let (gas_output_results, payload_results): (_, Vec<ForkchoiceResult>) =
            results.into_iter().unzip();

        if let Some(path) = self.benchmark.output {
            let output_path = path.join(FORKCHOICE_OUTPUT_SUFFIX);
            info!("Writing newPayload call latency output to file: {:?}", output_path);
            let mut writer = Writer::from_path(output_path)?;
            for result in payload_results {
                writer.serialize(result)?;
            }
            writer.flush()?;

            let output_path = path.join(GAS_OUTPUT_SUFFIX);
            info!("Writing total gas output to file: {:?}", output_path);
            let mut writer = Writer::from_path(output_path)?;
            for row in &gas_output_results {
                writer.serialize(row)?;
            }
            writer.flush()?;

            info!("Finished writing benchmark output files to {:?}.", path);
        }

        let gas_output = TotalGasOutput::new(gas_output_results);
        info!(
            total_duration=?gas_output.total_duration,
            total_gas_used=?gas_output.total_gas_used,
            blocks_processed=?gas_output.blocks_processed,
            "Total Ggas/s: {:.4}",
            gas_output.total_gigagas_per_second()
        );

        Ok(())
    }
}
