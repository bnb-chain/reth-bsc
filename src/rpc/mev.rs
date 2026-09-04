use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::SnapshotProvider;
use crate::hardforks::BscHardforks;
use crate::node::miner::bid_block::BidBlockArgs;
use crate::node::miner::bid_simulator::Bid;
use crate::node::miner::config::keystore;
use crate::node::miner::config::MiningConfig;
use alloy_consensus::BlobTransactionSidecar;
use alloy_consensus::Transaction;
use alloy_consensus::{transaction::RlpEcdsaDecodableTx, TxEip4844WithSidecar};
use alloy_primitives::Address;
use alloy_primitives::{Bytes, B256, U256, U64};
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use reth_chainspec::EthChainSpec;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

/// Per-block pending BidBlock tracking: block_number → builder → set of bid hashes.
type PendingBidBlocks = Arc<RwLock<HashMap<u64, HashMap<Address, HashSet<B256>>>>>;
use tracing::debug;

/// Raw bid data structure from builder
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawBid {
    /// Block number for this bid
    pub block_number: U64,
    /// Parent block hash
    pub parent_hash: B256,
    /// List of transactions in the bid (may include blob tx with sidecars)
    pub txs: Vec<Bytes>,
    /// List of transaction hashes that cannot be reverted
    #[serde(default)]
    pub un_revertible: Vec<B256>,
    /// Total gas used
    pub gas_used: U64,
    /// Gas fee
    pub gas_fee: U256,
    /// Builder fee (optional, None means not provided)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder_fee: Option<U256>,
}

/// Decoded transaction with optional sidecar
struct DecodedTransaction {
    tx: TransactionSigned,
    sidecar: Option<BlobTransactionSidecar>,
}

/// Builder bid arguments for mev_sendBid
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BidArgs {
    /// Raw bid from builder
    #[serde(alias = "RawBid")]
    pub raw_bid: RawBid,
    /// Signature of the bid from builder
    pub signature: Bytes,
    /// Optional payment transaction to builder from sentry
    #[serde(default)]
    pub pay_bid_tx: Bytes,
    /// Gas used by the payment transaction
    #[serde(default)]
    pub pay_bid_tx_gas_used: U64,
}

/// MEV parameters returned by mev_params
/// Matches geth-bsc implementation
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct MevParams {
    /// Validator commission rate (in basis points, e.g. 100 = 1%)
    #[serde(rename = "ValidatorCommission")]
    pub validator_commission: u64,
    /// Time left for bid simulation in nanoseconds
    #[serde(rename = "BidSimulationLeftOver")]
    pub bid_simulation_left_over: u64,
    /// Time left when bid cannot be interrupted in nanoseconds
    #[serde(rename = "NoInterruptLeftOver")]
    pub no_interrupt_left_over: u64,
    /// Time reserved to finalize a block, in nanoseconds; the BidBlock receive deadline is
    /// `BidMustBefore = header.Time - DelayLeftOver`. Exposed so builders can compute
    /// `BidMustBefore` per validator instead of hardcoding the 15ms default.
    #[serde(rename = "DelayLeftOver")]
    pub delay_left_over: u64,
    /// Maximum number of bids allowed per builder per block
    #[serde(rename = "MaxBidsPerBuilder")]
    pub max_bids_per_builder: u32,
    /// Gas ceiling for blocks (maximum gas limit) - decimal number
    #[serde(rename = "GasCeil")]
    pub gas_ceil: u64,
    /// Minimum average gas price for bid block - decimal number
    #[serde(rename = "GasPrice", serialize_with = "serialize_u256_as_decimal")]
    pub gas_price: U256,
    /// Maximum builder fee allowed - decimal number
    #[serde(rename = "BuilderFeeCeil", serialize_with = "serialize_u256_as_decimal")]
    pub builder_fee_ceil: U256,
    /// Whether the `mev_sendBidBlock` (BEP-675) path is accepted
    #[serde(rename = "BidBlockEnabled")]
    pub bid_block_enabled: bool,
    /// MEV service version
    #[serde(rename = "Version")]
    pub version: String,
}

/// Serialize U256 as decimal number (not hex string)
fn serialize_u256_as_decimal<S>(value: &U256, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    // Convert U256 to decimal string, then parse as u128 if possible
    let decimal_str = value.to_string();

    // Try to serialize as number if it fits in u64 (safe for JSON)
    if let Ok(num) = decimal_str.parse::<u64>() {
        serializer.serialize_u64(num)
    } else if let Ok(num) = decimal_str.parse::<u128>() {
        // For larger numbers, serialize as u128
        serializer.serialize_u128(num)
    } else {
        // For very large numbers, serialize as string
        serializer.serialize_str(&decimal_str)
    }
}

/// JSON wire shape of go-bsc's `BidBlockPermissionResult` (`internal/ethapi/api_mev.go`): the
/// detail fields are omitted entirely when `allowed` is true, matching Go's `omitempty` tags.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BidBlockPermissionResult {
    pub allowed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_hash: Option<B256>,
    /// Hex-quantity block number (e.g. `"0x64"`), matching go-bsc's `hexutil.Uint64`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block_number: Option<String>,
    /// RFC 3339 UTC timestamp, matching Go's default `time.Time` JSON marshaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    /// RFC 3339 UTC timestamp, matching Go's default `time.Time` JSON marshaling.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_at: Option<String>,
}

impl From<crate::node::miner::bid_block_permission::BidBlockPermissionStatus>
    for BidBlockPermissionResult
{
    fn from(status: crate::node::miner::bid_block_permission::BidBlockPermissionStatus) -> Self {
        if status.allowed {
            return Self {
                allowed: true,
                reason: None,
                block_hash: None,
                block_number: None,
                revoked_at: None,
                reset_at: None,
            };
        }
        Self {
            allowed: false,
            reason: Some(status.reason),
            block_hash: Some(status.block_hash),
            block_number: Some(format!("0x{:x}", status.block_num)),
            revoked_at: Some(unix_secs_to_rfc3339_utc(status.revoked_at)),
            reset_at: Some(unix_secs_to_rfc3339_utc(status.reset_at)),
        }
    }
}

/// Formats a Unix timestamp (UTC, whole seconds) as an RFC 3339 string (`"2024-01-15T10:30:04Z"`),
/// matching the shape Go's `time.Time.MarshalJSON` produces for `revokedAt`/`resetAt`. Avoids a
/// date/time dependency for what is otherwise the only place this repo needs one.
fn unix_secs_to_rfc3339_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let secs_of_day = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let (hour, min, sec) = (secs_of_day / 3600, (secs_of_day % 3600) / 60, secs_of_day % 60);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z")
}

/// Converts a day count since the Unix epoch (1970-01-01) into a (year, month, day) civil date.
/// Howard Hinnant's `civil_from_days` algorithm: <http://howardhinnant.github.io/date_algorithms.html#civil_from_days>.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month, day)
}

/// Custom MEV API server trait - only includes send_bid to avoid conflicts with reth's default MEV API
#[rpc(server, namespace = "mev")]
pub trait BscMevApi {
    /// Send a bid to the builder
    #[method(name = "sendBid")]
    async fn send_bid(&self, bid: BidArgs) -> RpcResult<B256>;

    /// Submit a builder-proposed block (BEP-675). Returns the bid hash on admission.
    #[method(name = "sendBidBlock")]
    async fn send_bid_block(&self, args: BidBlockArgs) -> RpcResult<B256>;

    /// Get MEV parameters
    #[method(name = "params")]
    async fn params(&self) -> RpcResult<MevParams>;

    /// Check if MEV is running
    #[method(name = "running")]
    async fn running(&self) -> RpcResult<bool>;

    /// Check if a builder is registered
    #[method(name = "hasBuilder")]
    async fn has_builder(&self, builder: Address) -> RpcResult<bool>;

    /// Add a builder to the whitelist
    #[method(name = "addBuilder")]
    async fn add_builder(&self, builder: Address) -> RpcResult<bool>;

    /// Remove a builder from the whitelist
    #[method(name = "removeBuilder")]
    async fn remove_builder(&self, builder: Address) -> RpcResult<bool>;

    /// Query a builder's current BEP-675 `SendBidBlock` permission (go-bsc
    /// `MevAPI.GetBidBlockPermission`): whether it's allowed, and if not, why and when it resets.
    #[method(name = "getBidBlockPermission")]
    async fn get_bid_block_permission(&self, builder: Address) -> RpcResult<BidBlockPermissionResult>;
}

const PAY_BID_TX_GAS_LIMIT: u64 = 25000;

// JSON-RPC error codes for bid rejections, matching go-bsc `core/types/bid_error.go` so builder
// clients see the same codes from reth-bsc and geth.
const INVALID_BID_PARAM_ERROR: i32 = -38001;
const MEV_NOT_RUNNING_ERROR: i32 = -38003;
const MEV_NOT_IN_TURN_ERROR: i32 = -38005;
const BID_BLOCK_PERMISSION_REVOKED_ERROR: i32 = -38006;
const BID_BLOCK_PRE_SEAL_VERIFY_ERROR: i32 = -38007;
const BID_BLOCK_TOO_LATE_ERROR: i32 = -38008;

/// Reproduces go-bsc `Miner.bidBlockEnabled()`: a BidBlock is only accepted when MEV is running,
/// the `BidBlockEnabled` flag is set, and the Pasteur fork is active at the chain head.
fn bid_block_admission_enabled(mev_running: bool, flag_enabled: bool, pasteur_active: bool) -> bool {
    mev_running && flag_enabled && pasteur_active
}

/// Why `mev_sendBidBlock`'s structural validation rejected a submission. Ordered as the checks run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BidBlockStructuralRejection {
    /// Bid targets a height at or below the head — it can never become the next block.
    StaleNumber,
    /// Bid targets a height beyond `head + 1`.
    FutureNumber,
    /// This validator does not propose the block after the head.
    NotInTurn,
    /// Bid's `parent_hash` is not the current head's hash.
    NonAlignedParent,
    /// Header claims no gas was used.
    EmptyGasUsed,
    /// Bid carries no transactions.
    EmptyTransactions,
}

/// Structural validation for `mev_sendBidBlock`, mirroring go-bsc `MevAPI.SendBidBlock`.
///
/// Split out as a free function so the checks — and critically their **order** — are testable
/// without standing up a full `MevApiImpl` with a snapshot provider and chain head. The order is
/// load-bearing and matches go-bsc: number, then in-turn, then parent alignment. `node-deploy-bsc`'s
/// `probeInTurn` depends on it, submitting a parent-hash-mismatched bid to learn whether the
/// validator is in turn — the number check must pass and in-turn must run *before* parent alignment
/// fails, or the probe reads the wrong answer.
fn validate_bid_block_structure(
    block_number: u64,
    head_number: u64,
    is_inturn: bool,
    bid_parent_hash: B256,
    head_hash: B256,
    gas_used: u64,
    transaction_count: usize,
) -> Result<(), BidBlockStructuralRejection> {
    use BidBlockStructuralRejection::*;
    if block_number < head_number + 1 {
        return Err(StaleNumber);
    }
    if block_number > head_number + 1 {
        return Err(FutureNumber);
    }
    if !is_inturn {
        return Err(NotInTurn);
    }
    if bid_parent_hash != head_hash {
        return Err(NonAlignedParent);
    }
    if gas_used == 0 {
        return Err(EmptyGasUsed);
    }
    if transaction_count == 0 {
        return Err(EmptyTransactions);
    }
    Ok(())
}

/// Mirrors go-bsc `bidutil.BidMustBefore`: the deadline after which a `mev_sendBidBlock`
/// submission is rejected as too late.
///
/// `bid_must_before_ms = parent.MilliTimestamp() + block_interval_ms - delay_left_over_ms`
///
/// `parent_timestamp_ms` must be the parent header's *full* millisecond timestamp (seconds plus
/// the sub-second component carried in `mixHash` post-Lorentz — see
/// [`crate::consensus::parlia::util::calculate_millisecond_timestamp`]), not just
/// `header.timestamp * 1000`: on sub-second block intervals (Fermi/Maxwell), truncating the
/// parent's millisecond part shifts the deadline by up to a full block interval. The subtracted
/// knob is `delay_left_over_ms` (go-bsc's `Config.DelayLeftOver`, default 15ms) —
/// `no_interrupt_left_over` bounds bid *simulation* time and is a different, much larger, knob.
fn bid_must_before_ms(parent_timestamp_ms: u64, block_interval_ms: u64, delay_left_over_ms: u64) -> u128 {
    (parent_timestamp_ms as u128 + block_interval_ms as u128)
        .saturating_sub(delay_left_over_ms as u128)
}

/// Implementation of the MEV Builder RPC API.
///
/// Clones share the whitelist and pending-BidBlock accounting. This lets JSON-RPC and gRPC expose
/// the same admission object without creating separate duplicate/quota state.
#[derive(Clone)]
pub struct MevApiImpl {
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    chain_spec: Arc<BscChainSpec>,
    validator_address: Address,
    validator_commission: u64,
    bid_simulation_left_over: u64, // milliseconds
    no_interrupt_left_over: u64,   // milliseconds
    /// go-bsc's `Config.DelayLeftOver`: time reserved to finalize a block, subtracted from the
    /// `mev_sendBidBlock` admission deadline (`bidMustBefore`). Distinct from
    /// `no_interrupt_left_over`, which only bounds bid *simulation*.
    delay_left_over: u64, // milliseconds
    max_bids_per_builder: u32,
    gas_ceil: u64,
    min_gas_price: U256,
    builder_fee_ceil: U256,
    bid_block_enabled: bool,
    version: String,
    /// Whitelist of allowed builders (shared with miner_ namespace via shared.rs)
    allowed_builders: Arc<RwLock<HashSet<Address>>>,
    /// Mirrors go-bsc `bidSimulator.pending`: blockNumber → builder → set of bid hashes.
    /// Used to enforce duplicate detection and the per-builder-per-block quota
    /// (`max_bids_per_builder`) at RPC admission time, before the bid enters the miner queue.
    pending_bid_blocks: PendingBidBlocks,
}

// NOTE: The allowed_builders is now also accessible via crate::shared::get_builder_whitelist()
// so that the miner_ RPC namespace can manage builders too.

impl MevApiImpl {
    /// Create a new MEV API instance
    pub fn new(
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        chain_spec: Arc<BscChainSpec>,
    ) -> Self {
        let mining_config =
            if let Some(cfg) = crate::node::miner::config::get_global_mining_config() {
                cfg.clone()
            } else {
                MiningConfig::from_env()
            };

        // Get validator address from config
        let mut validator_address = mining_config.validator_address.unwrap_or(Address::ZERO);

        // Try to load signing key and derive validator address if not set
        if validator_address == Address::ZERO {
            if let Some(keystore_path) = &mining_config.keystore_path {
                let password = mining_config.keystore_password.as_deref().unwrap_or("");
                if let Ok(signing_key) =
                    keystore::load_private_key_from_keystore(keystore_path, password)
                {
                    validator_address = keystore::get_validator_address(&signing_key);
                    tracing::info!(
                        "Derived validator address from keystore: {}",
                        validator_address
                    );
                }
            } else if let Some(hex_key) = &mining_config.private_key_hex {
                if let Ok(signing_key) = keystore::load_private_key_from_hex(hex_key) {
                    validator_address = keystore::get_validator_address(&signing_key);
                    tracing::info!("Derived validator address from hex key: {}", validator_address);
                }
            }
        }

        // Get MEV parameters from config
        let chain_id = chain_spec.chain().id();
        let gas_ceil = mining_config.get_gas_limit(chain_id);
        let min_gas_tip = mining_config.get_min_gas_tip();
        let min_gas_price = U256::from(min_gas_tip);

        // Get MEV parameters from mining config with fallback to defaults
        let validator_commission = mining_config.get_validator_commission();
        let bid_simulation_left_over = mining_config.get_bid_simulation_left_over();
        let no_interrupt_left_over = mining_config.get_no_interrupt_left_over();
        let delay_left_over = mining_config.get_delay_left_over();
        let max_bids_per_builder = mining_config.get_max_bids_per_builder();
        let builder_fee_ceil = U256::from(mining_config.get_builder_fee_ceil());
        let bid_block_enabled = mining_config.get_bid_block_enabled();

        // Version string
        let version = env!("CARGO_PKG_VERSION").to_string();

        // Initialize allowed builders from config
        // If not configured, initialize as empty HashSet (no builders allowed by default)
        let allowed_builders = mining_config
            .allowed_builders
            .map(|addrs| addrs.into_iter().collect::<HashSet<_>>())
            .unwrap_or_default(); // Empty HashSet if not configured

        // Register the whitelist in shared state so miner_ namespace can also access it
        let allowed_builders = crate::shared::init_builder_whitelist(allowed_builders);

        if allowed_builders.read().unwrap().is_empty() {
            tracing::warn!(
                "MEV API initialized with EMPTY builder whitelist - NO builders will be accepted!"
            );
            tracing::warn!(
                "Use mev_addBuilder or miner_addBuilder to add builders, or set BSC_ALLOWED_BUILDERS environment variable"
            );
        } else {
            let count = allowed_builders.read().unwrap().len();
            tracing::info!("MEV API initialized with builder whitelist: {} builders", count);
            for builder in allowed_builders.read().unwrap().iter() {
                tracing::info!("  - Allowed builder: {}", builder);
            }
        }

        tracing::info!(
            "MEV API initialized: validator_address={}, validator_commission={}({}%), gas_ceil={}, min_gas_price={}, version={}",
            validator_address, validator_commission, validator_commission as f64 / 100.0, gas_ceil, min_gas_price, version
        );

        Self {
            snapshot_provider,
            chain_spec,
            validator_address,
            validator_commission,
            bid_simulation_left_over,
            no_interrupt_left_over,
            delay_left_over,
            max_bids_per_builder,
            gas_ceil,
            min_gas_price,
            builder_fee_ceil,
            bid_block_enabled,
            version,
            allowed_builders,
            pending_bid_blocks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Mirrors go-bsc `bidSimulator.CheckPending`: returns an error if `bid_hash` is already
    /// registered for `(block_number, builder)` or if the builder has reached the per-block quota.
    fn check_pending_bid_block(
        &self,
        block_number: u64,
        builder: Address,
        bid_hash: B256,
    ) -> Result<(), String> {
        let pending = self.pending_bid_blocks.read().unwrap();
        if let Some(by_builder) = pending.get(&block_number) {
            if let Some(hashes) = by_builder.get(&builder) {
                if hashes.contains(&bid_hash) {
                    return Err("bid already exists".to_string());
                }
                if hashes.len() >= self.max_bids_per_builder as usize {
                    return Err(format!(
                        "too many bids: exceeded limit of {} bids per builder per block",
                        self.max_bids_per_builder
                    ));
                }
            }
        }
        Ok(())
    }

    /// Mirrors go-bsc `bidSimulator.AddPending`: registers `bid_hash` for `(block_number, builder)`.
    fn add_pending_bid_block(&self, block_number: u64, builder: Address, bid_hash: B256) {
        let mut pending = self.pending_bid_blocks.write().unwrap();
        pending
            .entry(block_number)
            .or_default()
            .entry(builder)
            .or_default()
            .insert(bid_hash);
    }

    /// Mirrors go-bsc `bidutil.BidMustBefore`: the deadline after which a bid is too late.
    ///
    /// See [`bid_must_before_ms`] for the formula and why `parent_timestamp_ms` must be the
    /// parent's full millisecond timestamp.
    fn bid_must_before_ms(&self, parent_timestamp_ms: u64, block_interval_ms: u64) -> u128 {
        bid_must_before_ms(parent_timestamp_ms, block_interval_ms, self.delay_left_over)
    }

    /// Get header by number from global header provider
    fn get_header_by_number(&self, block_number: u64) -> Option<alloy_consensus::Header> {
        crate::shared::get_canonical_header_by_number_from_provider(block_number)
    }

    /// Check if a builder is allowed
    fn is_builder_allowed(&self, builder: &Address) -> bool {
        let allowed_builders = self.allowed_builders.read().unwrap();
        // Empty HashSet means no builders are allowed
        allowed_builders.contains(builder)
    }

    /// `NewInvalidBidError` — generic invalid-bid rejection (code `-38001`).
    fn invalid_bid(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(INVALID_BID_PARAM_ERROR, msg.into(), None::<()>)
    }

    /// `ErrMevNotRunning` (code `-38003`, fixed message).
    fn mev_not_running() -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(
            MEV_NOT_RUNNING_ERROR,
            "the validator stop accepting bids for now, try again later",
            None::<()>,
        )
    }

    /// `ErrMevNotInTurn` (code `-38005`, fixed message).
    fn mev_not_in_turn() -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(
            MEV_NOT_IN_TURN_ERROR,
            "the validator is not in-turn to propose currently, try again later",
            None::<()>,
        )
    }

    /// `NewBidBlockPermissionRevokedError` (code `-38006`).
    fn permission_revoked(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(
            BID_BLOCK_PERMISSION_REVOKED_ERROR,
            msg.into(),
            None::<()>,
        )
    }

    /// `NewBidBlockPreSealVerifyError` (code `-38007`): the synchronous checks
    /// `preSealVerifyBidBlock` runs at admission failed.
    fn pre_seal_verify_failed(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(BID_BLOCK_PRE_SEAL_VERIFY_ERROR, msg.into(), None::<()>)
    }

    /// `NewBidBlockTooLateError` (code `-38008`): the bid arrived after `bidMustBefore`.
    fn too_late(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(BID_BLOCK_TOO_LATE_ERROR, msg.into(), None::<()>)
    }

    /// Internal error (`-32603`) for conditions go-bsc never hits (e.g. the chain head missing).
    fn internal_err(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
        jsonrpsee::types::ErrorObject::owned(-32603, msg.into(), None::<()>)
    }

    /// Submit a builder-proposed BEP-675 block through the common validator admission path.
    ///
    /// Both `mev_sendBidBlock` and `mev.v1.BidBlockService/SendBidBlock` call this method so the
    /// fork gate, signature/permission checks, duplicate quota and miner queue cannot diverge by
    /// transport.
    pub async fn submit_bid_block(&self, args: BidBlockArgs) -> RpcResult<B256> {
        let bb = &args.bid_block;

        if !crate::shared::is_mev_running() {
            return Err(Self::mev_not_running());
        }

        // Chain-head context (the parent the bid must build on); go-bsc reads `CurrentBlock()`.
        let head_number = crate::shared::get_best_canonical_block_number()
            .ok_or_else(|| Self::internal_err("chain head unavailable"))?;
        let head_header = self
            .get_header_by_number(head_number)
            .ok_or_else(|| Self::internal_err("chain head header unavailable"))?;

        // Number, then in-turn, then parent-hash alignment — go-bsc's order, enforced in
        // `validate_bid_block_structure` so the ordering itself is unit-tested.
        let block_number = bb.header.number;
        let parent_hash = head_header.hash_slow();
        let is_inturn = self
            .snapshot_provider
            .snapshot_by_hash(&parent_hash)
            .is_some_and(|snapshot| snapshot.is_inturn(self.validator_address));

        if let Err(rejection) = validate_bid_block_structure(
            block_number,
            head_number,
            is_inturn,
            bb.header.parent_hash,
            parent_hash,
            bb.header.gas_used,
            bb.transactions.len(),
        ) {
            use BidBlockStructuralRejection as R;
            return Err(match rejection {
                R::StaleNumber => Self::invalid_bid(format!(
                    "stale block number: {block_number}, latest block: {head_number}"
                )),
                R::FutureNumber => Self::invalid_bid(format!(
                    "block in future: {block_number}, latest block: {head_number}"
                )),
                R::NotInTurn => Self::mev_not_in_turn(),
                R::NonAlignedParent => {
                    Self::invalid_bid(format!("non-aligned parent hash: {parent_hash:?}"))
                }
                R::EmptyGasUsed => Self::invalid_bid("empty gasUsed in header"),
                R::EmptyTransactions => Self::invalid_bid("empty transactions"),
            });
        }

        // Every rejection above returns before this point, so nothing structurally invalid can
        // reach the miner queue (TC-009's `bid_block_queue_len()` invariant).
        self.admit_bid_block(&args, &head_header)
    }

    /// Miner-side BidBlock admission — mirrors the front of go-bsc `Miner.SendBidBlock`:
    /// `bidBlockEnabled()` gate, builder recovery, whitelist (`ExistBuilder`) and permission. The
    /// simulator-backed tail (`recordBidBlockBuilder`, `CheckPending`, bid timing,
    /// `ToDecodedBidBlock` + parlia extra/blind-sign, the full `preSealVerifyBidBlock`, and the
    /// bid-simulator enqueue) is the validator-side build path and lands in 8d. Until then
    /// admission acknowledges the bid hash.
    fn admit_bid_block(
        &self,
        args: &BidBlockArgs,
        head_header: &alloy_consensus::Header,
    ) -> RpcResult<B256> {
        let bid_hash = args.bid_block.hash();

        // bidBlockEnabled(): MEV running AND the BidBlockEnabled flag AND Pasteur active at head.
        let pasteur_active = self
            .chain_spec
            .is_pasteur_active_at_timestamp(head_header.number, head_header.timestamp);
        if !bid_block_admission_enabled(
            crate::shared::is_mev_running(),
            self.bid_block_enabled,
            pasteur_active,
        ) {
            return Err(Self::invalid_bid("BidBlock disabled, fallback to SendBid"));
        }

        let builder = args.ecrecover_sender().map_err(|e| {
            Self::invalid_bid(format!("invalid signature: bidHash={bid_hash}, err={e}"))
        })?;
        if !self.is_builder_allowed(&builder) {
            return Err(Self::invalid_bid(format!(
                "builder is not registered: builder={builder}, bidHash={bid_hash}"
            )));
        }

        // Mirrors go-bsc: permission check comes before CheckPending so a revoked builder cannot
        // consume quota.
        if !crate::shared::get_bid_block_permission_manager().is_allowed(builder) {
            return Err(Self::permission_revoked(
                "builder BidBlock permission revoked, fallback to SendBid",
            ));
        }

        // Mirrors go-bsc `bidSimulator.CheckPending`: duplicate + per-builder quota guard.
        // Must run before the timing check so rejected bids do not consume quota.
        let block_number = args.bid_block.header.number;
        self.check_pending_bid_block(block_number, builder, bid_hash)
            .map_err(Self::invalid_bid)?;

        // Mirrors go-bsc `bidSimulator.bidMustBefore`: reject bids that arrive after the
        // validator must have already started sealing (no time left to simulate).
        let block_interval_ms = self
            .snapshot_provider
            .snapshot_by_hash(&head_header.hash_slow())
            .map(|s| s.block_interval)
            .unwrap_or(3_000); // 3 s default
        let parent_timestamp_ms =
            crate::consensus::parlia::util::calculate_millisecond_timestamp(head_header);
        let bid_must_before_ms = self.bid_must_before_ms(parent_timestamp_ms, block_interval_ms);
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        if now_ms >= bid_must_before_ms {
            return Err(Self::too_late(format!(
                "too late: bid must arrive before {}ms, arrived {}ms later, bidHash={bid_hash}",
                bid_must_before_ms,
                now_ms.saturating_sub(bid_must_before_ms),
            )));
        }

        let mut decoded = args
            .to_decoded_bid_block(builder)
            .map_err(|e| Self::invalid_bid(format!("failed to decode bid block: {e}")))?;

        // Mirrors go-bsc `preSealVerifyBidBlock`'s payload-only checks (coinbase, gas limit, the
        // deposit-derived gas fee, blob sidecar structure, per-tx gas cap, trailing system-tx
        // shape) synchronously, returning `-38007` immediately on failure like geth does — rather
        // than admitting optimistically and dropping the bid silently later.
        //
        // NOT run here: `verify_bid_block_header`'s structural + cascading checks (extra-data
        // length/validator-list layout, authorized-validator/sign-recently/difficulty against the
        // snapshot). go-bsc only makes those checks meaningful by first overwriting the builder's
        // `Extra` with the validator's own reconstructed vanity/forkhash/validator-list/turnLength
        // (`SetExtraData`, run before `preSealVerifyBidBlock`) — replicating that rewrite here
        // would risk rejecting legitimate submissions whose raw `Extra` doesn't yet match that
        // final structure. Those checks remain deferred to the miner side
        // (`simulate_bid_block`), which does perform the rewrite first.
        let gas_ceil = crate::shared::get_miner_gas_limit().unwrap_or(head_header.gas_limit);
        let expected_gas_limit =
            EthereumBuilderConfig::new().with_gas_limit(gas_ceil).gas_limit(head_header.gas_limit);
        match crate::node::miner::bid_block::verify_bid_block_payload(
            &self.chain_spec,
            &decoded,
            head_header,
            self.validator_address,
            expected_gas_limit,
        ) {
            Ok((system_tx_start, gas_fee)) => {
                decoded.system_tx_start = system_tx_start;
                decoded.gas_fee = gas_fee;
            }
            Err(e) => {
                return Err(Self::pre_seal_verify_failed(format!(
                    "pre-seal verify failed: bidHash={bid_hash}, err={e}"
                )));
            }
        }

        // Decode and hand to the miner via the global intake queue.
        //
        // The remaining tail of go-bsc's Miner.SendBidBlock is intentionally deferred to the
        // miner side (bid_block::simulate_bid_block, called from BidSimulator::commit_bid_block):
        //
        //   • Extra overwrite + SetExtraData  →  header.extra_data = vanity + finalize_new_header
        //   • setBidMevInfo                   →  set_bid_block_mev_info
        //   • verify_bid_block_header         →  structural + cascading header checks (see above)
        //   • execution + state-root check    →  execute_bid_block_payload
        //
        // Behavioral difference vs geth: geth runs the queue handoff itself
        // (`sendBidBlock`/`newBidBlockLoop`) with a bounded channel and returns `ErrMevBusy`
        // (`-38004`) on a 1s enqueue timeout; reth-bsc's intake queue is unbounded, so admission
        // can never report busy. Left as a known gap — implementing genuine backpressure here is
        // a distinct change from surfacing the correct error codes for checks that already run.
        //
        // Register after all checks pass so quota is only consumed by accepted bids.
        self.add_pending_bid_block(block_number, builder, bid_hash);

        crate::shared::push_bid_block_package(decoded);

        tracing::info!(
            "BidBlock queued: block={block_number}, builder={builder}, bidHash={bid_hash:?}",
        );

        Ok(bid_hash)
    }

    /// Add a builder to the whitelist
    fn add_builder_internal(&self, builder: Address) -> bool {
        let mut allowed_builders = self.allowed_builders.write().unwrap();
        // Add to whitelist, returns true if newly added
        allowed_builders.insert(builder)
    }

    /// Remove a builder from the whitelist
    fn remove_builder_internal(&self, builder: &Address) -> bool {
        let mut allowed_builders = self.allowed_builders.write().unwrap();
        // Remove from whitelist, returns true if it was present
        allowed_builders.remove(builder)
    }

    /// Parse transaction from bytes with validation
    /// This matches the Go implementation: DecodeTxs(signer)
    fn parse_transaction(
        tx_bytes: &alloy_primitives::Bytes,
        chain_spec: &BscChainSpec,
    ) -> Result<TransactionSigned, String> {
        // Decode RLP to TransactionSigned
        use alloy_rlp::Decodable;
        let tx = TransactionSigned::decode(&mut &tx_bytes[..])
            .map_err(|e| format!("Failed to decode transaction: {}", e))?;

        // Validate chain ID if present (EIP-155)
        if let Some(tx_chain_id) = tx.chain_id() {
            if tx_chain_id != chain_spec.chain().id() {
                return Err(format!(
                    "Transaction chain ID {} does not match expected chain ID {}",
                    tx_chain_id,
                    chain_spec.chain().id()
                ));
            }
        }

        // Additional validation: ensure signature is valid
        // This will verify that the transaction can recover a valid signer
        tx.recover_signer().map_err(|e| format!("Failed to recover transaction signer: {}", e))?;

        Ok(tx)
    }

    /// Decode transaction with sidecar support
    /// This matches Go's UnmarshalBinary + decodeTyped logic
    /// For blob transactions, tries to extract sidecar from the byte stream
    fn decode_transaction_with_sidecar(
        tx_bytes: &alloy_primitives::Bytes,
        chain_spec: &BscChainSpec,
    ) -> Result<DecodedTransaction, String> {
        if tx_bytes.is_empty() {
            return Err("Empty transaction bytes".to_string());
        }

        // Check if it's a legacy transaction (first byte > 0x7f)
        let is_legacy = tx_bytes[0] > 0xc0;
        if is_legacy {
            // Legacy transaction - no sidecar possible
            let tx = Self::parse_transaction(tx_bytes, chain_spec)?;
            return Ok(DecodedTransaction { tx, sidecar: None });
        }

        // EIP-2718 typed transaction envelope
        let tx_type = tx_bytes[0];

        // For blob transactions (type 0x03), check if sidecar is included
        const BLOB_TX_TYPE: u8 = 0x03;

        if tx_type == BLOB_TX_TYPE {
            debug!(
                "Detected blob transaction, length: {}, first 64 bytes: {}",
                tx_bytes.len(),
                hex::encode(&tx_bytes[..tx_bytes.len().min(64)])
            );

            // Try to decode with sidecar first
            let payload = &tx_bytes[1..]; // Skip type byte

            match Self::try_decode_blob_tx_with_sidecar(payload) {
                Ok((tx, sidecar)) => {
                    // Validate chain ID
                    if let Some(tx_chain_id) = tx.chain_id() {
                        if tx_chain_id != chain_spec.chain().id() {
                            return Err(format!(
                                "Transaction chain ID {} does not match expected chain ID {}",
                                tx_chain_id,
                                chain_spec.chain().id()
                            ));
                        }
                    }

                    // Validate signature
                    tx.recover_signer()
                        .map_err(|e| format!("Failed to recover transaction signer: {}", e))?;

                    debug!(
                        "Successfully decoded blob tx {:?} with sidecar ({} blobs)",
                        tx.hash(),
                        sidecar.blobs.len()
                    );

                    return Ok(DecodedTransaction { tx, sidecar: Some(sidecar) });
                }
                Err(e) => {
                    debug!("Failed to decode with sidecar: {}, trying without", e);
                    // Fall through to standard decoding
                }
            }
        }

        // Standard decoding (no sidecar)
        let tx = Self::parse_transaction(tx_bytes, chain_spec)?;

        Ok(DecodedTransaction { tx, sidecar: None })
    }

    /// Try to decode a blob transaction with sidecar from RLP payload
    /// Uses alloy-consensus's TxEip4844WithSidecar which already has decode logic
    fn try_decode_blob_tx_with_sidecar(
        payload: &[u8],
    ) -> Result<(TransactionSigned, BlobTransactionSidecar), String> {
        use alloy_consensus::Signed;

        debug!(
            "Attempting to decode blob tx with sidecar using TxEip4844WithSidecar, payload length: {}",
            payload.len()
        );

        let mut buf = payload;

        // Decode using alloy's TxEip4844WithSidecar which handles the format:
        // rlp([tx_fields..., signature_fields, sidecar_fields])
        let (tx_with_sidecar, signature) =
            TxEip4844WithSidecar::<BlobTransactionSidecar>::rlp_decode_with_signature(&mut buf)
                .map_err(|e| {
                    debug!("Failed to decode TxEip4844WithSidecar: {}", e);
                    format!("Failed to decode transaction with sidecar: {}", e)
                })?;

        debug!(
            "Successfully decoded TxEip4844WithSidecar, blobs={}, remaining bytes={}",
            tx_with_sidecar.sidecar.blobs.len(),
            buf.len()
        );

        // Convert to TransactionSigned
        // First get the inner TxEip4844 and sidecar
        let (eip4844_tx, sidecar) = tx_with_sidecar.into_parts();

        // Create a Signed<TxEip4844>
        let signed_eip4844 = Signed::new_unhashed(eip4844_tx, signature);

        // Convert to TransactionSigned via TxEnvelope
        use alloy_consensus::TxEnvelope;
        let envelope: TxEnvelope = signed_eip4844.into();
        let tx_signed = TransactionSigned::from(envelope);

        debug!(
            "Converted to TransactionSigned: tx_hash={:?}, blobs={}",
            tx_signed.hash(),
            sidecar.blobs.len()
        );

        Ok((tx_signed, sidecar))
    }

    /// Convert BidArgs to Bid object
    /// This matches the Go implementation: BidArgs.ToBid()
    /// Returns the Bid object with blob sidecars included.
    fn to_bid(
        bid_args: &BidArgs,
        builder: alloy_primitives::Address,
        chain_spec: &BscChainSpec,
        bid_hash: B256,
    ) -> Result<Bid, String> {
        use std::collections::HashMap;

        // 1. Decode transactions from RawBid, extracting sidecars
        let mut txs = Vec::new();
        let mut blob_sidecars = HashMap::new();

        for tx_bytes in &bid_args.raw_bid.txs {
            let decoded = Self::decode_transaction_with_sidecar(tx_bytes, chain_spec)?;

            // Store sidecar if present
            if let Some(sidecar) = decoded.sidecar {
                let tx_hash = *decoded.tx.hash();
                debug!(
                    "Found blob sidecar for tx {:?} with {} blobs",
                    tx_hash,
                    sidecar.blobs.len()
                );
                blob_sidecars.insert(tx_hash, sidecar);
            }

            txs.push(decoded.tx);
        }

        // 2. Validate UnRevertible count
        if bid_args.raw_bid.un_revertible.len() > txs.len() {
            return Err(format!(
                "expect UnRevertible no more than {}, got {}",
                txs.len(),
                bid_args.raw_bid.un_revertible.len()
            ));
        }

        // 3. Handle PayBidTx if present
        if !bid_args.pay_bid_tx.is_empty() {
            let decoded = Self::decode_transaction_with_sidecar(&bid_args.pay_bid_tx, chain_spec)
                .map_err(|e| format!("Failed to parse PayBidTx: {}", e))?;

            // Store sidecar if present
            if let Some(sidecar) = decoded.sidecar {
                let tx_hash = *decoded.tx.hash();
                debug!(
                    "Found blob sidecar for PayBidTx {:?} with {} blobs",
                    tx_hash,
                    sidecar.blobs.len()
                );
                blob_sidecars.insert(tx_hash, sidecar);
            }

            txs.push(decoded.tx);
        }

        debug!(
            "Decoded {} transactions with {} blob sidecars for bid",
            txs.len(),
            blob_sidecars.len()
        );

        // 4. Create Bid object
        let bid = Bid {
            builder,
            block_number: bid_args.raw_bid.block_number.to(),
            parent_hash: bid_args.raw_bid.parent_hash,
            txs,
            blob_sidecars,
            un_revertible: bid_args.raw_bid.un_revertible.clone(),
            gas_used: bid_args.raw_bid.gas_used.to(),
            gas_fee: bid_args.raw_bid.gas_fee,
            builder_fee: bid_args.raw_bid.builder_fee.unwrap_or(U256::ZERO),
            committed: false,
            bid_hash,
            interrupt_flag: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        };

        Ok(bid)
    }

    /// Calculate RawBid hash
    /// This matches the Go implementation: rlpHash(RawBid)
    fn calculate_raw_bid_hash(raw_bid: &RawBid) -> B256 {
        use alloy_primitives::keccak256;
        use alloy_rlp::Encodable;

        // RLP encode the RawBid structure
        // The structure is: [blockNumber, parentHash, txs, unRevertible, gasUsed, gasFee, builderFee]
        let mut rlp_buffer = Vec::new();

        // Get builder_fee value (use 0 if None)
        let builder_fee = raw_bid.builder_fee.unwrap_or(U256::ZERO);

        // First calculate the length of all encoded items
        let payload_length = raw_bid.block_number.length()
            + raw_bid.parent_hash.length()
            + raw_bid.txs.length()
            + raw_bid.un_revertible.length()
            + raw_bid.gas_used.length()
            + raw_bid.gas_fee.length()
            + builder_fee.length();

        // Encode the list header
        alloy_rlp::Header { list: true, payload_length }.encode(&mut rlp_buffer);

        // Encode each field
        raw_bid.block_number.encode(&mut rlp_buffer);
        raw_bid.parent_hash.encode(&mut rlp_buffer);
        raw_bid.txs.encode(&mut rlp_buffer);
        raw_bid.un_revertible.encode(&mut rlp_buffer);
        raw_bid.gas_used.encode(&mut rlp_buffer);
        raw_bid.gas_fee.encode(&mut rlp_buffer);
        builder_fee.encode(&mut rlp_buffer);

        // Calculate keccak256 hash
        let hash = keccak256(&rlp_buffer);
        debug!("RawBid RLP encoded length: {}, hash: {:?}", rlp_buffer.len(), hash);
        hash
    }

    /// Recover builder address from signature
    fn recover_builder_address(
        raw_bid: &RawBid,
        signature: &alloy_primitives::Bytes,
    ) -> Result<alloy_primitives::Address, String> {
        use alloy_primitives::keccak256;
        use secp256k1::{Message, Secp256k1};

        if signature.len() != 65 {
            return Err(format!("Invalid signature length: {}", signature.len()));
        }

        // Calculate the hash of RawBid
        let hash = Self::calculate_raw_bid_hash(raw_bid);

        // Create message from hash
        let message = Message::from_digest_slice(hash.as_slice())
            .map_err(|e| format!("Failed to create message: {}", e))?;

        // Parse signature (r, s, v format - Ethereum style)
        let recovery_id = signature[64];
        // Ethereum uses v = 27 or 28, we need to convert to 0 or 1
        let recovery_id_value = if recovery_id >= 27 { recovery_id - 27 } else { recovery_id };

        // Create RecoveryId from i32
        let recovery_id = secp256k1::ecdsa::RecoveryId::try_from(i32::from(recovery_id_value))
            .map_err(|e| format!("Invalid recovery id: {:?}", e))?;

        let sig_bytes = &signature[..64];
        let recoverable_sig =
            secp256k1::ecdsa::RecoverableSignature::from_compact(sig_bytes, recovery_id)
                .map_err(|e| format!("Failed to parse signature: {}", e))?;

        // Recover public key
        let secp = Secp256k1::new();
        let public_key = secp
            .recover_ecdsa(&message, &recoverable_sig)
            .map_err(|e| format!("Failed to recover public key: {}", e))?;

        // Convert public key to address
        let public_key_bytes = public_key.serialize_uncompressed();
        // Skip the first byte (0x04) which is the uncompressed marker
        let public_key_hash = keccak256(&public_key_bytes[1..]);

        // Take the last 20 bytes as the address
        let address = alloy_primitives::Address::from_slice(&public_key_hash[12..]);

        Ok(address)
    }
}

#[async_trait::async_trait]
impl BscMevApiServer for MevApiImpl {
    /// Send a bid to the builder
    /// Returns the bid hash
    async fn send_bid(&self, bid: BidArgs) -> RpcResult<B256> {
        tracing::info!(
            "Received bid for block {} with {} txs",
            bid.raw_bid.block_number,
            bid.raw_bid.txs.len()
        );

        // bid.raw_bid.block_number is the NEW block to be built
        // bid.raw_bid.parent_hash is the hash of the PARENT block (block_number - 1)
        let new_block_number: u64 = bid.raw_bid.block_number.to();
        let parent_block_number = new_block_number.saturating_sub(1);

        // Get parent block header from chain (not from snapshot!)
        let parent_header = match self.get_header_by_number(parent_block_number) {
            Some(header) => header,
            None => {
                tracing::error!(
                    "Skip bid: parent block {} not found on chain",
                    parent_block_number
                );
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Parent block not found",
                    None::<()>,
                ));
            }
        };

        // Verify parent hash matches
        let parent_hash = parent_header.hash_slow();
        if bid.raw_bid.parent_hash != parent_hash {
            tracing::error!(
                "Skip bid: parent hash mismatch. Expected: {:?}, Got: {:?}, Block: {}",
                parent_hash,
                bid.raw_bid.parent_hash,
                new_block_number
            );
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Parent hash mismatch",
                None::<()>,
            ));
        }

        // Recover builder address from signature
        let builder = match Self::recover_builder_address(&bid.raw_bid, &bid.signature) {
            Ok(addr) => addr,
            Err(e) => {
                tracing::error!("Failed to recover builder address: {}", e);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    format!("Invalid signature: {}", e),
                    None::<()>,
                ));
            }
        };
        debug!("builder: {:?}", builder);

        // Check if builder is in whitelist
        if !self.is_builder_allowed(&builder) {
            tracing::error!(
                "Builder {} is not in whitelist, rejecting bid for block {}",
                builder,
                new_block_number
            );
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32603,
                format!("Builder {} is not registered", builder),
                None::<()>,
            ));
        }

        // Calculate bid hash (using RLP hash of RawBid)
        let bid_hash = Self::calculate_raw_bid_hash(&bid.raw_bid);
        debug!("bid_hash: {:?}", bid_hash);

        // Optional: Check if validator is inturn using snapshot (for filtering bids)
        // Note: This is optional - you may want to accept bids even when not inturn
        if let Some(snapshot) = self.snapshot_provider.snapshot_by_hash(&parent_hash) {
            // You can add validator checks here if needed
            tracing::debug!(
                "Validator snapshot available for block {}, validators: {}",
                parent_block_number,
                snapshot.validators.len()
            );
            if !snapshot.is_inturn(self.validator_address) {
                tracing::error!(
                    "Skip bid: validator is not inturn, block number: {}, validator address: {}",
                    new_block_number,
                    self.validator_address
                );
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Validator is not inturn",
                    None::<()>,
                ));
            }
        } else {
            tracing::debug!(
                "No snapshot available for block {} (validator may not be inturn)",
                parent_block_number
            );
        }

        if bid.raw_bid.gas_fee == 0 || bid.raw_bid.gas_used == 0 {
            tracing::error!(
                "Skip to new bid due to gas fee or gas used is 0, block number: {}",
                new_block_number
            );
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Gas fee or gas used is 0",
                None::<()>,
            ));
        }

        // Validate builder_fee if provided
        if let Some(builder_fee) = bid.raw_bid.builder_fee {
            // U256 is always >= 0, so no need to check for negative values
            if builder_fee > bid.raw_bid.gas_fee {
                tracing::error!(
                    "Skip to new bid due to builder fee is greater than gas fee, block number: {}",
                    new_block_number
                );
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Builder fee is greater than gas fee",
                    None::<()>,
                ));
            }
        }

        if bid.pay_bid_tx.is_empty() || bid.pay_bid_tx_gas_used == 0 {
            tracing::error!(
                "Skip to new bid due to pay bid tx is empty or gas used is 0, block number: {}",
                new_block_number
            );
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Pay bid tx is empty or gas used is 0",
                None::<()>,
            ));
        }

        if bid.pay_bid_tx_gas_used > PAY_BID_TX_GAS_LIMIT {
            tracing::error!("Skip to new bid due to pay bid tx gas used is greater than limit, block number: {}", new_block_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Pay bid tx gas used is greater than limit",
                None::<()>,
            ));
        }
        // Check if this bid is already pending - skip for now as we removed miner reference
        // TODO: Add check_pending_bid to global state if needed

        // Convert BidArgs to Bid object
        let bid_obj = match Self::to_bid(&bid, builder, &self.chain_spec, bid_hash) {
            Ok(bid) => bid,
            Err(e) => {
                tracing::error!("Failed to convert BidArgs to Bid: {}", e);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    format!("Invalid bid: {}", e),
                    None::<()>,
                ));
            }
        };

        // Log acceptance before async processing
        tracing::info!(
            "Bid accepted for block {}, bid_hash: {:?}",
            bid.raw_bid.block_number,
            bid_hash
        );

        // Submit to global bid queue
        debug!(
            "push bid package to queue bid_hash: {:?}, send time: {:?}",
            bid_hash,
            std::time::Instant::now()
        );
        if let Err(e) = crate::shared::push_bid_package(bid_obj) {
            tracing::error!("Failed to push bid package to queue: {}", e);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32603,
                format!("Failed to queue bid: {}", e),
                None::<()>,
            ));
        }

        Ok(bid_hash)
    }

    /// RPC entry for a builder-proposed block (BEP-675).
    ///
    /// Mirrors go-bsc `MevAPI.SendBidBlock`: MEV-running check, then structural validation (the bid
    /// must build exactly the next block on the head, the validator in turn, aligned parent, and
    /// non-empty gasUsed/txs). It then delegates to [`MevApiImpl::admit_bid_block`], which mirrors
    /// `Miner.SendBidBlock`.
    async fn send_bid_block(&self, args: BidBlockArgs) -> RpcResult<B256> {
        self.submit_bid_block(args).await
    }

    /// Get MEV parameters
    /// Returns the current MEV configuration matching geth-bsc implementation
    async fn params(&self) -> RpcResult<MevParams> {
        tracing::debug!("MEV params requested");

        // Mirrors go-bsc's `Miner.bidBlockEnabled()`: MEV running AND the BidBlockEnabled flag
        // AND Pasteur active at the chain head — dynamic, not a static echo of the config flag,
        // so a builder never sees `BidBlockEnabled: true` right before every submission is
        // rejected with "disabled" for not-yet-being-past Pasteur. Falls back to `false` if the
        // chain head isn't available yet (e.g. still syncing at startup) rather than failing the
        // whole `params()` call.
        let pasteur_active = crate::shared::get_best_canonical_block_number()
            .and_then(|n| self.get_header_by_number(n))
            .is_some_and(|head| {
                self.chain_spec.is_pasteur_active_at_timestamp(head.number, head.timestamp)
            });
        let bid_block_enabled = bid_block_admission_enabled(
            crate::shared::is_mev_running(),
            self.bid_block_enabled,
            pasteur_active,
        );

        Ok(MevParams {
            validator_commission: self.validator_commission,
            // Convert milliseconds to nanoseconds (1ms = 1,000,000 ns)
            bid_simulation_left_over: self.bid_simulation_left_over * 1_000_000,
            no_interrupt_left_over: self.no_interrupt_left_over * 1_000_000,
            delay_left_over: self.delay_left_over * 1_000_000,
            max_bids_per_builder: self.max_bids_per_builder,
            gas_ceil: self.gas_ceil,
            gas_price: self.min_gas_price,
            builder_fee_ceil: self.builder_fee_ceil,
            bid_block_enabled,
            version: self.version.clone(),
        })
    }

    /// Check if MEV is running
    /// Returns true if MEV worker is active and accepting bids
    async fn running(&self) -> RpcResult<bool> {
        tracing::debug!("MEV running status requested");
        Ok(crate::shared::is_mev_running())
    }

    /// Check if a builder is registered in the whitelist
    async fn has_builder(&self, builder: Address) -> RpcResult<bool> {
        tracing::debug!("Checking if builder {} is registered", builder);
        Ok(self.is_builder_allowed(&builder))
    }

    /// Add a builder to the whitelist
    async fn add_builder(&self, builder: Address) -> RpcResult<bool> {
        tracing::info!("Adding builder {} to whitelist", builder);
        let added = self.add_builder_internal(builder);
        if added {
            tracing::info!("Builder {} successfully added to whitelist", builder);
        } else {
            tracing::info!("Builder {} was already in whitelist", builder);
        }
        Ok(added)
    }

    /// Remove a builder from the whitelist
    async fn remove_builder(&self, builder: Address) -> RpcResult<bool> {
        tracing::info!("Removing builder {} from whitelist", builder);
        let removed = self.remove_builder_internal(&builder);
        if removed {
            tracing::info!("Builder {} successfully removed from whitelist", builder);
        } else {
            tracing::info!("Builder {} was not in whitelist", builder);
        }
        Ok(removed)
    }

    /// Query a builder's current BidBlock permission status.
    async fn get_bid_block_permission(
        &self,
        builder: Address,
    ) -> RpcResult<BidBlockPermissionResult> {
        let status = crate::shared::get_bid_block_permission_manager().get_status(builder);
        Ok(status.into())
    }
}

#[cfg(test)]
mod bid_block_param_tests {
    use super::*;

    #[test]
    fn mev_params_exposes_bid_block_enabled_field() {
        let params = MevParams {
            validator_commission: 100,
            bid_simulation_left_over: 0,
            no_interrupt_left_over: 0,
            delay_left_over: 0,
            max_bids_per_builder: 3,
            gas_ceil: 0,
            gas_price: U256::ZERO,
            builder_fee_ceil: U256::ZERO,
            bid_block_enabled: true,
            version: "test".to_string(),
        };
        let json = serde_json::to_value(&params).unwrap();
        // geth parity: the field is exposed as "BidBlockEnabled".
        assert_eq!(json.get("BidBlockEnabled"), Some(&serde_json::Value::Bool(true)));
    }

    #[test]
    fn mev_params_exposes_delay_left_over_field() {
        // geth parity (bnb-chain/bsc#3807): `Miner.Config.DelayLeftOver` is exposed as
        // "DelayLeftOver" (a Go `time.Duration`, i.e. nanoseconds in JSON) so builders can
        // compute `BidMustBefore` per validator instead of hardcoding the 15ms default.
        let params = MevParams {
            validator_commission: 100,
            bid_simulation_left_over: 0,
            no_interrupt_left_over: 0,
            delay_left_over: 15 * 1_000_000, // 15ms in nanoseconds
            max_bids_per_builder: 3,
            gas_ceil: 0,
            gas_price: U256::ZERO,
            builder_fee_ceil: U256::ZERO,
            bid_block_enabled: true,
            version: "test".to_string(),
        };
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(
            json.get("DelayLeftOver"),
            Some(&serde_json::Value::from(15_000_000u64)),
            "DelayLeftOver must serialize under go-bsc's key, in nanoseconds"
        );
    }

    #[test]
    fn bid_block_admission_requires_all_three_conditions() {
        // geth `bidBlockEnabled()` = MEV running AND flag set AND Pasteur active.
        assert!(super::bid_block_admission_enabled(true, true, true));
        assert!(!super::bid_block_admission_enabled(false, true, true));
        assert!(!super::bid_block_admission_enabled(true, false, true));
        assert!(!super::bid_block_admission_enabled(true, true, false));
    }

    /// A submission that passes every structural check, as the baseline the rejection tests mutate.
    fn valid_structure() -> (u64, u64, bool, B256, B256, u64, usize) {
        let head_hash = B256::repeat_byte(0xaa);
        (101, 100, true, head_hash, head_hash, 121_000, 1)
    }

    #[test]
    fn bid_block_structure_accepts_a_well_formed_submission() {
        let (n, head, inturn, bid_parent, head_hash, gas, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(n, head, inturn, bid_parent, head_hash, gas, txs),
            Ok(())
        );
    }

    #[test]
    fn bid_block_structure_rejects_stale_block_number() {
        // TC-009 sub-case 1: number == head, so it can never be the next block. Rejected before the
        // in-turn check, which is why `probeInTurn` can run against any head.
        let (_, head, inturn, bid_parent, head_hash, gas, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(head, head, inturn, bid_parent, head_hash, gas, txs),
            Err(super::BidBlockStructuralRejection::StaleNumber)
        );
        // Well below the head too, not just exactly at it.
        assert_eq!(
            super::validate_bid_block_structure(1, head, inturn, bid_parent, head_hash, gas, txs),
            Err(super::BidBlockStructuralRejection::StaleNumber)
        );
    }

    #[test]
    fn bid_block_structure_rejects_future_block_number() {
        // TC-009 sub-case 2: head + 2. Distinct from stale so builders can tell "you are behind"
        // from "you are ahead" — the messages differ and both map to -38001.
        let (_, head, inturn, bid_parent, head_hash, gas, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(
                head + 2,
                head,
                inturn,
                bid_parent,
                head_hash,
                gas,
                txs
            ),
            Err(super::BidBlockStructuralRejection::FutureNumber)
        );
    }

    #[test]
    fn bid_block_structure_rejects_when_not_in_turn() {
        // TC-009 sub-case 3: correct height, aligned parent, valid body — only the turn is wrong,
        // and it must surface as -38005 rather than collapsing into a generic -38001.
        let (n, head, _, bid_parent, head_hash, gas, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(n, head, false, bid_parent, head_hash, gas, txs),
            Err(super::BidBlockStructuralRejection::NotInTurn)
        );
    }

    #[test]
    fn bid_block_structure_rejects_non_aligned_parent_hash() {
        // TC-009 sub-case 4: builds on something that is not the current head.
        let (n, head, inturn, _, head_hash, gas, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(
                n,
                head,
                inturn,
                B256::repeat_byte(0xde),
                head_hash,
                gas,
                txs
            ),
            Err(super::BidBlockStructuralRejection::NonAlignedParent)
        );
    }

    #[test]
    fn bid_block_structure_rejects_empty_transactions() {
        // TC-009 sub-case 5.
        let (n, head, inturn, bid_parent, head_hash, gas, _) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(n, head, inturn, bid_parent, head_hash, gas, 0),
            Err(super::BidBlockStructuralRejection::EmptyTransactions)
        );
    }

    #[test]
    fn bid_block_structure_rejects_empty_gas_used() {
        // Not in TC-009's list but validated by go-bsc and by node-deploy-bsc's `zero-gas-used`
        // case, and it runs before the empty-transactions check.
        let (n, head, inturn, bid_parent, head_hash, _, txs) = valid_structure();
        assert_eq!(
            super::validate_bid_block_structure(n, head, inturn, bid_parent, head_hash, 0, txs),
            Err(super::BidBlockStructuralRejection::EmptyGasUsed)
        );
    }

    #[test]
    fn bid_block_structure_check_order_matches_geth() {
        use super::BidBlockStructuralRejection as R;
        let head_hash = B256::repeat_byte(0xaa);
        let bad_parent = B256::repeat_byte(0xde);

        // Order is load-bearing, not cosmetic. `node-deploy-bsc`'s `probeInTurn` submits a
        // parent-mismatched bid precisely to read the in-turn answer out of the response: a bid at
        // the right height that is *not* in turn must report -38005, while the same bid *in* turn
        // must report the parent mismatch. Swap those two checks and the probe silently inverts.
        assert_eq!(
            super::validate_bid_block_structure(101, 100, false, bad_parent, head_hash, 121_000, 1),
            Err(R::NotInTurn),
            "in-turn must be checked before parent alignment"
        );
        assert_eq!(
            super::validate_bid_block_structure(101, 100, true, bad_parent, head_hash, 121_000, 1),
            Err(R::NonAlignedParent)
        );

        // Number precedes in-turn, so a stale/future bid is rejected the same way regardless of
        // whose turn it is — that is what lets the probe run against any head without waiting.
        for inturn in [true, false] {
            assert_eq!(
                super::validate_bid_block_structure(
                    100, 100, inturn, bad_parent, head_hash, 0, 0
                ),
                Err(R::StaleNumber),
                "number must be checked before everything else"
            );
        }

        // Parent alignment precedes the body checks, so a misaligned bid with an empty body still
        // reports the parent mismatch.
        assert_eq!(
            super::validate_bid_block_structure(101, 100, true, bad_parent, head_hash, 0, 0),
            Err(R::NonAlignedParent),
            "parent alignment must be checked before gasUsed/transactions"
        );
    }

    #[test]
    fn bid_error_codes_match_geth() {
        // go-bsc core/types/bid_error.go reserves the -38001..-38008 range; builders parse it.
        assert_eq!(MevApiImpl::mev_not_running().code(), -38003);
        assert_eq!(MevApiImpl::mev_not_in_turn().code(), -38005);
        assert_eq!(MevApiImpl::invalid_bid("x").code(), -38001);
        assert_eq!(MevApiImpl::permission_revoked("x").code(), -38006);
        // Regression guard: these two used to collapse into -38001 (generic invalid-bid), so a
        // builder keying retry/backoff behavior on the specific geth code would misbehave.
        assert_eq!(MevApiImpl::pre_seal_verify_failed("x").code(), -38007);
        assert_eq!(MevApiImpl::too_late("x").code(), -38008);
        assert_eq!(
            MevApiImpl::mev_not_running().message(),
            "the validator stop accepting bids for now, try again later"
        );
    }

    #[test]
    fn bid_must_before_matches_geth_formula() {
        // parent at second 1_000, no sub-second component, 3s block interval, 15ms delay left
        // over: bidutil.BidMustBefore = 1_000_000 + 3_000 - 15 = 1_002_985.
        assert_eq!(super::bid_must_before_ms(1_000_000, 3_000, 15), 1_002_985);
    }

    #[test]
    fn bid_must_before_uses_full_millisecond_parent_timestamp() {
        // A parent with a nonzero sub-second component (carried in mixHash post-Lorentz) must
        // shift the deadline forward by that amount, not be truncated away.
        let parent_ms_no_subsecond = 1_000_000u64;
        let parent_ms_with_subsecond = 1_000_400u64; // .400s into the block

        let deadline_a = super::bid_must_before_ms(parent_ms_no_subsecond, 450, 15);
        let deadline_b = super::bid_must_before_ms(parent_ms_with_subsecond, 450, 15);

        assert_eq!(deadline_b - deadline_a, 400);
    }

    #[test]
    fn bid_must_before_does_not_reject_everything_on_sub_second_intervals() {
        // Regression guard for the original bug: dropping the parent's millisecond component and
        // subtracting `no_interrupt_left_over` (500ms) instead of `delay_left_over` (15ms) made
        // the deadline land at-or-before the parent's own timestamp on Fermi's 450ms interval,
        // rejecting every bid as "too late" the instant the parent block appeared.
        let parent_timestamp_ms =
            crate::consensus::parlia::util::calculate_millisecond_timestamp(&alloy_consensus::Header {
                timestamp: 1_700_000_000,
                ..Default::default()
            });
        let fermi_block_interval_ms = 450;
        let delay_left_over_ms = 15;

        let deadline = super::bid_must_before_ms(
            parent_timestamp_ms,
            fermi_block_interval_ms,
            delay_left_over_ms,
        );

        // The deadline must fall strictly after the parent's own timestamp, leaving a real
        // admission window instead of being already in the past.
        assert!(deadline > parent_timestamp_ms as u128);
        assert_eq!(deadline, parent_timestamp_ms as u128 + fermi_block_interval_ms as u128 - 15);
    }

    #[test]
    fn unix_secs_to_rfc3339_matches_known_dates() {
        // 2024-01-15T10:30:04Z
        assert_eq!(super::unix_secs_to_rfc3339_utc(1_705_314_604), "2024-01-15T10:30:04Z");
        // The Unix epoch itself.
        assert_eq!(super::unix_secs_to_rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2000-02-29 exercises the leap-year branch of civil_from_days.
        assert_eq!(super::unix_secs_to_rfc3339_utc(951_782_400), "2000-02-29T00:00:00Z");
        // 2024-12-31T23:59:59Z, the last second of a leap year.
        assert_eq!(super::unix_secs_to_rfc3339_utc(1_735_689_599), "2024-12-31T23:59:59Z");
    }

    #[test]
    fn bid_block_permission_result_omits_details_when_allowed() {
        use crate::node::miner::bid_block_permission::BidBlockPermissionStatus;

        let result: BidBlockPermissionResult =
            BidBlockPermissionStatus { allowed: true, ..Default::default() }.into();
        let json = serde_json::to_value(&result).unwrap();

        assert_eq!(json["allowed"], true);
        // go-bsc's `omitempty` tags drop these entirely when allowed; a builder-facing client
        // must not see stale/zeroed detail fields for the common "not revoked" case.
        assert!(json.get("reason").is_none());
        assert!(json.get("blockHash").is_none());
        assert!(json.get("blockNumber").is_none());
        assert!(json.get("revokedAt").is_none());
        assert!(json.get("resetAt").is_none());
    }

    #[test]
    fn bid_block_permission_result_matches_geth_wire_shape_when_revoked() {
        use crate::node::miner::bid_block_permission::BidBlockPermissionStatus;

        let status = BidBlockPermissionStatus {
            allowed: false,
            reason: "InsertChain err: state root mismatch".to_string(),
            block_hash: B256::repeat_byte(0xab),
            block_num: 100,
            revoked_at: 1_705_314_604,
            reset_at: 1_705_401_004,
        };
        let json = serde_json::to_value(BidBlockPermissionResult::from(status)).unwrap();

        assert_eq!(json["allowed"], false);
        assert_eq!(json["reason"], "InsertChain err: state root mismatch");
        assert_eq!(json["blockHash"], format!("{:#x}", B256::repeat_byte(0xab)));
        // Hex-quantity, matching go-bsc's hexutil.Uint64 — not a plain JSON number.
        assert_eq!(json["blockNumber"], "0x64");
        assert_eq!(json["revokedAt"], "2024-01-15T10:30:04Z");
        assert_eq!(json["resetAt"], "2024-01-16T10:30:04Z");
    }
}
