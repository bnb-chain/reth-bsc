use clap::{Args, Parser};
use reth::{builder::NodeHandle, cli::Cli};
use reth_bsc::node::consensus::BscConsensus;
use reth_bsc::{
    chainspec::{parser::BscChainSpecParser, genesis_override},
    node::{evm::config::BscEvmConfig, BscNode},
};
use reth_bsc::consensus::parlia::bls_signer;
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

    /// Path to an Ethereum V3 keystore JSON for mining
    #[arg(long = "mining.keystore-path")]
    pub keystore_path: Option<PathBuf>,

    /// Password for the keystore file (plain string)
    #[arg(long = "mining.keystore-password")]
    pub keystore_password: Option<String>,

    /// Custom genesis file path
    #[arg(long = "genesis")]
    pub genesis_file: Option<PathBuf>,

    /// Use development chain with auto-generated validators
    #[arg(long = "bsc-dev")]
    pub dev_mode: bool,

    /// Genesis hash override for chain validation
    #[arg(long = "genesis-hash")]
    pub genesis_hash: Option<String>,

    // ---- BLS vote key management ----
    /// Path to a BLS keystore JSON for vote signing (used for voting/attestations)
    #[arg(long = "bls.keystore-path")]
    pub bls_keystore_path: Option<PathBuf>,

    /// Password for the BLS keystore file
    #[arg(long = "bls.keystore-password")]
    pub bls_keystore_password: Option<String>,

    /// BLS private key for vote signing (hex; NOT RECOMMENDED for production)
    #[arg(long = "bls.private-key")]
    pub bls_private_key: Option<String>,

    /// Enable Enhanced Validator Network (EVN) features like disabling TX broadcast
    /// to/from EVN peers. Validators and sentry nodes should enable this.
    /// Can also be set via env var `BSC_EVN_ENABLED=true`.
    #[arg(long = "evn.enabled")]
    pub evn_enabled: bool,

    /// Comma-separated devp2p NodeIDs (enode IDs) to whitelist as EVN peers.
    /// Env alternative: `BSC_EVN_NODEIDS_WHITELIST` (comma-separated)
    #[arg(long = "evn.whitelist-nodeids", value_delimiter = ',')]
    pub evn_whitelist_nodeids: Vec<String>,

    /// Comma-separated validator addresses that this node proxies (EVN behavior)
    /// Env alternative: `BSC_EVN_PROXYED_VALIDATORS` (comma-separated, 0x-prefixed)
    #[arg(long = "evn.proxyed-validator", value_delimiter = ',')]
    pub evn_proxyed_validators: Vec<String>,

    /// Comma-separated bytes32 NodeIDs to add in StakeHub (0x-prefixed)
    #[arg(long = "evn.add-nodeid", value_delimiter = ',')]
    pub evn_add_nodeids: Vec<String>,

    /// Comma-separated bytes32 NodeIDs to remove in StakeHub (0x-prefixed)
    #[arg(long = "evn.remove-nodeid", value_delimiter = ',')]
    pub evn_remove_nodeids: Vec<String>,
}

fn main() -> eyre::Result<()> {
    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // Initialize bid package queue at startup
    reth_bsc::shared::init_bid_package_queue();

    Cli::<BscChainSpecParser, BscCliArgs>::parse().run_with_components::<BscNode>(
        |spec| (BscEvmConfig::new(spec.clone()), BscConsensus::new(spec)),
        async move |builder, args| {
            // Set genesis hash override if provided
            if let Err(e) = genesis_override::set_genesis_hash_override(args.genesis_hash) {
                tracing::error!("Failed to set genesis hash override: {}", e);
                return Err(e);
            }
            
            // Map CLI args into a global MiningConfig override before launching services
            {
                use reth_bsc::node::miner::{config as mining_config, MiningConfig};

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

                if let Some(ref path) = args.keystore_path {
                    mining_config.keystore_path = Some(path.clone());
                }
                if let Some(ref pass) = args.keystore_password {
                    mining_config.keystore_password = Some(pass.clone());
                }

                // Ensure keys are available if enabled but none provided
                mining_config = mining_config.ensure_keys_available();

                // Best-effort set; ignore error if already set
                if let Err(_boxed_config) = mining_config::set_global_mining_config(mining_config) {
                    tracing::warn!("Mining config already set, ignoring new configuration");
                }
            }

            // Initialize BLS signer from environment if provided
            // Prefer CLI over env vars
            if let Some(ref path) = args.bls_keystore_path {
                let pass = args.bls_keystore_password.as_deref().unwrap_or("");
                if let Err(e) = bls_signer::init_global_bls_signer_from_keystore(path, pass) {
                    tracing::error!("Failed to init BLS signer from keystore: {}", e);
                } else {
                    tracing::info!("Initialized BLS signer from CLI keystore path");
                }
            } else if let Some(ref hex) = args.bls_private_key {
                if let Err(e) = bls_signer::init_global_bls_signer_from_hex(hex) {
                    tracing::error!("Failed to init BLS signer from hex: {}", e);
                } else {
                    tracing::warn!("Initialized BLS signer from CLI hex (dev only)");
                }
            }

            // If not yet initialized, fall back to env-based initialization
            if !bls_signer::is_bls_signer_initialized() {
                tracing::debug!("BLS signer not initialized via CLI, attempting env vars");
                bls_signer::init_from_env_if_present();
            }

            // Initialize EVN configuration (CLI overrides env)
            {
                let enabled_from_env = std::env::var("BSC_EVN_ENABLED")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
                    .unwrap_or(false);
                let evn_enabled = args.evn_enabled || enabled_from_env;
                // Collect whitelist node IDs
                let whitelist_from_env = std::env::var("BSC_EVN_NODEIDS_WHITELIST")
                    .ok()
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut whitelist_nodeids = whitelist_from_env;
                whitelist_nodeids.extend(args.evn_whitelist_nodeids.iter().cloned());

                // Collect proxyed validators
                let proxies_from_env = std::env::var("BSC_EVN_PROXYED_VALIDATORS")
                    .ok()
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut proxyed_validators = Vec::new();
                proxyed_validators.extend(proxies_from_env);
                proxyed_validators.extend(args.evn_proxyed_validators.iter().cloned());

                let mut parsed_validators = Vec::new();
                for addr_str in proxyed_validators {
                    match addr_str.parse::<alloy_primitives::Address>() {
                        Ok(addr) => parsed_validators.push(addr),
                        Err(_) => tracing::warn!("Invalid evn.proxyed-validator address: {}", addr_str),
                    }
                }

                // Parse EVN add/remove nodeids from CLI/env (optional)
                let parse_nodeids = |items: Vec<String>| -> Vec<[u8;32]> {
                    let mut out = Vec::new();
                    for s in items {
                        let s = s.trim();
                        if s.is_empty() { continue; }
                        let hex = s.strip_prefix("0x").unwrap_or(s);
                        if let Ok(bytes) = alloy_primitives::hex::decode(hex) {
                            if bytes.len() == 32 {
                                let mut arr = [0u8; 32];
                                arr.copy_from_slice(&bytes);
                                out.push(arr);
                            } else {
                                tracing::warn!("Ignoring nodeID with invalid length (need 32 bytes): {}", s);
                            }
                        } else {
                            tracing::warn!("Ignoring invalid hex nodeID: {}", s);
                        }
                    }
                    out
                };

                let cfg = reth_bsc::node::network::evn::EvnConfig {
                    enabled: evn_enabled,
                    whitelist_nodeids,
                    proxyed_validators: parsed_validators,
                    nodeids_to_add: parse_nodeids(args.evn_add_nodeids.clone()),
                    nodeids_to_remove: parse_nodeids(args.evn_remove_nodeids.clone()),
                };
                tracing::debug!(target: "bsc::init", "EVN is enabled: {}, config: {:?}", evn_enabled, cfg);
                let _ = reth_bsc::node::network::evn::set_global_evn_config(cfg);
                if evn_enabled { tracing::info!("EVN features enabled (disable peer tx broadcast)"); }
            }

            let (node, engine_handle_tx) = BscNode::new();
            let NodeHandle { node, node_exit_future: exit_future } =
                builder.node(node)
                    .extend_rpc_modules(move |ctx| {
                        tracing::info!("Start to register Parlia RPC API...");
                        use reth_bsc::rpc::parlia::{ParliaApiImpl, ParliaApiServer, DynSnapshotProvider};
                        
                        let snapshot_provider = if let Some(provider) = reth_bsc::shared::get_snapshot_provider() {
                            provider.clone()
                        } else {
                            tracing::error!("Failed to register Parlia RPC due to can not get snapshot provider");
                            return Err(eyre::eyre!("Failed to get snapshot provider"));
                        };
                        
                        let wrapped_provider = Arc::new(DynSnapshotProvider::new(snapshot_provider));
                        let parlia_api = ParliaApiImpl::new(wrapped_provider, ctx.provider().clone());
                        ctx.modules.merge_configured(parlia_api.into_rpc())?;
                        tracing::info!("Succeed to register Parlia RPC API");

                        tracing::info!("Start to register MEV RPC API...");
                        use reth_bsc::rpc::mev::{MevApiImpl, BscMevApiServer};

                        // Get snapshot provider and chain spec for MEV API
                        let snapshot_provider = if let Some(provider) = reth_bsc::shared::get_snapshot_provider() {
                            provider.clone()
                        } else {
                            tracing::warn!("Snapshot provider not available, MEV RPC API not registered");
                            return Ok(());
                        };

                        // Get chain spec from context
                        let chain_spec = std::sync::Arc::new(ctx.config().chain.clone().as_ref().clone());
                        let mev_api = MevApiImpl::new(snapshot_provider, chain_spec);
                        ctx.modules.merge_configured(mev_api.into_rpc())?;
                        tracing::info!("Succeed to register MEV RPC API");

                        tracing::info!("Start to register Blob RPC API...");
                        use reth_bsc::rpc::blob::{BlobApiImpl, BlobApiServer};

                        // Get pool and provider from context
                        let pool = ctx.pool().clone();
                        let provider = ctx.provider().clone();
                        
                        let blob_api = BlobApiImpl::new(pool, provider);
                        ctx.modules.merge_configured(blob_api.into_rpc())?;
                        tracing::info!("Succeed to register Blob RPC API");
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
