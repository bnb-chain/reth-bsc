use jsonrpsee::core::RpcResult;
use alloy_primitives::B256;
use alloy_consensus::Transaction;
use reth_primitives::TransactionSigned;
use reth_primitives_traits::SignerRecoverable;
use std::sync::Arc;
use crate::node::miner::bid_simulator::{Bid, NewBidPackage};
use reth_provider::StateProviderFactory;
use crate::chainspec::BscChainSpec;
use reth_chainspec::EthChainSpec;
use tracing::debug;
use crate::node::miner::bsc_miner::BscMiner;
// Use the MEV server trait and types from reth-rpc-api
pub use reth_rpc_api::MevFullApiServer;
pub use reth_rpc_api::mev::{BidArgs, RawBid};
pub use alloy_rpc_types_mev::{EthBundleHash, MevSendBundle, SimBundleOverrides, SimBundleResponse};


const PAY_BID_TX_GAS_LIMIT: u64 = 25000;

/// Implementation of the MEV Builder RPC API

pub struct MevApiImpl<Pool, Client> 
where
    Client: StateProviderFactory + Clone + Send + Sync + 'static,
{
    miner: Arc<BscMiner<Pool, Client>>,

}

impl<Pool, Client> MevApiImpl<Pool, Client>
where
    Client: reth_provider::HeaderProvider<Header = alloy_consensus::Header>
        + reth_provider::BlockNumReader
        + reth_provider::StateProviderFactory
        + reth_provider::CanonStateSubscriptions
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: reth::transaction_pool::TransactionPool<Transaction: reth::transaction_pool::PoolTransaction<Consensus = TransactionSigned>> + Clone + 'static,
{
    /// Create a new MEV API instance
    pub fn new(
        miner: Arc<BscMiner<Pool, Client>>,
    ) -> Self {
        Self { miner }
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
        tx.recover_signer()
            .map_err(|e| format!("Failed to recover transaction signer: {}", e))?;

        Ok(tx)
    }

    /// Convert BidArgs to Bid object
    /// This matches the Go implementation: BidArgs.ToBid()
    fn to_bid(
        bid_args: &BidArgs,
        builder: alloy_primitives::Address,
        chain_spec: &BscChainSpec,
        bid_hash: B256,
    ) -> Result<Bid, String> {
        // 1. Decode transactions from RawBid
        let mut txs = Vec::new();
        for tx_bytes in &bid_args.raw_bid.txs {
            let tx = Self::parse_transaction(tx_bytes, chain_spec)?;
            txs.push(tx);
        }

        // 2. Validate UnRevertible count
        if bid_args.raw_bid.un_revertible.len() > txs.len() {
            return Err(format!(
                "expect UnRevertible no more than {}, got {}",
                txs.len(),
                bid_args.raw_bid.un_revertible.len()
            ));
        }

        // 3. Create UnRevertible hash set (stored in Bid for later use)
        // Note: In Rust we'll store it as a Vec in the Bid struct
        // The Go version uses mapset, but Vec is sufficient for our needs

        // 4. Handle PayBidTx if present
        if !bid_args.pay_bid_tx.is_empty() {
            let pay_bid_tx = Self::parse_transaction(&bid_args.pay_bid_tx, chain_spec)
                .map_err(|e| format!("Failed to parse PayBidTx: {}", e))?;
            txs.push(pay_bid_tx);
        }

        // 5. Create Bid object
        let bid = Bid {
            builder,
            block_number: bid_args.raw_bid.block_number.to(),
            parent_hash: bid_args.raw_bid.parent_hash,
            txs,
            gas_used: bid_args.raw_bid.gas_used.to(),
            gas_fee: bid_args.raw_bid.gas_fee,
            builder_fee: bid_args.raw_bid.builder_fee,
            committed: false,
            bid_hash: bid_hash,
        };

        Ok(bid)
    }

    /// Calculate RawBid hash
    /// This matches the Go implementation: rlpHash(RawBid)
    fn calculate_raw_bid_hash(raw_bid: &RawBid) -> B256 {
        use alloy_primitives::keccak256;
        use alloy_rlp::{Encodable};
        
        // RLP encode the RawBid structure
        // The structure is: [blockNumber, parentHash, txs, unRevertible, gasUsed, gasFee, builderFee]
        let mut rlp_buffer = Vec::new();
        
        // First calculate the length of all encoded items
        let payload_length = raw_bid.block_number.length()
            + raw_bid.parent_hash.length()
            + raw_bid.txs.length()
            + raw_bid.un_revertible.length()
            + raw_bid.gas_used.length()
            + raw_bid.gas_fee.length()
            + raw_bid.builder_fee.length();
        
        // Encode the list header
        alloy_rlp::Header {
            list: true,
            payload_length,
        }
        .encode(&mut rlp_buffer);
        
        // Encode each field
        raw_bid.block_number.encode(&mut rlp_buffer);
        raw_bid.parent_hash.encode(&mut rlp_buffer);
        raw_bid.txs.encode(&mut rlp_buffer);
        raw_bid.un_revertible.encode(&mut rlp_buffer);
        raw_bid.gas_used.encode(&mut rlp_buffer);
        raw_bid.gas_fee.encode(&mut rlp_buffer);
        raw_bid.builder_fee.encode(&mut rlp_buffer);
        
        // Calculate keccak256 hash
        let hash = keccak256(&rlp_buffer);
        debug!("RawBid RLP encoded length: {}, hash: {:?}", rlp_buffer.len(), hash);
        hash
    }

    /// Recover builder address from signature
    fn recover_builder_address(raw_bid: &RawBid, signature: &alloy_primitives::Bytes) -> Result<alloy_primitives::Address, String> {
        use secp256k1::{Message, Secp256k1};
        use alloy_primitives::keccak256;
        
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
        let recovery_id_value = if recovery_id >= 27 {
            recovery_id - 27
        } else {
            recovery_id
        };
        
        // Create RecoveryId from i32
        let recovery_id = secp256k1::ecdsa::RecoveryId::try_from(i32::from(recovery_id_value))
            .map_err(|e| format!("Invalid recovery id: {:?}", e))?;
        
        let sig_bytes = &signature[..64];
        let recoverable_sig = secp256k1::ecdsa::RecoverableSignature::from_compact(sig_bytes, recovery_id)
            .map_err(|e| format!("Failed to parse signature: {}", e))?;
        
        // Recover public key
        let secp = Secp256k1::new();
        let public_key = secp.recover_ecdsa(&message, &recoverable_sig)
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
impl<Pool, Client> MevFullApiServer for MevApiImpl<Pool, Client>
where
    Client: reth_provider::HeaderProvider<Header = alloy_consensus::Header>
        + reth_provider::BlockNumReader
        + reth_provider::StateProviderFactory
        + reth_provider::CanonStateSubscriptions
        + Clone
        + Send
        + Sync
        + 'static,
    Pool: reth::transaction_pool::TransactionPool<Transaction: reth::transaction_pool::PoolTransaction<Consensus = TransactionSigned>> + Clone + 'static,
{
    /// Send a bundle to the relay (not implemented for BSC)
    async fn send_bundle(
        &self,
        _request: MevSendBundle,
    ) -> RpcResult<EthBundleHash> {
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "Method not supported",
            Some("sendBundle is not supported on BSC"),
        ))
    }

    /// Simulate a bundle (not implemented for BSC)
    async fn sim_bundle(
        &self,
        _bundle: MevSendBundle,
        _sim_overrides: SimBundleOverrides,
    ) -> RpcResult<SimBundleResponse> {
        Err(jsonrpsee::types::ErrorObject::owned(
            -32601,
            "Method not supported",
            Some("simBundle is not supported on BSC"),
        ))
    }

    /// Send a bid to the builder
    /// Returns the bid hash
    async fn send_bid(&self, bid: BidArgs) -> RpcResult<B256> {
        // todo: check mev run
        tracing::info!(
            "Received bid for block {} with {} txs",
            bid.raw_bid.block_number,
            bid.raw_bid.txs.len()
        );

        let parent_number = bid.raw_bid.block_number.to();
        let parent_snapshot = match self.miner.snapshot_provider().snapshot(parent_number) {
            Some(snapshot) => snapshot,
            None => {
                tracing::error!("Skip to new bid due to no snapshot available, block number: {}", parent_number);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "No snapshot available",
                    None::<()>,
                ));
            }
        };

        if bid.raw_bid.parent_hash != parent_snapshot.block_hash {
            tracing::error!("Skip to new bid due to block hash mismatch, block number: {}", parent_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Block hash mismatch",
                None::<()>,
            ));
        }

        if let Some(validator_address) = self.miner.mining_config().validator_address {
            if !parent_snapshot.is_inturn(validator_address) {
                tracing::error!("Skip to new bid due to is not inturn, block number: {}", parent_number);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Not inturn",
                    None::<()>,
                ));
            }
        }else {
            tracing::error!("Skip to new bid due to no validator address, block number: {}", parent_number);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "No validator address",
                    None::<()>,
                ));
        }

        if bid.raw_bid.gas_fee == 0 || bid.raw_bid.gas_used ==0{
            tracing::error!("Skip to new bid due to gas fee or gas used is 0, block number: {}", parent_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Gas fee or gas used is 0",
                None::<()>,
            ));
        }

        if bid.raw_bid.builder_fee != 0 {
            let builder_fee = bid.raw_bid.builder_fee;
            if builder_fee < 0 {
                tracing::error!("Skip to new bid due to builder fee is less than 0, block number: {}", parent_number);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Builder fee is less than 0",
                    None::<()>,
                ));
            }

            if builder_fee > bid.raw_bid.gas_fee {
                tracing::error!("Skip to new bid due to builder fee is greater than gas fee, block number: {}", parent_number);
                return Err(jsonrpsee::types::ErrorObject::owned(
                    -32602,
                    "Builder fee is greater than gas fee",
                    None::<()>,
                ));
            }
        }

        if bid.pay_bid_tx.is_empty() || bid.pay_bid_tx_gas_used == 0 {
            tracing::error!("Skip to new bid due to pay bid tx is empty or gas used is 0, block number: {}", parent_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Pay bid tx is empty or gas used is 0",
                None::<()>,
            ));
        }

        if bid.pay_bid_tx_gas_used > PAY_BID_TX_GAS_LIMIT {
            tracing::error!("Skip to new bid due to pay bid tx gas used is greater than limit, block number: {}", parent_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Pay bid tx gas used is greater than limit",
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
        
        // Calculate bid hash (using RLP hash of RawBid)
        let bid_hash = Self::calculate_raw_bid_hash(&bid.raw_bid);
        debug!("bid_hash: {:?}", bid_hash);
        
        // Check if this bid is already pending
        if !self.miner.check_pending_bid(bid.raw_bid.block_number.to(), builder, bid_hash) {
            tracing::error!("Skip to new bid due to pending bid, block number: {}", bid.raw_bid.block_number);
            return Err(jsonrpsee::types::ErrorObject::owned(
                -32602,
                "Pending bid",
                None::<()>,
            ));
        }

        // Convert BidArgs to Bid object
        let bid_obj = match Self::to_bid(&bid, builder, self.miner.chain_spec(), bid_hash) {
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

        // Create bid package
        let bid_package = NewBidPackage {
            bid: bid_obj,
            runtime: 0, // Will be calculated by simulator
            bid_value: 0, // Will be calculated by simulator
        };

        // Log acceptance before async processing
        tracing::info!(
            "Bid accepted for block {}, bid_hash: {:?}",
            bid.raw_bid.block_number,
            bid_hash
        );

        // Submit to miner
        self.miner.send_bid(bid_package);

        Ok(bid_hash)
    }
}