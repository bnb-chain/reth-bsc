//! Prepare ETH-serviceable data in the importer, then recheck cache-only at the network boundary.

use super::service::{BlockMsg, ImportEvent};
use crate::{
    chainspec::BscChainSpec,
    node::network::{
        announced_blocks::{AnnouncedBlocks, Classification},
        BscNewBlock,
    },
    BscBlock,
};
use alloy_primitives::{B256, U128, U256};
use reth_eth_wire::NewBlock;
use reth_network::{
    import::{BlockImportEvent, BlockValidation},
    message::NewBlockMessage,
};
use reth_provider::{BlockReader, HeaderProvider, ProviderResult};
use std::{fmt, sync::Arc};

#[derive(Clone)]
pub(crate) struct Publication {
    pub(crate) retained: Arc<AnnouncedBlocks>,
    lookup: Arc<dyn Fn(B256) -> ProviderResult<Option<ReadyBlock>> + Send + Sync>,
}

pub(crate) struct ReadyBlock {
    pub(crate) block: Arc<BscBlock>,
    pub(crate) td: U256,
}

impl fmt::Debug for Publication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Publication")
            .field("retained", &self.retained)
            .finish_non_exhaustive()
    }
}

impl Publication {
    pub(crate) fn from_provider<P>(
        retained: Arc<AnnouncedBlocks>,
        provider: P,
        chain_spec: Arc<BscChainSpec>,
    ) -> Self
    where
        P: BlockReader<Block = BscBlock>
            + HeaderProvider<Header = alloy_consensus::Header>
            + Send
            + Sync
            + 'static,
    {
        Self::new(retained, move |hash| {
            capture_canonical_block(
                hash,
                || provider.block_by_hash(hash),
                |number| provider.block_hash(number),
                |number| provider.header_td_by_number(number),
                &chain_spec,
            )
        })
    }

    pub(crate) fn new(
        retained: Arc<AnnouncedBlocks>,
        lookup: impl Fn(B256) -> ProviderResult<Option<ReadyBlock>> + Send + Sync + 'static,
    ) -> Self {
        Self { retained, lookup: Arc::new(lookup) }
    }

    /// This may read the provider and belongs only in the import actor, never NetworkManager poll.
    pub(crate) fn prepare_hash(&self, hash: B256) -> bool {
        if self.retained.get(hash).is_some() {
            return true;
        }
        let version = self.retained.view_version();
        let ready = match (self.lookup)(hash) {
            Ok(Some(ready)) => ready,
            Ok(None) => {
                skipped("provider_unready");
                return false;
            }
            Err(error) => {
                tracing::error!(target: "bsc::block_import", %hash, %error, "Failed to prepare network block data");
                skipped("provider_error");
                return false;
            }
        };
        if ready.block.header.hash_slow() != hash {
            skipped("hash_mismatch");
            return false;
        }
        self.retained.retain(ready.block, ready.td, Classification::Canonical, version)
    }

    /// Geth advertises the exact parent through a full NewBlock, without waiting for the child.
    pub(crate) fn prepare_parent(&self, block: &BlockMsg) -> bool {
        let header = &block.block.0.block.header;
        header.number == 0 || self.prepare_hash(header.parent_hash)
    }

    pub(crate) fn prepare_event(&self, event: &mut ImportEvent) -> bool {
        match event {
            BlockImportEvent::Announcement(validation) => self.prepare_validation(validation),
            BlockImportEvent::Outcome(outcome) => match &mut outcome.result {
                Ok(validation) => self.prepare_validation(validation),
                Err(_) => true,
            },
        }
    }

    fn prepare_validation(&self, validation: &mut BlockValidation<BscNewBlock>) -> bool {
        match validation {
            BlockValidation::ValidHeader { block } => self.prepare_full(block),
            BlockValidation::ValidBlock { block } => self.prepare_hash(block.hash),
        }
    }

    /// All remaining work is bounded cache access and fixed-size metadata checks. Queue admission
    /// is not a wire-send acknowledgment; TTL/capacity guarantees do not extend to peer delivery.
    pub(crate) fn filter_event(&self, mut event: ImportEvent) -> Option<ImportEvent> {
        let validation = match &mut event {
            BlockImportEvent::Announcement(validation) => validation,
            BlockImportEvent::Outcome(outcome) => match &mut outcome.result {
                Ok(validation) => validation,
                Err(_) => return Some(event),
            },
        };
        match validation {
            BlockValidation::ValidHeader { block } => *block = self.full_ready(block.clone())?,
            BlockValidation::ValidBlock { block } => {
                let (ready, _) = self.retained.republish(block.hash)?;
                if ready.header.number != block.block.0.block.header.number {
                    skipped("height_mismatch");
                    return None;
                }
            }
        }
        Some(event)
    }

    /// Normalize TD in the import actor; changing the envelope may clone a shared block body.
    /// The NetworkManager boundary below never performs this potentially large clone.
    pub(crate) fn prepare_full(&self, block: &mut BlockMsg) -> bool {
        if !self.prepare_parent(block) {
            return false;
        }
        let Some(td) = self.full_td(block, false) else {
            return false;
        };
        if block.block.0.td != td {
            Arc::make_mut(&mut block.block).0.td = td;
        }
        true
    }

    /// Cache-only final parent check also covers an admission that evicted the parent meanwhile.
    pub(crate) fn full_ready(&self, block: BlockMsg) -> Option<BlockMsg> {
        let td = self.full_td(&block, true)?;
        if block.block.0.td != td {
            skipped("td_mismatch");
            return None;
        }
        Some(block)
    }

    fn full_td(&self, block: &BlockMsg, publishing: bool) -> Option<U128> {
        let header = &block.block.0.block.header;
        if header.hash_slow() != block.hash {
            skipped("hash_mismatch");
            return None;
        }
        let td = if let Some(number) = header.number.checked_sub(1) {
            let parent = if publishing {
                self.retained.republish(header.parent_hash)
            } else {
                self.retained.get(header.parent_hash)
            };
            let Some((parent, parent_td)) = parent else {
                skipped("parent_unready");
                return None;
            };
            if parent.header.number != number {
                skipped("height_mismatch");
                return None;
            }
            let Some(td) = parent_td.checked_add(header.difficulty) else {
                skipped("td_overflow");
                return None;
            };
            td
        } else {
            header.difficulty
        };
        if td > U256::from(u128::MAX) {
            skipped("td_overflow");
            return None;
        }
        Some(U128::from(td.to::<u128>()))
    }

    /// Builds a hash event only after actor-side preparation. It retains no execution state.
    pub(crate) fn hash_message(&self, hash: B256) -> Option<BlockMsg> {
        let (block, td) = self.retained.get(hash)?;
        if td > U256::from(u128::MAX) {
            skipped("td_overflow");
            return None;
        }
        Some(NewBlockMessage {
            hash,
            block: Arc::new(BscNewBlock(NewBlock {
                block: (*block).clone(),
                td: U128::from(td.to::<u128>()),
            })),
        })
    }
}

/// Initial admission accepts only an exact canonical provider block. The two canonical checks
/// reject observed reorgs around the height-keyed TD read; they are not an atomic snapshot and do
/// not prove absence of an A-to-B-to-A interleaving. Once retained, TD is immutable and hash-keyed.
fn capture_canonical_block(
    hash: B256,
    block: impl FnOnce() -> ProviderResult<Option<BscBlock>>,
    mut canonical_hash: impl FnMut(u64) -> ProviderResult<Option<B256>>,
    td: impl FnOnce(u64) -> ProviderResult<Option<U256>>,
    chain_spec: &BscChainSpec,
) -> ProviderResult<Option<ReadyBlock>> {
    let Some(mut block) = block()? else { return Ok(None) };
    let number = block.header.number;
    if block.header.hash_slow() != hash || canonical_hash(number)? != Some(hash) {
        return Ok(None);
    }
    // As in head_fetch::validate_block, BSC may omit an empty withdrawals list on the wire.
    if block.header.withdrawals_root == Some(crate::consensus::parlia::EMPTY_WITHDRAWALS_HASH) &&
        block.body.inner.withdrawals.is_none()
    {
        block.body.inner.withdrawals = Some(Default::default());
    }
    if reth::consensus_common::validation::validate_body_against_header(&block.body, &block.header)
        .is_err() ||
        crate::node::network::data_availability::validate_stored_data_availability(
            &mut block, chain_spec,
        )
        .is_err()
    {
        skipped("incomplete_body");
        return Ok(None);
    }
    let Some(td) = td(number)? else { return Ok(None) };
    if canonical_hash(number)? != Some(hash) {
        return Ok(None);
    }
    Ok(Some(ReadyBlock { block: Arc::new(block), td }))
}

fn skipped(reason: &'static str) {
    metrics::counter!("bsc.network.retention.publication_skipped_total", "reason" => reason)
        .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use parking_lot::Mutex;
    use reth_primitives_traits::BlockBody;
    use std::{
        cell::Cell,
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    fn spec() -> BscChainSpec {
        BscChainSpec::from(reth_chainspec::ChainSpecBuilder::mainnet().cancun_activated().build())
    }

    fn block(number: u64, parent_hash: B256) -> Arc<BscBlock> {
        Arc::new(BscBlock {
            header: Header { number, parent_hash, difficulty: U256::from(2), ..Default::default() },
            ..Default::default()
        })
    }

    fn message(block: &BscBlock, td: u128) -> BlockMsg {
        NewBlockMessage {
            hash: block.header.hash_slow(),
            block: Arc::new(BscNewBlock(NewBlock { block: block.clone(), td: U128::from(td) })),
        }
    }

    #[test]
    fn capture_checks_exact_hash_body_and_canonical_td_view() {
        let block = block(1, B256::ZERO);
        let hash = block.header.hash_slow();
        let read = || Ok(Some((*block).clone()));
        let canonical = |_| Ok(Some(hash));
        let td = |_| Ok(Some(U256::from(22)));
        let ready = capture_canonical_block(hash, read, canonical, td, &spec()).unwrap().unwrap();
        assert_eq!(ready.td, U256::from(22));
        assert_eq!(ready.block, block);
        assert!(capture_canonical_block(B256::ZERO, read, canonical, td, &spec())
            .unwrap()
            .is_none());
        let reads = Cell::new(0);
        assert!(capture_canonical_block(
            hash,
            read,
            |_| {
                reads.set(reads.get() + 1);
                Ok(Some(if reads.get() == 1 { hash } else { B256::ZERO }))
            },
            td,
            &spec()
        )
        .unwrap()
        .is_none());
        assert_eq!(reads.get(), 2);
        let mut corrupt = (*block).clone();
        corrupt.header.transactions_root = B256::repeat_byte(9);
        let corrupt_hash = corrupt.header.hash_slow();
        assert!(capture_canonical_block(
            corrupt_hash,
            || Ok(Some(corrupt)),
            |_| Ok(Some(corrupt_hash)),
            |_| panic!("invalid body must not reach TD lookup"),
            &spec()
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn canonical_bsc_empty_withdrawals_omission_is_serviceable() {
        let mut block = (*block(1, B256::ZERO)).clone();
        block.header.withdrawals_root = Some(crate::consensus::parlia::EMPTY_WITHDRAWALS_HASH);
        let hash = block.header.hash_slow();
        let ready = capture_canonical_block(
            hash,
            || Ok(Some(block)),
            |_| Ok(Some(hash)),
            |_| Ok(Some(U256::from(10))),
            &spec(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(ready.block.body.inner.withdrawals, Some(Default::default()));
    }

    #[test]
    fn incomplete_stored_blob_body_can_be_retried_after_sidecars_return() {
        let now =
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let mut good = crate::node::network::data_availability::tests::blob_block(now);
        good.header.transactions_root = good.body.calculate_tx_root();
        let hash = good.header.hash_slow();
        for sidecar in good.body.sidecars.as_mut().unwrap() {
            sidecar.block_hash = hash;
        }
        let mut partial = good.clone();
        partial.body.sidecars.as_mut().unwrap().pop();
        let stored = Arc::new(Mutex::new(partial));
        let source = stored.clone();
        let publication = Publication::new(Arc::new(AnnouncedBlocks::default()), move |hash| {
            capture_canonical_block(
                hash,
                || Ok(Some(source.lock().clone())),
                |_| Ok(Some(hash)),
                |_| Ok(Some(U256::from(20))),
                &spec(),
            )
        });
        assert!(!publication.prepare_hash(hash));
        assert!(publication.retained.get(hash).is_none());
        *stored.lock() = good.clone();
        assert!(publication.prepare_hash(hash));
        assert_eq!(*publication.retained.get(hash).unwrap().0, good);
    }

    #[test]
    fn early_full_requires_only_exact_parent_and_hash_waits_for_child() {
        let parent = block(1, B256::ZERO);
        let child = block(2, parent.header.hash_slow());
        let ready = Arc::new(Mutex::new(None::<Arc<BscBlock>>));
        let source = ready.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let lookup_calls = calls.clone();
        let cache = Arc::new(AnnouncedBlocks::default());
        cache.retain(parent.clone(), U256::from(10), Classification::Sidechain, 0);
        let publication = Publication::new(cache, move |hash| {
            lookup_calls.fetch_add(1, Ordering::Relaxed);
            Ok(source
                .lock()
                .as_ref()
                .filter(|b| b.header.hash_slow() == hash)
                .map(|block| ReadyBlock { block: block.clone(), td: U256::from(12) }))
        });
        let mut msg = message(&child, 999);
        let shared = msg.block.clone();
        assert!(publication.full_ready(msg.clone()).is_none());
        assert!(Arc::ptr_eq(&shared, &msg.block), "final gate must not clone or rewrite payloads");
        assert!(publication.prepare_full(&mut msg));
        assert_eq!(msg.block.0.td, U128::from(12));
        assert_eq!(shared.0.td, U128::from(999));
        let envelope = msg.block.clone();
        assert!(Arc::ptr_eq(&publication.full_ready(msg.clone()).unwrap().block, &envelope));
        assert_eq!(calls.load(Ordering::Relaxed), 0, "retained parent avoids provider IO");
        let hash_event =
            || BlockImportEvent::Announcement(BlockValidation::ValidBlock { block: msg.clone() });
        assert!(publication.filter_event(hash_event()).is_none());
        *ready.lock() = Some(child.clone());
        assert!(publication.prepare_hash(msg.hash));
        assert!(publication.filter_event(hash_event()).is_some());
        let reads = calls.load(Ordering::Relaxed);
        assert!(publication.filter_event(hash_event()).is_some());
        assert_eq!(calls.load(Ordering::Relaxed), reads, "network boundary is cache-only");
    }

    #[test]
    fn final_gate_rechecks_parent_after_child_capacity_admission() {
        let cache = Arc::new(AnnouncedBlocks::new(Duration::from_secs(1800), 1, u64::MAX));
        let parent = block(1, B256::ZERO);
        let child = block(2, parent.header.hash_slow());
        cache.retain(parent.clone(), U256::from(10), Classification::Canonical, 0);
        let publication = Publication::new(cache.clone(), |_| panic!("no IO at final gate"));
        let mut msg = message(&child, 0);
        assert!(publication.prepare_full(&mut msg));
        assert!(cache.retain(child.clone(), U256::from(12), Classification::Canonical, 0));
        assert!(
            publication.full_ready(msg).is_none(),
            "child admission evicted the only parent copy"
        );
        assert!(publication
            .filter_event(BlockImportEvent::Announcement(BlockValidation::ValidBlock {
                block: message(&child, 12)
            }))
            .is_some());
        let tiny = Arc::new(AnnouncedBlocks::new(Duration::from_secs(1800), 1, 1));
        let lookup_child = child.clone();
        let publication = Publication::new(tiny, move |_| {
            Ok(Some(ReadyBlock { block: lookup_child.clone(), td: U256::from(12) }))
        });
        assert!(!publication.prepare_hash(child.header.hash_slow()));
        assert!(publication
            .filter_event(BlockImportEvent::Announcement(BlockValidation::ValidBlock {
                block: message(&child, 12)
            }))
            .is_none());
    }

    #[test]
    fn retained_branch_td_is_stable_and_wrong_height_and_overflow_fail_closed() {
        let cache = Arc::new(AnnouncedBlocks::default());
        let parent = block(1, B256::ZERO);
        let child = block(2, parent.header.hash_slow());
        let publication =
            Publication::new(cache.clone(), |_| panic!("side branch must use retained TD"));
        cache.retain(parent.clone(), U256::from(10), Classification::Canonical, 0);
        cache.canonical_change([parent.header.hash_slow()], []);
        let mut msg = message(&child, 12);
        assert!(publication.prepare_full(&mut msg));
        assert_eq!(publication.full_ready(msg).unwrap().block.0.td, U128::from(12));
        let wrong_height = block(3, parent.header.hash_slow());
        assert!(publication.full_ready(message(&wrong_height, 12)).is_none());
        cache.retain(parent, U256::MAX, Classification::Sidechain, cache.view_version());
        assert!(publication.full_ready(message(&child, 12)).is_none());
    }
}
