use std::collections::HashSet;
use std::sync::{Arc, LazyLock, Mutex, atomic::{AtomicBool, AtomicU64, Ordering}};

use alloy_consensus::{Header, BlockHeader};
use alloy_primitives::B256;

use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::{bls_signer, provider::SnapshotProvider, vote::VoteData, votes, VoteAddress};
use crate::hardforks::BscHardforks;
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::node::evm::util::get_cannonical_header_from_cache;
use crate::node::vote_journal;

/// Number of blocks to wait after mining becomes enabled before producing votes.
/// This mirrors geth's VoteManager warm-up (blocksNumberSinceMining = 40) to avoid
/// double-voting when switching between primary/backup validators.
const DEFAULT_WARMUP_BLOCKS: u64 = 40;
/// Extra safety margin for network propagation of votes.
const TIME_FOR_BROADCAST_MS: u64 = 50; // Similar to geth

/// Dedup recently produced votes by target hash to avoid re-signing/spam.
static PRODUCED_TARGETS: LazyLock<Mutex<schnellru::LruMap<B256, (), schnellru::ByLength>>> =
    LazyLock::new(|| Mutex::new(schnellru::LruMap::new(schnellru::ByLength::new(2048))));

static START_VOTE: AtomicBool = AtomicBool::new(true);
static BLOCKS_SINCE_MINING: AtomicU64 = AtomicU64::new(0);
static VOTEADDR_INDEX: LazyLock<Mutex<schnellru::LruMap<B256, HashSet<VoteAddress>, schnellru::ByLength>>> =
    LazyLock::new(|| Mutex::new(schnellru::LruMap::new(schnellru::ByLength::new(1024))));

/// Control vote production during sync similar to geth's Start/Done/Failed events.
pub fn set_downloader_active(active: bool) {
    // When downloader is active, pause voting; resume when it's done/failed
    START_VOTE.store(!active, Ordering::Relaxed);
}

fn warmup_blocks() -> u64 {
    std::env::var("BSC_VOTE_WARMUP_BLOCKS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&v| v > 0)
        .unwrap_or(DEFAULT_WARMUP_BLOCKS)
}

/// Attempt to produce and broadcast a vote for the given canonical head.
///
/// Safety checks:
/// - Luban active at this block
/// - Global BLS signer initialized
/// - Local BLS public key is present in validator set for parent snapshot and non-zero
/// - Not already produced for this target hash (LRU)
pub fn maybe_produce_and_broadcast_for_head(
    chain_spec: Arc<BscChainSpec>,
    snapshot_provider: &dyn SnapshotProvider,
    head: &Header,
) {
    // Only vote when mining is enabled (similar to geth behavior)
    let mining_enabled = crate::node::miner::config::get_global_mining_config()
        .map(|c| c.is_mining_enabled())
        .unwrap_or(false);
    if !mining_enabled {
        BLOCKS_SINCE_MINING.store(0, Ordering::Relaxed);
        tracing::debug!(target: "bsc::vote", reason = "mining-disabled", "skip vote production");
        return;
    }

    // Respect downloader gating
    if !START_VOTE.load(Ordering::Relaxed) {
        tracing::debug!(target: "bsc::vote", reason = "downloader-active", "skip vote production");
        return;
    }
    // Only vote post-Luban
    if !chain_spec.is_luban_active_at_block(head.number()) {
        tracing::debug!(target: "bsc::vote", reason = "pre-luban", block=head.number(), "skip vote production");
        return;
    }

    // Require signer
    if !bls_signer::is_bls_signer_initialized() {
        tracing::debug!(target: "bsc::vote", reason = "bls-not-initialized", "skip vote production");
        return;
    }

    // Target block is the current head (parent of next block to be attested)
    let target_number = head.number();
    let target_hash = head.hash_slow();

    // LRU de-dup on target hash
    {
        let guard = PRODUCED_TARGETS.lock();
        match guard {
            Ok(mut lru) => {
                if lru.get(&target_hash).is_some() {
                    tracing::debug!(target: "bsc::vote", reason = "dup-target", target_hash=%format!("0x{:x}", target_hash), "skip vote production");
                    return;
                }
                lru.insert(target_hash, ());
            }
            Err(e) => {
                tracing::error!(target: "bsc::vote", error=%e, "LRU lock poisoned (produced_targets)");
                return;
            }
        }
    }

    // Enforce warm-up delay since mining started
    let threshold = warmup_blocks();
    let cnt = BLOCKS_SINCE_MINING.fetch_add(1, Ordering::Relaxed) + 1;
    if cnt <= threshold {
        tracing::debug!(target: "bsc::vote", reason = "warmup", count=cnt, threshold=threshold, "skip vote production");
        return;
    }
    // Clamp to threshold to avoid unbounded growth
    BLOCKS_SINCE_MINING.store(threshold, Ordering::Relaxed);

    // Use current header's snapshot to determine source (last justified)
    let snap = match snapshot_provider.snapshot_by_hash(&target_hash) {
        Some(s) => s,
        None => {
            tracing::debug!(target: "bsc::vote", reason = "missing-snapshot", target_hash=%format!("0x{:x}", target_hash), "skip vote production");
            return;
        }
    };

    // Verify local validator is part of current validator set and has a non-zero vote address
    let my_vote_addr = match bls_signer::global_bls_public_key() {
        Ok(pk) => pk,
        Err(_) => return,
    };

    // O(1) membership via per-snapshot vote address index cached in LRU
    let guard = VOTEADDR_INDEX.lock();
    let set = match guard {
        Ok(mut g) => {
            if let Some(s) = g.get(&snap.block_hash) {
                s.clone()
            } else {
                let mut hs = HashSet::with_capacity(snap.validators_map.len());
                for info in snap.validators_map.values() {
                    if info.vote_addr != VoteAddress::ZERO {
                        hs.insert(info.vote_addr);
                    }
                }
                g.insert(snap.block_hash, hs.clone());
                hs
            }
        }
        Err(e) => {
            tracing::error!(target: "bsc::vote", error=%e, "LRU lock poisoned (voteaddr index)");
            return;
        }
    };

    let is_validator = set.contains(&my_vote_addr);
    drop(set);
    if !is_validator {
        tracing::debug!(target: "bsc::vote", reason = "not-validator", "skip vote production");
        return;
    }

    // Too-late-to-vote guard: ensure we have time to broadcast before next block assembly
    let cur_ms = calculate_millisecond_timestamp(head);
    let vote_assemble_ms = cur_ms.saturating_add(snap.block_interval);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(u64::MAX);
    if now_ms.saturating_add(TIME_FOR_BROADCAST_MS) > vote_assemble_ms {
        tracing::debug!(target: "bsc::vote", reason = "too-late", now_ms=now_ms, vote_assemble_ms=vote_assemble_ms, "skip vote production");
        return;
    }
    // Compose vote data: source = last justified from snapshot; target = head.hash/number
    let mut source_number = snap.vote_data.target_number;
    let mut source_hash = snap.vote_data.target_hash;

    // if source_hash is zero, it loads genesis hash as source.
    // once one attestation generated, attestation of snap would not be nil forever basically
    // ref: https://github.com/bnb-chain/bsc/blob/583cfec3ea811fb124e6812aabd190555d5aeabc/consensus/parlia/parlia.go#L2161
    if source_hash == B256::ZERO {
        match get_cannonical_header_from_cache(0) {
            Some(genesis_header) => {
                source_number = genesis_header.number();
                source_hash = genesis_header.hash_slow();
            }
            None => {
                tracing::debug!(target: "bsc::vote", reason = "zero-source", "skip vote production, genesis header not found");
                return;
            }
        }
    }

    // Check vote rules against local journal to prevent slashing and double votes.
    if !vote_journal::under_rules(source_number, target_number) {
        tracing::debug!(target: "bsc::vote", reason = "under-rules-failed", source_number=source_number, target_number=target_number, "skip vote production");
        return;
    }

    let data = VoteData { 
        source_number, 
        source_hash, 
        target_number, 
        target_hash 
    };

    // Sign and insert/broadcast
    match bls_signer::sign_vote_with_global(data) {
        Ok(envelope) => {
            // Persist in journal first to avoid vote loss due to failures.
            if let Err(e) = vote_journal::persist_vote(&envelope) {
                tracing::error!(target: "bsc::vote", error=%e, "Failed to write vote into journal");
                // Continue despite journal error; do not halt voting pipeline.
            }
            // insert into local pool
            tracing::debug!(target: "bsc::vote", "insert self vote into local pool, target_number: {}, target_hash: {}", data.target_number, data.target_hash);
            votes::put_vote(envelope.clone());
            // broadcast to peers
            crate::node::network::bsc_protocol::registry::broadcast_votes(vec![envelope]);
        }
        Err(e) => {
            tracing::warn!(target: "bsc::vote", error=%e, "Failed to sign vote");
        }
    }
}
