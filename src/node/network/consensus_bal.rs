//! Bounded retention of full BAL attachments. A commitment match is not execution validation.

use alloy_consensus::Header;
use alloy_eips::eip7928::{bal::Bal, BlockAccessList};
use alloy_primitives::{keccak256, Bytes, B256};
use parking_lot::Mutex;
use reth_network_api::Peers;
use std::{
    collections::VecDeque,
    sync::{LazyLock, OnceLock},
};

// Local retention limits, not consensus limits. Eviction always permits sequential execution.
const CACHE_BYTES: usize = 128 * 1024 * 1024;
const CACHE_ENTRIES: usize = 128;

/// Prototype handshake marker in the existing BSC capability packet's opaque extension.
pub(crate) const CAPABILITY: &[u8] = b"bep732-bal/1";

#[derive(Default)]
struct BalCache {
    entries: VecDeque<(B256, Bytes)>,
    bytes: usize,
}

impl BalCache {
    fn insert(&mut self, hash: B256, raw: Bytes) {
        if raw.len() > CACHE_BYTES || self.entries.iter().any(|(key, _)| *key == hash) {
            return;
        }
        while self.entries.len() >= CACHE_ENTRIES || self.bytes + raw.len() > CACHE_BYTES {
            if let Some((_, old)) = self.entries.pop_front() {
                self.bytes -= old.len();
            }
        }
        self.bytes += raw.len();
        self.entries.push_back((hash, raw));
    }
}

static STORE: OnceLock<reth_provider::BalStoreHandle> = OnceLock::new();

/// Read executed BALs from the provider's canonical persistence lifecycle.
pub(crate) fn set_store(store: reth_provider::BalStoreHandle) {
    if STORE.set(store).is_err() {
        tracing::warn!(target: "bsc::bal", "Consensus BAL store already installed");
    }
}

static CACHE: LazyLock<Mutex<BalCache>> = LazyLock::new(|| Mutex::new(BalCache::default()));

/// Retain only canonical, structurally valid bytes matching the sealed block's commitment.
/// Returns whether validation succeeded; valid entries can still exceed the local cache budget.
/// Failure discards the attachment, never the block; execution remains authoritative.
pub(crate) fn retain(header: &Header, transaction_count: usize, raw: Bytes) -> bool {
    if header.block_access_list_hash != Some(keccak256(&raw)) {
        return false;
    }
    let Ok(list) = alloy_rlp::decode_exact::<BlockAccessList>(&raw) else { return false };
    let bal = Bal::from(list);
    if bal.validate_structure(transaction_count).is_err() ||
        bal.validate_gas_limit(header.gas_limit).is_err()
    {
        return false;
    }
    CACHE.lock().insert(header.hash_slow(), raw);
    true
}

/// Retain a locally executed payload in the provider buffer. Only canonical entries are flushed
/// by engine persistence; unexecuted network and BidBlock attachments must use `retain` instead.
pub(crate) fn retain_executed(header: &Header, transaction_count: usize, raw: Bytes) {
    if !retain(header, transaction_count, raw.clone()) {
        tracing::error!(target: "bsc::bal", "Locally built BAL failed retention checks");
        return;
    }
    if let Some(store) = STORE.get() {
        if let Err(error) = store.insert(
            alloy_eips::NumHash::new(header.number, header.hash_slow()),
            reth_provider::RawBal::new(raw),
        ) {
            tracing::warn!(target: "bsc::bal", %error, "Failed to retain locally executed BAL");
        }
    }
}

/// Retrieve a retained attachment for this exact block, including noncanonical branches.
pub(crate) fn get(hash: B256) -> Option<Bytes> {
    let cached =
        CACHE.lock().entries.iter().find(|(key, _)| *key == hash).map(|(_, raw)| raw.clone());
    cached.or_else(|| {
        STORE.get().and_then(|store| match store.get_by_hash(hash) {
            Ok(raw) => raw,
            Err(error) => {
                tracing::warn!(target: "bsc::bal", %error, %hash, "Failed to read retained BAL");
                None
            }
        })
    })
}

/// Fetch from the announcing peer without delaying block execution. The import service owns and
/// bounds these futures; dropping it cancels requests, and the whole operation has a deadline.
pub(crate) async fn fetch_missing(
    network: reth_network::NetworkHandle<super::BscNetworkPrimitives>,
    peer: reth_network_api::PeerId,
    header: Header,
    transaction_count: usize,
) {
    let fetch = async {
        let Ok(Some(info)) = network.get_peer_by_id(peer).await else { return "peer_unavailable" };
        if info.eth_version < reth_eth_wire::EthVersion::Eth71 {
            return "unsupported";
        }
        let (response, receiver) = tokio::sync::oneshot::channel();
        network.send_request(
            peer,
            reth_network_api::PeerRequest::GetBlockAccessLists {
                request: reth_eth_wire::GetBlockAccessLists(vec![header.hash_slow()]),
                response,
            },
        );
        metrics::counter!("bsc_consensus_bal_fetch_sent_total").increment(1);
        match receiver.await {
            Ok(Ok(response)) => retain_response(&header, transaction_count, response),
            _ => "request_failed",
        }
    };
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(2), fetch).await.unwrap_or("timeout");
    metrics::counter!("bsc_consensus_bal_fetch_total", "result" => result).increment(1);
}

fn retain_response(
    header: &Header,
    transaction_count: usize,
    response: reth_eth_wire::BlockAccessLists,
) -> &'static str {
    if response.0.is_empty() {
        return "unavailable";
    }
    if response.0.len() != 1 {
        return "invalid";
    }
    match response.0.into_iter().next().flatten() {
        None => "unavailable",
        Some(raw) => {
            if retain(header, transaction_count, raw) {
                "valid"
            } else {
                "invalid"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BscBlock;

    #[test]
    fn retrieval_checks_cardinality_presence_and_commitment() {
        let header = Header {
            number: 73202,
            block_access_list_hash: Some(keccak256([0xc0])),
            ..Default::default()
        };
        let good = Bytes::from_static(&[0xc0]);
        let bad = Bytes::from_static(&[0x80]);
        for (entries, expected) in [
            (vec![], "unavailable"),
            (vec![None], "unavailable"),
            (vec![Some(good.clone()), None], "invalid"),
            (vec![Some(bad)], "invalid"),
            (vec![Some(good.clone())], "valid"),
        ] {
            assert_eq!(
                retain_response(&header, 0, reth_eth_wire::BlockAccessLists(entries)),
                expected
            );
        }
        assert_eq!(get(header.hash_slow()), Some(good));
    }

    #[test]
    fn cache_evicts_oldest_and_does_not_duplicate() {
        let mut cache = BalCache::default();
        for index in 0..=CACHE_ENTRIES {
            cache.insert(
                B256::from_slice(&alloy_primitives::U256::from(index).to_be_bytes::<32>()),
                Bytes::from_static(&[0xc0]),
            );
        }
        assert_eq!(cache.entries.len(), CACHE_ENTRIES);
        assert_eq!(cache.bytes, CACHE_ENTRIES);
        assert!(!cache.entries.iter().any(|(key, _)| *key == B256::ZERO));
        let last = cache.entries.back().unwrap().0;
        cache.insert(last, Bytes::from_static(&[0xc0]));
        assert_eq!(cache.bytes, CACHE_ENTRIES);
    }

    #[test]
    fn cache_enforces_byte_budget_and_skips_oversized_entries() {
        let mut cache = BalCache::default();
        let raw = Bytes::from(vec![0; CACHE_BYTES / 2 + 1]);
        cache.insert(B256::repeat_byte(1), raw.clone());
        cache.insert(B256::repeat_byte(2), raw);
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0, B256::repeat_byte(2));
        assert_eq!(cache.bytes, CACHE_BYTES / 2 + 1);
        cache.insert(B256::repeat_byte(3), Bytes::from(vec![0; CACHE_BYTES + 1]));
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(cache.entries[0].0, B256::repeat_byte(2));
        assert_eq!(cache.bytes, CACHE_BYTES / 2 + 1);
    }

    #[test]
    fn invalid_attachment_does_not_replace_retained_bal() {
        let mut block = BscBlock::default();
        let header = &mut block.header;
        header.number = 732;
        header.block_access_list_hash = Some(keccak256([0xc0]));
        assert!(retain(header, 0, Bytes::from_static(&[0xc0])));
        assert!(!retain(header, 0, Bytes::from_static(&[0x80])));
        assert_eq!(get(header.hash_slow()), Some(Bytes::from_static(&[0xc0])));
        header.block_access_list_hash = Some(keccak256([0x80]));
        assert!(!retain(header, 0, Bytes::from_static(&[0x80])));
        assert_eq!(get(header.hash_slow()), None);
    }
}
