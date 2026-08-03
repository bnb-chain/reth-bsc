//! Debug-only builder extraction RPC for BEP-675 end-to-end testing.
//!
//! `debug_buildCandidateBlock` runs the normal block-building pipeline against the current
//! head on behalf of an **arbitrary coinbase** (a validator this node does not hold the key
//! for), with a caller-supplied user-tx list, and returns a state-valid [`BidBlock`]
//! candidate: header with correct state/receipts roots, user txs first, and the trailing
//! system txs re-emitted **unsigned** in go-bsc's wire shape (`v = r = s = 0`) so the target
//! validator can bind-sign them — exactly what a geth builder ships over `mev_sendBidBlock`.
//!
//! Registered only when `BSC_DEBUG_BUILDER=true`; this is a test seam, not a production API.
//! The throwaway signatures the miner-mode executor puts on the generated system txs come
//! from this node's key and are stripped before returning; they never affect the state root
//! because system txs execute with `caller = coinbase` regardless of signature.

use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::bid_block::DEPOSIT_SELECTOR;
use crate::consensus::parlia::util::{
    calculate_difficulty, calculate_millisecond_timestamp, set_millisecond_part_of_timestamp,
};
use crate::hardforks::BscHardforks;
use crate::node::evm::config::{BscEvmConfig, BscNextBlockEnvAttributes};
use crate::node::miner::bid_block::BidBlock;
use alloy_consensus::transaction::SignerRecoverable;
use alloy_consensus::Transaction;
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, Bytes, B256, U256};
use alloy_rlp::Encodable;
use jsonrpsee::core::RpcResult;
use jsonrpsee::proc_macros::rpc;
use reth_ethereum_payload_builder::EthereumBuilderConfig;
use reth_ethereum_primitives::TransactionSigned;
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_primitives_traits::SealedHeader;
use reth_provider::StateProviderFactory;
use reth_revm::{database::StateProviderDatabase, db::State};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Result of `debug_buildCandidateBlock`: the geth-wire candidate plus the values the
/// admission path will derive from it (for driver-side sanity checks).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateBlockResult {
    /// The candidate, serialized with the same wire shape `mev_sendBidBlock` accepts.
    pub bid_block: BidBlock,
    /// Index where the trailing unsigned system-tx region begins.
    pub system_tx_start: usize,
    /// The deposit value (claimed gas fee) the admission path will extract.
    pub gas_fee: U256,
}

#[rpc(server, namespace = "debug")]
pub trait BscDebugBuilderApi {
    /// Build a state-valid candidate block on behalf of `coinbase` from `raw_txs`
    /// (EIP-2718 bytes, executed in order; failures abort). Returns the BidBlock-shaped
    /// candidate with trailing system txs unsigned.
    #[method(name = "buildCandidateBlock")]
    async fn build_candidate_block(
        &self,
        coinbase: Address,
        raw_txs: Vec<Bytes>,
    ) -> RpcResult<CandidateBlockResult>;
}

pub struct DebugBuilderApiImpl<Client> {
    client: Client,
    chain_spec: Arc<BscChainSpec>,
}

impl<Client> DebugBuilderApiImpl<Client> {
    pub fn new(client: Client, chain_spec: Arc<BscChainSpec>) -> Self {
        Self { client, chain_spec }
    }
}

fn internal_err(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
    jsonrpsee::types::ErrorObject::owned(-32603, msg.into(), None::<()>)
}

fn invalid_params(msg: impl Into<String>) -> jsonrpsee::types::ErrorObjectOwned {
    jsonrpsee::types::ErrorObject::owned(-32602, msg.into(), None::<()>)
}

/// Re-encode a generated (node-key-signed) legacy system tx in go-bsc's unsigned wire shape:
/// the tx body fields followed by literal `V = R = S = 0`. Mirrors go-bsc's
/// `types.NewTransaction` encoding for BidBlock trailing system txs.
fn encode_unsigned_legacy(tx: &TransactionSigned) -> Result<Bytes, String> {
    let legacy = match tx.clone().into_typed_transaction() {
        alloy_consensus::EthereumTypedTransaction::Legacy(legacy) => legacy,
        _ => return Err("generated system tx is not a legacy tx".to_string()),
    };
    let mut payload = Vec::new();
    legacy.nonce.encode(&mut payload);
    legacy.gas_price.encode(&mut payload);
    legacy.gas_limit.encode(&mut payload);
    legacy.to.encode(&mut payload);
    legacy.value.encode(&mut payload);
    legacy.input.encode(&mut payload);
    0u8.encode(&mut payload); // v
    0u8.encode(&mut payload); // r
    0u8.encode(&mut payload); // s
    let mut out = Vec::new();
    alloy_rlp::Header { list: true, payload_length: payload.len() }.encode(&mut out);
    out.extend_from_slice(&payload);
    Ok(Bytes::from(out))
}

#[async_trait::async_trait]
impl<Client> BscDebugBuilderApiServer for DebugBuilderApiImpl<Client>
where
    Client: StateProviderFactory + Clone + Send + Sync + 'static,
{
    async fn build_candidate_block(
        &self,
        coinbase: Address,
        raw_txs: Vec<Bytes>,
    ) -> RpcResult<CandidateBlockResult> {
        let client = self.client.clone();
        let chain_spec = self.chain_spec.clone();

        // Block building + state root are CPU-bound; keep them off the async executor.
        tokio::task::spawn_blocking(move || {
            build_candidate(client, chain_spec, coinbase, raw_txs)
        })
        .await
        .map_err(|e| internal_err(format!("build task panicked: {e}")))?
    }
}

fn build_candidate<Client>(
    client: Client,
    chain_spec: Arc<BscChainSpec>,
    coinbase: Address,
    raw_txs: Vec<Bytes>,
) -> RpcResult<CandidateBlockResult>
where
    Client: StateProviderFactory,
{
    // Parent = current canonical head (the same header admission will check against).
    let head_number = crate::shared::get_best_canonical_block_number()
        .ok_or_else(|| internal_err("chain head unavailable"))?;
    let parent_header = crate::shared::get_canonical_header_by_number_from_provider(head_number)
        .ok_or_else(|| internal_err("chain head header unavailable"))?;
    let parent_hash = parent_header.hash_slow();
    let parent = SealedHeader::new(parent_header.clone(), parent_hash);

    let snapshot_provider = crate::shared::get_snapshot_provider()
        .ok_or_else(|| internal_err("snapshot provider unavailable"))?;
    let snapshot = snapshot_provider
        .snapshot_by_hash(&parent_hash)
        .ok_or_else(|| internal_err("no snapshot for head"))?;

    // Same schedule the validator plans: parent milli-timestamp + block interval.
    let child_milli = calculate_millisecond_timestamp(&parent_header) + snapshot.block_interval;
    let timestamp = child_milli / 1000;

    // Same gas limit the admission path expects (mev.rs admit_bid_block).
    let gas_ceil = crate::shared::get_miner_gas_limit().unwrap_or(parent_header.gas_limit);
    let gas_limit =
        EthereumBuilderConfig::new().with_gas_limit(gas_ceil).gas_limit(parent_header.gas_limit);

    let difficulty = calculate_difficulty(&snapshot, coinbase);

    let user_txs: Vec<TransactionSigned> = raw_txs
        .iter()
        .enumerate()
        .map(|(i, b)| {
            TransactionSigned::decode_2718(&mut b.as_ref())
                .map_err(|e| invalid_params(format!("raw tx {i} decode failed: {e}")))
        })
        .collect::<Result<_, _>>()?;
    let user_tx_count = user_txs.len();

    let state_provider = client
        .state_by_block_hash(parent_hash)
        .map_err(|e| internal_err(format!("state provider: {e}")))?;
    let sp_db = StateProviderDatabase::new(&state_provider);
    let mut db = State::builder().with_database(sp_db).with_bundle_update().build();

    let evm_config = BscEvmConfig::new(chain_spec.clone());
    let parent_beacon_block_root = BscHardforks::is_bohr_active_at_timestamp(
        chain_spec.as_ref(),
        parent_header.number + 1,
        timestamp,
    )
    .then(B256::default);

    let mut builder = evm_config
        .builder_for_next_block(
            &mut db,
            &parent,
            BscNextBlockEnvAttributes {
                inner: NextBlockEnvAttributes {
                    timestamp,
                    suggested_fee_recipient: coinbase,
                    prev_randao: difficulty.into(),
                    gas_limit,
                    parent_beacon_block_root,
                    withdrawals: None,
                    extra_data: Default::default(),
                    slot_number: None,
                },
                validator_cache_sink: None,
                turn_length_sink: None,
                state_root_precomputed_sink: None,
                trie_handle: None,
                state_root_deadline_ms: None,
            },
        )
        .map_err(|e| internal_err(format!("builder_for_next_block: {e}")))?;

    builder
        .apply_pre_execution_changes()
        .map_err(|e| internal_err(format!("pre-execution: {e}")))?;

    for (i, tx) in user_txs.into_iter().enumerate() {
        let recovered = tx
            .try_into_recovered()
            .map_err(|e| invalid_params(format!("raw tx {i} sender recovery failed: {e}")))?;
        builder
            .execute_transaction(recovered)
            .map_err(|e| invalid_params(format!("raw tx {i} execution failed: {e}")))?;
    }

    // finish() executes the trailing system txs (deposit with value = accrued
    // SYSTEM_ADDRESS fees) and computes state root / receipts root / bloom / gasUsed.
    let out = builder
        .finish(&state_provider, None)
        .map_err(|e| internal_err(format!("finish: {e}")))?;
    let BlockBuilderOutcome { block, .. } = out;

    let mut header = block.clone_header();
    // The assembler leaves prev_randao in mix_hash and env difficulty in the header; a real
    // geth builder ships the millisecond timestamp in mixHash (SetMilliseconds) and the
    // in-turn/no-turn difficulty — the validator's pre-seal cascading checks (Ramanujan lower
    // timestamp bound, WrongDifficulty) read both. Neither affects the executed state.
    set_millisecond_part_of_timestamp(child_milli, &mut header);
    header.difficulty = difficulty;

    let body_txs: Vec<TransactionSigned> = block.body().transactions().cloned().collect();
    if body_txs.len() < user_tx_count {
        return Err(internal_err("built block lost user txs"));
    }

    let mut transactions: Vec<Bytes> = Vec::with_capacity(body_txs.len());
    for tx in &body_txs[..user_tx_count] {
        transactions.push(Bytes::from(tx.encoded_2718()));
    }
    for tx in &body_txs[user_tx_count..] {
        transactions.push(encode_unsigned_legacy(tx).map_err(internal_err)?);
    }

    // Re-root the header over the transactions actually being returned. The built block's root
    // commits to the *signed* system txs, but the body above re-emits them unsigned, so shipping
    // the sealed root would produce a candidate whose header does not commit to its own body — a
    // BidBlock no real builder can produce, and one `verify_bid_block_payload` rejects outright
    // (`invalid tx root`) now that the signature covers only the header (bsc #3742).
    //
    // Safe for the state root this seam exists to exercise: the validator recomputes
    // `transactions_root` in `simulate_bid_block` after bind-signing, so the sealed header it
    // proposes is unaffected by what we put here.
    header.transactions_root =
        crate::node::miner::bid_block::submitted_tx_root(&transactions);

    // The first generated system tx must be the deposit carrying the claimed gas fee.
    let gas_fee = match body_txs.get(user_tx_count) {
        Some(tx) if tx.input().starts_with(&DEPOSIT_SELECTOR) => tx.value(),
        _ => U256::ZERO,
    };

    Ok(CandidateBlockResult {
        bid_block: BidBlock { header, transactions, sidecars: Vec::new() },
        system_tx_start: user_tx_count,
        gas_fee,
    })
}
