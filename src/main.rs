use clap::{Args, Parser};
use reth::{
    builder::NodeHandle,
    cli::Cli,
    consensus::FullConsensus,
    version::{default_reth_version_metadata, try_init_version_metadata},
};
use reth_bsc::consensus::parlia::bls_signer;
use reth_bsc::node::consensus::BscConsensus;
use reth_bsc::{
    chainspec::{genesis_override, parser::BscChainSpecParser},
    node::{evm::config::BscEvmConfig, BscNode},
    BscPrimitives,
};
use std::path::PathBuf;
use std::sync::Arc;

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

    /// Target gas limit for mined blocks (e.g., 30000000 for 30M, 140000000 for 140M)
    #[arg(long = "mining.gas-limit")]
    pub mining_gas_limit: Option<u64>,

    /// Minimum gas tip for mined blocks (e.g., 1000000000 for 1G, 1000000000000 for 1T)
    #[arg(long = "mining.min-gas-tip")]
    pub mining_min_gas_tip: Option<u128>,

    /// Use reth 2.0 sparse-trie background task for state-root computation in
    /// payload build (opt-in).
    ///
    /// Env alternative: `BSC_MINING_USE_SPARSE_TRIE_STATE_ROOT=true`.
    #[arg(long = "mining.use-sparse-trie-state-root")]
    pub mining_use_sparse_trie_state_root: bool,

    /// Accept BEP-675 builder-proposed blocks via `mev_sendBidBlock`.
    ///
    /// Env alternative: `BSC_MINING_BID_BLOCK_ENABLED=true`.
    #[arg(long = "mining.bid-block-enabled")]
    pub mining_bid_block_enabled: bool,

    /// Port for the optional BEP-675 `BidBlockService` gRPC listener. Zero disables it.
    ///
    /// Env alternative: `BSC_MEV_GRPC_PORT`.
    #[arg(long = "mining.mev-grpc-port")]
    pub mining_mev_grpc_port: Option<u16>,

    /// Maximum process-wide in-flight gRPC `SendBidBlock` requests.
    ///
    /// Env alternative: `BSC_MEV_GRPC_CONCURRENCY`.
    #[arg(long = "mining.mev-grpc-concurrency")]
    pub mining_mev_grpc_concurrency: Option<u32>,

    /// Total gRPC `SendBidBlock` timeout in milliseconds.
    ///
    /// Env alternative: `BSC_MEV_GRPC_REQUEST_TIMEOUT_MS`.
    #[arg(long = "mining.mev-grpc-request-timeout-ms")]
    pub mining_mev_grpc_request_timeout_ms: Option<u64>,

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

    /// Comma-separated devp2p NodeIDs (enode IDs) to mark as proxyed peers.
    /// These peers will receive priority block/vote broadcasts.
    /// Env alternative: `BSC_PROXYED_PEER_IDS` (comma-separated)
    #[arg(long = "proxyed-peers", value_delimiter = ',')]
    pub proxyed_peer_ids: Vec<String>,

    /// Disable TX broadcast forbidden for EVN peers.
    #[arg(long = "evn.disable-tx-broadcast-forbidden")]
    pub evn_disable_tx_broadcast_forbidden: bool,
}

/// BSC default for `--gpo.ignoreprice` (wei), matching geth and pre-v2.2 reth-bsc.
///
/// BSC blocks carry many legitimate zero-gas-price transactions, most of them not sent
/// by the block's coinbase, so with upstream's default threshold of 0 they dominate the
/// gas-price-oracle sample and `eth_gasPrice` / `eth_maxPriorityFeePerGas` return 0.
const BSC_DEFAULT_GPO_IGNORE_PRICE: u64 = 2;

/// Returns true if `--gpo.ignoreprice` was passed explicitly, either as
/// `--gpo.ignoreprice <value>` or `--gpo.ignoreprice=<value>`.
fn user_set_gpo_ignore_price(mut args: impl Iterator<Item = String>) -> bool {
    args.any(|arg| arg == "--gpo.ignoreprice" || arg.starts_with("--gpo.ignoreprice="))
}

fn main() -> eyre::Result<()> {
    // Override reth's global version metadata so startup/P2P logs identify
    // this binary as Reth-BSC with its own version + commit.
    {
        use std::borrow::Cow;
        // The BSC release identifier (e.g. `0.1.0-fix`), derived from the nearest
        // git tag in build.rs. This is what distinguishes patch/fix releases that
        // share a Cargo package version.
        let bsc_version = env!("RETH_BSC_VERSION");
        let git_sha_short = env!("RETH_BSC_GIT_SHA");
        let git_sha_long = env!("RETH_BSC_GIT_SHA_LONG");

        let mut md = default_reth_version_metadata();
        // Capture the upstream Reth identity before we overwrite these fields, so
        // both the BSC and upstream builds can be reported in `reth --version`.
        let upstream_reth_version = md.cargo_pkg_version.clone();
        let upstream_reth_sha = md.vergen_git_sha_long.clone();
        let build_timestamp = md.vergen_build_timestamp.clone();
        let cargo_features = md.vergen_cargo_features.clone();
        let build_profile = md.build_profile_name.clone();

        md.name_client = Cow::Borrowed("Reth-BSC");
        // Expose the BSC release id as the primary version. This flows into the
        // `reth_info{version=...}` metric and the DB client version, so `0.1.0-fix`
        // is distinguishable from `0.1.0` without a commit-to-tag lookup.
        md.cargo_pkg_version = Cow::Borrowed(bsc_version);
        md.vergen_git_sha = Cow::Borrowed(git_sha_short);
        md.vergen_git_sha_long = Cow::Borrowed(git_sha_long);
        md.short_version = Cow::Owned(format!("{bsc_version} ({git_sha_short})"));
        // Report both the BSC release/commit and the upstream Reth version/commit
        // that this binary was built against.
        // Note: the CLI prepends the client name ("Reth-BSC") to the first line,
        // so line 0 is just "Version: ..." to render as "Reth-BSC Version: ...".
        md.long_version = Cow::Owned(format!(
            "Version: {bsc_version}\n\
             Commit SHA: {git_sha_long}\n\
             Upstream Reth Version: {upstream_reth_version}\n\
             Upstream Reth Commit SHA: {upstream_reth_sha}\n\
             Build Timestamp: {build_timestamp}\n\
             Build Features: {cargo_features}\n\
             Build Profile: {build_profile}",
        ));
        md.p2p_client_version = Cow::Owned(format!(
            "reth-bsc/v{bsc_version}-{git_sha_short}/{}",
            std::env::consts::OS,
        ));
        let _ = try_init_version_metadata(md);
    }

    reth_cli_util::sigsegv_handler::install();

    // Enable backtraces unless a RUST_BACKTRACE value has already been explicitly provided.
    if std::env::var_os("RUST_BACKTRACE").is_none() {
        std::env::set_var("RUST_BACKTRACE", "1");
    }

    // Initialize bid package queue at startup
    reth_bsc::shared::init_bid_package_queue();

    Cli::<BscChainSpecParser, BscCliArgs>::parse().run_with_components::<BscNode>(
        |spec| {
            (
                BscEvmConfig::new(spec.clone()),
                Arc::new(BscConsensus::new(spec))
                    as Arc<dyn FullConsensus<BscPrimitives>>,
            )
        },
        async move |mut builder, args| {
            // Set genesis hash override if provided
            if let Err(e) = genesis_override::set_genesis_hash_override(args.genesis_hash) {
                tracing::error!("Failed to set genesis hash override: {}", e);
                return Err(e);
            }

            if builder.config().rpc.ipcdisable {    
                panic!("IPC is disabled, please enable it by setting --ipc.enable to true");
            }
            let ipc_path = builder.config().rpc.ipcpath.clone();

            // Apply the BSC gas-price-oracle default unless the user set the flag.
            if !user_set_gpo_ignore_price(std::env::args()) {
                builder.config_mut().rpc.gas_price_oracle.ignore_price =
                    BSC_DEFAULT_GPO_IGNORE_PRICE;
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
                }

                if let Some(ref path) = args.keystore_path {
                    mining_config.keystore_path = Some(path.clone());
                }
                if let Some(ref pass) = args.keystore_password {
                    mining_config.keystore_password = Some(pass.clone());
                }

                // We'll derive and trust the validator address from the configured signing key when possible.
                // If not available, fall back to configured address (may be ZERO when disabled).
                mining_config.signing_key = if let Some(keystore_path) = &mining_config.keystore_path {
                    let password = mining_config.keystore_password.as_deref().unwrap_or("");
                    let signing_key = mining_config::keystore::load_private_key_from_keystore(keystore_path, password).
                        map_err(|e| eyre::eyre!("Failed to load private key from keystore: {}", e))?;
                    mining_config.validator_address = Some(mining_config::keystore::get_validator_address(&signing_key));
                    Some(signing_key)
                } else if let Some(hex_key) = &mining_config.private_key_hex {
                    let signing_key = mining_config::keystore::load_private_key_from_hex(hex_key).
                        map_err(|e| eyre::eyre!("Failed to load private key from hex: {}", e))?;
                    mining_config.validator_address = Some(mining_config::keystore::get_validator_address(&signing_key));
                    Some(signing_key)
                } else {
                    None
                };
                
                if let Some(gas_limit) = args.mining_gas_limit {
                    mining_config.gas_limit = Some(gas_limit);
                }

                if let Some(min_gas_tip) = args.mining_min_gas_tip {
                    mining_config.min_gas_tip = Some(min_gas_tip);
                }

                // CLI takes precedence over env BSC_MINING_USE_SPARSE_TRIE_STATE_ROOT.
                if args.mining_use_sparse_trie_state_root {
                    mining_config.use_sparse_trie_state_root = true;
                }

                // CLI takes precedence over env BSC_MINING_BID_BLOCK_ENABLED.
                if args.mining_bid_block_enabled {
                    mining_config.bid_block_enabled = true;
                }

                if let Some(port) = args.mining_mev_grpc_port {
                    mining_config.mev_grpc_port = port;
                }
                if let Some(concurrency) = args.mining_mev_grpc_concurrency {
                    mining_config.mev_grpc_concurrency = concurrency;
                }
                if let Some(timeout_ms) = args.mining_mev_grpc_request_timeout_ms {
                    mining_config.mev_grpc_request_timeout_ms = timeout_ms;
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
                let disable_tx_broadcast_forbidden = args.evn_disable_tx_broadcast_forbidden;
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
                    disable_tx_broadcast_forbidden,
                    whitelist_nodeids,
                    proxyed_validators: parsed_validators,
                    nodeids_to_add: parse_nodeids(args.evn_add_nodeids.clone()),
                    nodeids_to_remove: parse_nodeids(args.evn_remove_nodeids.clone()),
                };
                tracing::debug!(target: "bsc::init", "EVN is enabled: {}, config: {:?}", evn_enabled, cfg);
                let _ = reth_bsc::node::network::evn::set_global_evn_config(cfg);
                if evn_enabled { tracing::info!("EVN features enabled (disable peer tx broadcast)"); }
            }

            // Collect and parse proxyed peer IDs from CLI/env
            let proxyed_peer_ids = {
                let peers_from_env = std::env::var("BSC_PROXYED_PEER_IDS")
                    .ok()
                    .map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>())
                    .unwrap_or_default();
                let mut peer_ids_str = peers_from_env;
                peer_ids_str.extend(args.proxyed_peer_ids.iter().cloned());
                
                let mut parsed_peer_ids = Vec::new();
                for peer_id_str in peer_ids_str {
                    // Parse as 64-character hex PeerId (64 bytes total)
                    let peer_id_str = peer_id_str.trim();
                    if peer_id_str.is_empty() { continue; }
                    
                    let hex = peer_id_str.strip_prefix("0x").unwrap_or(peer_id_str);
                    match alloy_primitives::hex::decode(hex) {
                        Ok(bytes) if bytes.len() == 64 => {
                            let mut arr = [0u8; 64];
                            arr.copy_from_slice(&bytes);
                            let peer_id = reth_network_api::PeerId::from(alloy_primitives::FixedBytes::<64>::from(arr));
                            parsed_peer_ids.push(peer_id);
                        }
                        Ok(_) => tracing::warn!("Invalid proxyed peer ID length (need 64 bytes): {}", peer_id_str),
                        Err(e) => tracing::warn!("Failed to parse proxyed peer ID '{}': {}", peer_id_str, e),
                    }
                }
                
                if !parsed_peer_ids.is_empty() {
                    tracing::info!(target: "bsc::init", "Configured {} proxyed peer(s)", parsed_peer_ids.len());
                }
                parsed_peer_ids
            };

            // Store proxyed peer IDs for later use in network configuration
            if !proxyed_peer_ids.is_empty() {
                if let Err(e) = reth_bsc::shared::set_proxyed_peer_ids(proxyed_peer_ids) {
                    tracing::warn!(target: "bsc::init", "Failed to set proxyed peer IDs: {:?}", e);
                }
            }

            let mev_grpc_config =
                reth_bsc::node::miner::config::get_global_mining_config().and_then(|config| {
                    let port = config.get_mev_grpc_port();
                    (port != 0).then(|| {
                        (
                            port,
                            reth_bsc::grpc::mev::MevGrpcConfig::new(
                                config.get_mev_grpc_concurrency(),
                                std::time::Duration::from_millis(
                                    config.get_mev_grpc_request_timeout_ms(),
                                ),
                            ),
                        )
                    })
                });
            // Match go-bsc: the gRPC listener uses the configured HTTP bind host and an independent
            // port. The handle crosses the synchronous RPC-extension hook so it can be shut down
            // after the node exit future resolves.
            let mev_grpc_host = builder.config().rpc.http_addr;
            let mev_grpc_handle = Arc::new(std::sync::Mutex::new(None));
            let mev_grpc_handle_for_rpc = Arc::clone(&mev_grpc_handle);

            let (node, engine_handle_tx) = BscNode::new();
            let NodeHandle { node, node_exit_future: exit_future } =
                builder.node(node)
                    .extend_rpc_modules(move |ctx| {
                        // Every BSC namespace below is registered with `merge_if_module_configured`
                        // rather than `merge_configured`: the latter merges into every configured
                        // transport regardless of `--http.api`/`--ws.api`, so operator namespace
                        // selection was silently ignored for the BSC-specific APIs. That left
                        // `admin_setBidBlockPermission`, `miner_stop`, `miner_setGasLimit`,
                        // `miner_setEtherbase` and `mev_addBuilder`/`removeBuilder` callable
                        // unauthenticated on any node with HTTP enabled, even when the operator had
                        // excluded those namespaces — geth honours the equivalent `HTTPModules`
                        // setting, so this was also a parity gap.
                        use reth_rpc_server_types::RethRpcModule;

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
                        // `parlia` stays unconditional: reth's CLI rejects module names outside its
                        // known set ("Invalid RPC module 'parlia' in http.api"), so gating it on
                        // `RethRpcModule::Other("parlia")` would make the namespace unreachable —
                        // an operator has no way to ask for it back. That is acceptable here and
                        // only here, because every `parlia_*` method is read-only (snapshot,
                        // validator and justified/finalized queries, plus two calldata encoders);
                        // none mutates node state. The namespaces that do — admin, miner, mev —
                        // are gated below.
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
                        let mev_api = Arc::new(MevApiImpl::new(snapshot_provider, chain_spec));
                        ctx.modules.merge_if_module_configured(RethRpcModule::Mev, mev_api.as_ref().clone().into_rpc())?;
                        tracing::info!("Succeed to register MEV RPC API");

                        tracing::info!("Start to register Miner RPC API...");
                        use reth_bsc::rpc::miner::{BscMinerApiImpl, BscMinerApiServer};

                        // Remove reth's built-in MinerApi methods (registered on IPC by default)
                        // to avoid conflicts with our BscMinerApi which redefines them
                        ctx.modules.remove_method_from_configured("miner_setExtra");
                        ctx.modules.remove_method_from_configured("miner_setGasPrice");
                        ctx.modules.remove_method_from_configured("miner_setGasLimit");
                        let miner_api = BscMinerApiImpl::new();
                        ctx.modules.merge_if_module_configured(RethRpcModule::Miner, miner_api.into_rpc())?;
                        tracing::info!("Succeed to register Miner RPC API");

                        tracing::info!("Start to register BSC Eth extension API (eth_coinbase, eth_health)...");
                        use reth_bsc::rpc::eth_ext::{BscEthExtApiImpl, BscEthExtApiServer};

                        // Remove the default unimplemented eth_coinbase before registering our version
                        ctx.modules.remove_method_from_configured("eth_coinbase");
                        let eth_ext_api = BscEthExtApiImpl::new();
                        ctx.modules.merge_if_module_configured(RethRpcModule::Eth, eth_ext_api.into_rpc())?;
                        tracing::info!("Succeed to register BSC Eth extension API");

                        tracing::info!("Start to register BSC Admin RPC API (admin_setBidBlockPermission)...");
                        use reth_bsc::rpc::admin::{BscAdminApiImpl, BscAdminApiServer};

                        let admin_api = BscAdminApiImpl::new();
                        ctx.modules.merge_if_module_configured(RethRpcModule::Admin, admin_api.into_rpc())?;
                        tracing::info!("Succeed to register BSC Admin RPC API");

                        tracing::info!("Start to register Blob RPC API...");
                        use reth_bsc::rpc::blob::{BlobApiImpl, BlobApiServer};

                        // Get pool and provider from context
                        let pool = ctx.pool().clone();
                        let provider = ctx.provider().clone();

                        let blob_api = BlobApiImpl::new(pool, provider);
                        ctx.modules.merge_if_module_configured(RethRpcModule::Eth, blob_api.into_rpc())?;
                        tracing::info!("Succeed to register Blob RPC API");

                        // Debug-only builder extraction seam for BEP-675 e2e testing
                        // (bep675 testing plan, Tier 2). Off unless explicitly enabled.
                        if std::env::var("BSC_DEBUG_BUILDER").map(|v| v == "true").unwrap_or(false) {
                            tracing::info!("Start to register Debug Builder RPC API (debug_buildCandidateBlock)...");
                            use reth_bsc::rpc::debug_builder::{BscDebugBuilderApiServer, DebugBuilderApiImpl};
                            let chain_spec = std::sync::Arc::new(ctx.config().chain.clone().as_ref().clone());
                            let debug_builder_api = DebugBuilderApiImpl::new(ctx.provider().clone(), chain_spec);
                            ctx.modules.merge_if_module_configured(RethRpcModule::Debug, debug_builder_api.into_rpc())?;
                            tracing::info!("Succeed to register Debug Builder RPC API");
                        }

                        tracing::info!("Start to register eth_config (EIP-7910) API...");
                        use reth::api::FullNodeComponents;
                        use reth_bsc::rpc::{BscEthConfigApiServer, BscEthConfigHandler};

                        let eth_config = BscEthConfigHandler::new(
                            ctx.provider().clone(),
                            ctx.node().evm_config().clone(),
                        );
                        ctx.modules.merge_if_module_configured(RethRpcModule::Eth, eth_config.into_rpc())?;
                        tracing::info!("Succeed to register eth_config (EIP-7910) API");

                        // Bind only after all RPC module registration succeeded, so a later merge
                        // error cannot leave a detached gRPC listener behind during failed startup.
                        if let Some((port, grpc_config)) = mev_grpc_config {
                            let listen_addr = std::net::SocketAddr::new(mev_grpc_host, port);
                            let handle = reth_bsc::grpc::mev::start_mev_grpc_server(
                                listen_addr,
                                grpc_config,
                                mev_api,
                            )?;
                            *mev_grpc_handle_for_rpc.lock().unwrap() = Some(handle);
                        }
                        Ok(())
                    })
                    .launch().await?;

            // Send the engine handle to the network
            engine_handle_tx.send(node.beacon_engine_handle.clone()).unwrap();
            reth_bsc::shared::set_engine_api_tx(node.engine_api_tx.clone().unwrap()).unwrap();
            tracing::debug!("set engine api tx successfully");

            // Publish CanonicalInMemoryState so the sparse-trie spawner can resolve
            // in-memory parents back to the on-disk anchor. Concrete `BlockchainProvider`
            // is only reachable here (post-launch); the generic builder context isn't.
            let _ = reth_bsc::shared::set_canonical_in_memory_state(
                node.provider.canonical_in_memory_state(),
            );

            // Set the IPC client
            reth_bsc::shared::set_ipc_client(ipc_path).await.unwrap();

            let grpc_handle = mev_grpc_handle.lock().unwrap().take();
            let exit_result = exit_future.await;
            if let Some(handle) = grpc_handle {
                handle.shutdown().await;
            }
            exit_result
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::user_set_gpo_ignore_price;

    fn args(v: &[&str]) -> std::vec::IntoIter<String> {
        v.iter().map(|s| s.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn detects_explicit_gpo_ignore_price() {
        assert!(user_set_gpo_ignore_price(args(&["reth-bsc", "node", "--gpo.ignoreprice", "0"])));
        assert!(user_set_gpo_ignore_price(args(&["reth-bsc", "node", "--gpo.ignoreprice=7"])));
        assert!(!user_set_gpo_ignore_price(args(&["reth-bsc", "node", "--chain", "bsc"])));
        assert!(!user_set_gpo_ignore_price(args(&["reth-bsc", "node", "--gpo.blocks", "20"])));
    }
}
