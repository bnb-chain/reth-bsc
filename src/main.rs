use clap::{Args, Parser};
use reth::{builder::NodeHandle, cli::Cli};
use reth_bsc::node::consensus::BscConsensus;
use reth_bsc::{
    chainspec::parser::BscChainSpecParser,
    node::{evm::config::BscEvmConfig, BscNode},
};
use std::sync::Arc;
use std::path::PathBuf;

// We use jemalloc for performance reasons
#[cfg(all(feature = "jemalloc", unix))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// BSC-specific command line arguments
#[derive(Debug, Clone, Args)]
#[non_exhaustive]
pub struct BscCliArgs {
    /// Enable mining
    #[arg(long = "mining.enabled")]
    pub mining_enabled: bool,

    /// Auto-generate development keys for mining
    #[arg(long = "mining.dev")]
    pub mining_dev: bool,

    /// Private key for mining (hex format, for testing only)
    /// The validator address will be automatically derived from this key
    #[arg(long = "mining.private-key")]
    pub private_key: Option<String>,

    /// Custom genesis file path
    #[arg(long = "genesis")]
    pub genesis_file: Option<PathBuf>,

    /// Use development chain with auto-generated validators
    #[arg(long = "bsc-dev")]
    pub dev_mode: bool,
}

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    Cli::<BscChainSpecParser, BscCliArgs>::parse().run_with_components::<BscNode>(
        |spec| (BscEvmConfig::new(spec.clone()), BscConsensus::new(spec)),
        async move |builder, args| {
            // Map CLI args into a global MiningConfig override before launching services
            {
                use reth_bsc::node::mining_config::{self, MiningConfig};

                let mut mining_config: MiningConfig = if args.mining_dev {
                    // Dev mode: generate ephemeral keys
                    MiningConfig::development()
                } else {
                    // Start from env, then apply CLI toggles
                    MiningConfig::from_env()
                };

                if args.mining_enabled {
                    mining_config.enabled = true;
                }

                if let Some(ref pk_hex) = args.private_key {
                    mining_config.private_key_hex = Some(pk_hex.clone());
                    // Derive validator address from provided key
                    if let Ok(sk) = mining_config::keystore::load_private_key_from_hex(pk_hex) {
                        let addr = mining_config::keystore::get_validator_address(&sk);
                        mining_config.validator_address = Some(addr);
                    }
                }

                // Ensure keys are available if enabled but none provided
                mining_config = mining_config.ensure_keys_available();

                // Best-effort set; ignore error if already set
                if let Err(_boxed_config) = mining_config::set_global_mining_config(mining_config) {
                    tracing::warn!("Mining config already set, ignoring new configuration");
                }
            }

            let (node, engine_handle_tx) = BscNode::new();
            let NodeHandle { node, node_exit_future: exit_future } =
                builder.node(node)
                    .extend_rpc_modules(move |ctx| {
                        tracing::info!("Start to register Parlia RPC API: parlia_getSnapshot");
                        use reth_bsc::rpc::parlia::{ParliaApiImpl, ParliaApiServer, DynSnapshotProvider};
                        
                        let snapshot_provider = if let Some(provider) = reth_bsc::shared::get_snapshot_provider() {
                            provider.clone()
                        } else {
                            tracing::error!("Failed to register Parlia RPC due to can not get snapshot provider");
                            return Err(eyre::eyre!("Failed to get snapshot provider"));
                        };
                        
                        let wrapped_provider = Arc::new(DynSnapshotProvider::new(snapshot_provider));
                        let parlia_api = ParliaApiImpl::new(wrapped_provider);
                        ctx.modules.merge_configured(parlia_api.into_rpc())?;

                        tracing::info!("Succeed to register Parlia RPC API");
                        Ok(())
                    })
                    .launch().await?;

            // Send the engine handle to the network
            engine_handle_tx.send(node.beacon_engine_handle.clone()).unwrap();

            exit_future.await
        },
    )?;
    Ok(())
}
