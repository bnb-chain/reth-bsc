use super::{bsc::bsc_mainnet, bsc_chapel::bsc_testnet, bsc_rialto::bsc_qanet, local::bsc_local, BscChainSpec};
use reth_cli::chainspec::ChainSpecParser;
use std::{sync::Arc, path::Path};
use alloy_genesis::Genesis;
use reth_chainspec::{ChainSpec, ForkCondition};
use reth_ethereum_forks::EthereumHardfork;
use crate::hardforks::bsc::BscHardfork;
use serde_json::Value;

/// Bsc chain specification parser.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BscChainSpecParser;

impl ChainSpecParser for BscChainSpecParser {
    type ChainSpec = BscChainSpec;

    const SUPPORTED_CHAINS: &'static [&'static str] = &["bsc", "bsc-testnet", "bsc-qanet", "local"];

    fn parse(s: &str) -> eyre::Result<Arc<Self::ChainSpec>> {
        chain_value_parser(s)
    }
}

/// Clap value parser for [`BscChainSpec`]s.
///
/// The value parser matches either a known chain, the path
/// to a json file, or a json formatted string in-memory. The json needs to be a Genesis struct.
pub fn chain_value_parser(s: &str) -> eyre::Result<Arc<BscChainSpec>> {
    match s {
        "bsc" => Ok(Arc::new(BscChainSpec { inner: bsc_mainnet() })),
        "bsc-testnet" => Ok(Arc::new(BscChainSpec { inner: bsc_testnet() })),
        "bsc-qanet" => Ok(Arc::new(BscChainSpec { inner: bsc_qanet() })),
        "local" => Ok(Arc::new(BscChainSpec { inner: bsc_local() })),
        _ => {
            // Try to parse as file path or JSON string
            if Path::new(s).exists() {
                parse_genesis_file(s)
            } else if s.starts_with('{') {
                parse_genesis_json(s)
            } else {
                Err(eyre::eyre!("Unsupported chain or invalid genesis file: {}", s))
            }
        }
    }
}

/// Parse a genesis.json file and create a BscChainSpec with hard fork information
pub fn parse_genesis_file(path: &str) -> eyre::Result<Arc<BscChainSpec>> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| eyre::eyre!("Failed to read genesis file {}: {}", path, e))?;
    parse_genesis_json(&content)
}

/// Parse genesis JSON string and extract hard fork configuration
pub fn parse_genesis_json(json_str: &str) -> eyre::Result<Arc<BscChainSpec>> {
    let genesis: Genesis = serde_json::from_str(json_str)
        .map_err(|e| eyre::eyre!("Failed to parse genesis JSON: {}", e))?;
    
    let value: Value = serde_json::from_str(json_str)
        .map_err(|e| eyre::eyre!("Failed to parse genesis JSON as Value: {}", e))?;
    
    let mut chain_spec = ChainSpec::builder()
        .chain(genesis.config.chain_id.into())
        .genesis(genesis)
        .with_fork(EthereumHardfork::Frontier, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Homestead, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Tangerine, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::SpuriousDragon, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Byzantium, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Constantinople, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Petersburg, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::Istanbul, ForkCondition::Block(0))
        .with_fork(EthereumHardfork::MuirGlacier, ForkCondition::Block(0));
    
    // Extract and add hard fork configuration from the genesis config
    if let Some(config) = value.get("config") {
        chain_spec = add_hardforks_to_chainspec(chain_spec, config)?;
    } else {
        // Add local development hardforks - just add Bohr at block 0 for development
        chain_spec = chain_spec.with_fork(BscHardfork::Bohr, ForkCondition::Block(0));
    }
    
    let chain_spec = chain_spec.build();
    Ok(Arc::new(BscChainSpec { inner: chain_spec }))
}


/// Add hard forks from genesis config to chain spec builder
fn add_hardforks_to_chainspec(
    mut chain_spec: reth_chainspec::ChainSpecBuilder, 
    config: &Value
) -> eyre::Result<reth_chainspec::ChainSpecBuilder> {
    // Handle BSC-specific hard forks with timestamps
    if let Some(ramanujan_block) = config.get("ramanujanBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Ramanujan, ForkCondition::Block(ramanujan_block));
    }
    
    if let Some(niels_block) = config.get("nielsBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Niels, ForkCondition::Block(niels_block));
    }

    if let Some(mirror_sync_block) = config.get("mirrorSyncBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::MirrorSync, ForkCondition::Block(mirror_sync_block));
    }

    if let Some(bruno_block) = config.get("brunoBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Bruno, ForkCondition::Block(bruno_block));
    }

    if let Some(euler_block) = config.get("eulerBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Euler, ForkCondition::Block(euler_block));
    }

    if let Some(nano_block) = config.get("nanoBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Nano, ForkCondition::Block(nano_block));
    }

    if let Some(moran_block) = config.get("moranBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Moran, ForkCondition::Block(moran_block));
    }

    if let Some(gibbs_block) = config.get("gibbsBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Gibbs, ForkCondition::Block(gibbs_block));
    }

    if let Some(planck_block) = config.get("planckBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Planck, ForkCondition::Block(planck_block));
    }

    if let Some(luban_block) = config.get("lubanBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Luban, ForkCondition::Block(luban_block));
    }

    if let Some(plato_block) = config.get("platoBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Plato, ForkCondition::Block(plato_block));
    }

    if let Some(hertz_block) = config.get("hertzBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Hertz, ForkCondition::Block(hertz_block));
    }

    if let Some(hertz_fix_block) = config.get("hertzfixBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::HertzFix, ForkCondition::Block(hertz_fix_block));
    }

    // Handle block-based forks from geth genesis format
    if let Some(berlin_block) = config.get("berlinBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(EthereumHardfork::Berlin, ForkCondition::Block(berlin_block));
    }
    
    if let Some(london_block) = config.get("londonBlock").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(EthereumHardfork::London, ForkCondition::Block(london_block));
    }
    
    // Handle timestamp-based forks
    if let Some(shanghai_time) = config.get("shanghaiTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(EthereumHardfork::Shanghai, ForkCondition::Timestamp(shanghai_time));
    }
    
    if let Some(kepler_time) = config.get("keplerTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Kepler, ForkCondition::Timestamp(kepler_time));
    }
    
    if let Some(feynman_time) = config.get("feynmanTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Feynman, ForkCondition::Timestamp(feynman_time));
    }
    
    if let Some(feynman_fix_time) = config.get("feynmanFixTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::FeynmanFix, ForkCondition::Timestamp(feynman_fix_time));
    }
    
    if let Some(cancun_time) = config.get("cancunTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(EthereumHardfork::Cancun, ForkCondition::Timestamp(cancun_time));
        chain_spec = chain_spec.with_fork(BscHardfork::Cancun, ForkCondition::Timestamp(cancun_time));
    }
    
    if let Some(haber_time) = config.get("haberTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Haber, ForkCondition::Timestamp(haber_time));
    }
    
    if let Some(haber_fix_time) = config.get("haberFixTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::HaberFix, ForkCondition::Timestamp(haber_fix_time));
    }
    
    if let Some(bohr_time) = config.get("bohrTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Bohr, ForkCondition::Timestamp(bohr_time));
    }

    if let Some(tycho_time) = config.get("tychoTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Tycho, ForkCondition::Timestamp(tycho_time));
    }
    
    if let Some(prague_time) = config.get("pragueTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(EthereumHardfork::Prague, ForkCondition::Timestamp(prague_time));
    }
    
    if let Some(pascal_time) = config.get("pascalTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Pascal, ForkCondition::Timestamp(pascal_time));
    }
    
    if let Some(lorentz_time) = config.get("lorentzTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Lorentz, ForkCondition::Timestamp(lorentz_time));
    }
    
    if let Some(maxwell_time) = config.get("maxwellTime").and_then(|v| v.as_u64()) {
        chain_spec = chain_spec.with_fork(BscHardfork::Maxwell, ForkCondition::Timestamp(maxwell_time));
    }
    
    Ok(chain_spec)
}