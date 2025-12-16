use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::SnapshotProvider;
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
use reth_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};
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

/// Custom MEV API server trait - only includes send_bid to avoid conflicts with reth's default MEV API
#[rpc(server, namespace = "mev")]
pub trait BscMevApi {
    /// Send a bid to the builder
    #[method(name = "sendBid")]
    async fn send_bid(&self, bid: BidArgs) -> RpcResult<B256>;

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
}

const PAY_BID_TX_GAS_LIMIT: u64 = 25000;

/// Implementation of the MEV Builder RPC API
pub struct MevApiImpl {
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    chain_spec: Arc<BscChainSpec>,
    validator_address: Address,
    validator_commission: u64,
    bid_simulation_left_over: u64, // milliseconds
    no_interrupt_left_over: u64,   // milliseconds
    max_bids_per_builder: u32,
    gas_ceil: u64,
    min_gas_price: U256,
    builder_fee_ceil: U256,
    version: String,
    /// Whitelist of allowed builders (empty HashSet means no builders allowed)
    allowed_builders: Arc<RwLock<HashSet<Address>>>,
}

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
        let max_bids_per_builder = mining_config.get_max_bids_per_builder();
        let builder_fee_ceil = U256::from(mining_config.get_builder_fee_ceil());

        // Version string
        let version = env!("CARGO_PKG_VERSION").to_string();

        // Initialize allowed builders from config
        // If not configured, initialize as empty HashSet (no builders allowed by default)
        let allowed_builders = mining_config
            .allowed_builders
            .map(|addrs| addrs.into_iter().collect::<HashSet<_>>())
            .unwrap_or_default(); // Empty HashSet if not configured

        if allowed_builders.is_empty() {
            tracing::warn!(
                "MEV API initialized with EMPTY builder whitelist - NO builders will be accepted!"
            );
            tracing::warn!(
                "Use mev_addBuilder to add builders or set BSC_ALLOWED_BUILDERS environment variable"
            );
        } else {
            tracing::info!(
                "MEV API initialized with builder whitelist: {} builders",
                allowed_builders.len()
            );
            for builder in &allowed_builders {
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
            max_bids_per_builder,
            gas_ceil,
            min_gas_price,
            builder_fee_ceil,
            version,
            allowed_builders: Arc::new(RwLock::new(allowed_builders)),
        }
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

    /// Get MEV parameters
    /// Returns the current MEV configuration matching geth-bsc implementation
    async fn params(&self) -> RpcResult<MevParams> {
        tracing::debug!("MEV params requested");

        Ok(MevParams {
            validator_commission: self.validator_commission,
            // Convert milliseconds to nanoseconds (1ms = 1,000,000 ns)
            bid_simulation_left_over: self.bid_simulation_left_over * 1_000_000,
            no_interrupt_left_over: self.no_interrupt_left_over * 1_000_000,
            max_bids_per_builder: self.max_bids_per_builder,
            gas_ceil: self.gas_ceil,
            gas_price: self.min_gas_price,
            builder_fee_ceil: self.builder_fee_ceil,
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
}
