//! BSC-specific implementation of EIP-7910 `eth_config` RPC endpoint.
//!
//! This overrides the upstream `EthConfigHandler` to:
//! - Return BSC-compatible precompile names (geth's `Name()` convention)
//! - Return standard Ethereum system contracts only (not BSC system contracts)
//! - Return null for `blobSchedule` (BSC doesn't support EIP-4844 blobs)
//!
//! Ref: <https://eips.ethereum.org/EIPS/eip-7910>

use crate::hardforks::BscHardforks;
use alloy_consensus::BlockHeader;
use alloy_eips::eip2935::HISTORY_STORAGE_ADDRESS;
use alloy_eips::eip7840::BlobParams;
use alloy_evm::precompiles::Precompile;
use alloy_primitives::{Address, Bytes};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use jsonrpsee::types::{error::INTERNAL_ERROR_CODE, ErrorObject};
use reth_chainspec::{ChainSpecProvider, EthChainSpec, EthereumHardforks, Hardforks, Head};
use reth_evm::{precompiles::PrecompilesMap, ConfigureEvm, Evm};
use reth_primitives::NodePrimitives;
use reth_primitives_traits::header::HeaderMut;
use reth_provider::BlockReaderIdExt;
use reth_revm::db::EmptyDB;
use serde::Serialize;
use std::collections::BTreeMap;

// ---- BSC-specific RPC trait (replaces upstream EthConfigApiServer) ----

/// BSC-specific `eth_config` RPC trait.
///
/// Replaces the upstream `EthConfigApiServer` to support:
/// - Nullable `blobSchedule` (BSC doesn't use EIP-4844 blobs)
/// - BSC-compatible precompile naming
/// - Standard Ethereum system contracts only (not BSC system contracts)
#[rpc(server, namespace = "eth")]
pub trait BscEthConfigApi {
    /// Returns the chain configuration for the current, next, and last forks.
    #[method(name = "config")]
    fn config(&self) -> RpcResult<BscEthConfig>;
}

// ---- Response types ----

/// BSC eth_config response with current, next, and last fork configurations.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BscEthConfig {
    pub current: BscEthForkConfig,
    pub next: Option<BscEthForkConfig>,
    pub last: Option<BscEthForkConfig>,
}

/// BSC fork configuration.
///
/// Unlike the upstream `EthForkConfig`, this uses `Option<BlobParams>` for
/// `blob_schedule` so it can serialize as `null` for BSC (which doesn't
/// support EIP-4844 blob transactions).
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BscEthForkConfig {
    pub activation_time: u64,
    /// Nullable blob schedule. BSC returns `null` since it doesn't use EIP-4844 blobs.
    pub blob_schedule: Option<BlobParams>,
    /// Chain ID serialized as hex string (e.g., `"0x61"`) to match geth format.
    #[serde(serialize_with = "serialize_chain_id_hex")]
    pub chain_id: u64,
    /// Fork identifier hash.
    pub fork_id: Bytes,
    /// Active precompiles: name → address.
    pub precompiles: BTreeMap<String, Address>,
    /// Active system contracts: name → address.
    pub system_contracts: BTreeMap<String, Address>,
}

/// Serialize chain_id as hex string with `0x` prefix to match geth format.
fn serialize_chain_id_hex<S: serde::Serializer>(id: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format!("0x{:x}", id))
}

// ---- Handler ----

/// BSC-specific handler for the `eth_config` RPC endpoint.
///
/// Extends the upstream EIP-7910 implementation with:
/// - BSC-compatible precompile names (HEADER_VALIDATE, BLS_SIGNATURE_VERIFY, etc.)
/// - Standard Ethereum system contracts only (HISTORY_STORAGE_ADDRESS at Prague)
/// - Null blob schedule (BSC doesn't modify blob params)
/// - BSC hardfork-aware fork configuration
#[derive(Debug, Clone)]
pub struct BscEthConfigHandler<Provider, Evm> {
    provider: Provider,
    evm_config: Evm,
}

impl<Provider, EvmConfig> BscEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig:
        ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    /// Creates a new [`BscEthConfigHandler`].
    pub const fn new(provider: Provider, evm_config: EvmConfig) -> Self {
        Self { provider, evm_config }
    }

    /// Returns standard Ethereum system contracts active at the given timestamp.
    ///
    /// Matches geth-bsc behavior: only standard Ethereum system contracts are returned.
    /// BSC-specific system contracts (ValidatorSet, Slash, StakeHub, etc.) are NOT
    /// included, matching geth's `ActiveSystemContracts(timestamp)` output.
    fn system_contracts_at(&self, timestamp: u64) -> BTreeMap<String, Address> {
        let chain_spec = self.provider.chain_spec();
        let mut contracts = BTreeMap::new();

        // EIP-2935: HISTORY_STORAGE_ADDRESS - activated at Prague equivalent.
        // BSC doesn't have beacon roots, deposit contract, withdrawal request, or
        // consolidation request contracts since it uses Parlia consensus, not PoS.
        if chain_spec.is_prague_active_at_timestamp(timestamp) {
            contracts.insert(
                "HISTORY_STORAGE_ADDRESS".to_string(),
                HISTORY_STORAGE_ADDRESS,
            );
        }

        contracts
    }

    /// Builds fork config for a specific timestamp, including BSC-named precompiles.
    fn build_fork_config_at(
        &self,
        timestamp: u64,
        precompiles: BTreeMap<String, Address>,
    ) -> BscEthForkConfig {
        let chain_spec = self.provider.chain_spec();

        let system_contracts = self.system_contracts_at(timestamp);

        // Fork config only exists for timestamp-based hardforks.
        let fork_id = chain_spec
            .fork_id(&Head { timestamp, number: u64::MAX, ..Default::default() })
            .hash
            .0
            .into();

        // BSC doesn't support EIP-4844 blobs, so blobSchedule is always null.
        // This matches geth-bsc behavior where ActiveBlobSchedule returns nil for BSC.
        let blob_schedule = None;

        BscEthForkConfig {
            activation_time: timestamp,
            blob_schedule,
            chain_id: chain_spec.chain().id(),
            fork_id,
            precompiles,
            system_contracts,
        }
    }

    /// Main config method - builds current, next, and last fork configurations.
    fn config_impl(&self) -> RpcResult<BscEthConfig> {
        let chain_spec = self.provider.chain_spec();
        let latest = self
            .provider
            .latest_header()
            .map_err(|e| internal_err(e.to_string()))?
            .ok_or_else(|| internal_err("best block not found"))?
            .into_header();

        let current_precompiles = bsc_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &latest)
                .map_err(|e| internal_err(e.to_string()))?,
        );

        let mut fork_timestamps = chain_spec
            .forks_iter()
            .filter_map(|(_, cond)| cond.as_timestamp())
            .collect::<Vec<_>>();
        fork_timestamps.sort_unstable();
        fork_timestamps.dedup();

        let current_fork_idx =
            match fork_timestamps.iter().position(|ts| &latest.timestamp() < ts) {
                // All forks are in the past, use the last one.
                None => fork_timestamps.len().checked_sub(1),
                // First fork hasn't activated yet — no active timestamp fork.
                Some(0) => None,
                // Found a future fork; current is the one right before it.
                Some(idx) => Some(idx - 1),
            };
        let (current_fork_idx, current_fork_timestamp) = current_fork_idx
            .and_then(|idx| fork_timestamps.get(idx).map(|ts| (idx, *ts)))
            .ok_or_else(|| internal_err("no active timestamp fork found"))?;

        let current = self.build_fork_config_at(current_fork_timestamp, current_precompiles);

        let mut config = BscEthConfig { current, next: None, last: None };

        if let Some(next_fork_timestamp) = fork_timestamps.get(current_fork_idx + 1).copied() {
            let fake_header = {
                let mut header = latest.clone();
                header.set_timestamp(next_fork_timestamp);
                header
            };
            let next_precompiles = bsc_precompiles_map(
                self.evm_config
                    .evm_for_block(EmptyDB::default(), &fake_header)
                    .map_err(|e| internal_err(e.to_string()))?,
            );

            config.next =
                Some(self.build_fork_config_at(next_fork_timestamp, next_precompiles));
        } else {
            // If there is no fork scheduled, there is no "last" or "final" fork scheduled.
            return Ok(config);
        }

        let last_fork_timestamp = fork_timestamps.last().copied().unwrap();
        let fake_header = {
            let mut header = latest;
            header.set_timestamp(last_fork_timestamp);
            header
        };
        let last_precompiles = bsc_precompiles_map(
            self.evm_config
                .evm_for_block(EmptyDB::default(), &fake_header)
                .map_err(|e| internal_err(e.to_string()))?,
        );

        config.last =
            Some(self.build_fork_config_at(last_fork_timestamp, last_precompiles));

        Ok(config)
    }
}

impl<Provider, EvmConfig> BscEthConfigApiServer for BscEthConfigHandler<Provider, EvmConfig>
where
    Provider: ChainSpecProvider<ChainSpec: Hardforks + EthereumHardforks + BscHardforks>
        + BlockReaderIdExt<Header: HeaderMut>
        + 'static,
    EvmConfig:
        ConfigureEvm<Primitives: NodePrimitives<BlockHeader = Provider::Header>> + 'static,
{
    fn config(&self) -> RpcResult<BscEthConfig> {
        self.config_impl()
    }
}

// ---- Helper functions ----

/// Helper to create a jsonrpsee internal error.
fn internal_err(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
    ErrorObject::owned(INTERNAL_ERROR_CODE, msg.into(), None::<()>)
}

/// Maps a precompile address to its geth-compatible name.
///
/// Names match geth-bsc's precompile `Name()` methods exactly:
/// - Standard Ethereum precompiles: ECREC, SHA256, RIPEMD160, ID, MODEXP, etc.
/// - BSC custom precompiles: HEADER_VALIDATE, BLS_SIGNATURE_VERIFY, etc.
/// - EIP-2537 BLS12-381 precompiles: BLS12_G1ADD, BLS12_G1MSM, etc.
///
/// Returns None for unknown addresses, in which case the caller should
/// fall back to `precompile_id().name()`.
fn address_to_precompile_name(address: &Address) -> Option<&'static str> {
    let bytes = address.as_slice();
    // Precompile addresses have zeros in the first 12 bytes
    if bytes[..12].iter().any(|&b| b != 0) {
        return None;
    }
    let n = u64::from_be_bytes(bytes[12..20].try_into().ok()?);

    match n {
        // Standard Ethereum precompiles (Istanbul)
        1 => Some("ECREC"),
        2 => Some("SHA256"),
        3 => Some("RIPEMD160"),
        4 => Some("ID"),
        5 => Some("MODEXP"),
        6 => Some("BN254_ADD"),
        7 => Some("BN254_MUL"),
        8 => Some("BN254_PAIRING"),
        9 => Some("BLAKE2F"),
        // EIP-4844: KZG point evaluation (Cancun)
        0x0a => Some("KZG_POINT_EVALUATION"),
        // EIP-2537: BLS12-381 precompiles (Pascal/Prague)
        0x0b => Some("BLS12_G1ADD"),
        0x0c => Some("BLS12_G1MSM"),
        0x0d => Some("BLS12_G2ADD"),
        0x0e => Some("BLS12_G2MSM"),
        0x0f => Some("BLS12_PAIRING_CHECK"),
        0x10 => Some("BLS12_MAP_FP_TO_G1"),
        0x11 => Some("BLS12_MAP_FP2_TO_G2"),
        // BSC custom precompiles
        // Names match geth-bsc's Name() methods for the latest active fork versions.
        // At current BSC forks (post-Hertz, post-Plato), these are the correct names.
        100 => Some("HEADER_VALIDATE"),                          // tendermint header validation
        101 => Some("IAVL_MERKLE_PROOF_VALIDATE_PLATO"),         // IAVL proof (Plato+ version)
        102 => Some("BLS_SIGNATURE_VERIFY"),                     // BLS signature verify
        103 => Some("COMET_BFT_LIGHT_BLOCK_VALIDATE_HERTZ"),     // CometBFT (Hertz+ version)
        104 => Some("VERIFY_DOUBLE_SIGN_EVIDENCE"),              // double sign evidence
        105 => Some("SECP256K1_SIGNATURE_RECOVER"),              // secp256k1 signature recover
        // EIP-7212: P256VERIFY (secp256r1, Haber+)
        0x100 => Some("P256VERIFY"),
        _ => None,
    }
}

/// Extracts precompile name→address map from the EVM, using BSC-compatible names.
///
/// Uses [`address_to_precompile_name`] for known precompile addresses (matching geth's
/// `Name()` convention), and falls back to `precompile_id().name()` for any unknown
/// addresses.
///
/// This fixes the issue where all BSC custom precompiles use `PrecompileId::Identity`,
/// which would cause name collisions (all mapping to "Identity") and override the
/// standard Identity precompile at address 0x04.
fn bsc_precompiles_map(
    evm: impl Evm<Precompiles = PrecompilesMap>,
) -> BTreeMap<String, Address> {
    let precompiles = evm.precompiles();
    precompiles
        .addresses()
        .filter_map(|address| {
            let name = if let Some(n) = address_to_precompile_name(address) {
                n.to_string()
            } else {
                // Fallback for unknown precompiles
                precompiles.get(address)?.precompile_id().name().to_string()
            };
            Some((name, *address))
        })
        .collect()
}
