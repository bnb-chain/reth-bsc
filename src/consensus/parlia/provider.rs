use super::snapshot::Snapshot;
use parking_lot::RwLock;
use std::sync::Arc;

use crate::chainspec::BscChainSpec;

use crate::consensus::parlia::{Parlia, VoteAddress};
use crate::node::evm::error::BscBlockExecutionError;
use alloy_primitives::{Address, B256};

/// Validator information extracted from header
#[derive(Debug, Clone)]
pub struct ValidatorsInfo {
    pub consensus_addrs: Vec<Address>,
    pub vote_addrs: Option<Vec<VoteAddress>>,
}


use reth_db::{Database, DatabaseError};
use reth_db::table::{Compress, Decompress};
use reth_db::models::ParliaSnapshotBlob;
use reth_db::transaction::{DbTx, DbTxMut};
use reth_db::cursor::DbCursorRO;
use schnellru::{ByLength, LruMap};

pub trait SnapshotProvider: Send + Sync {
    /// Returns the snapshot that is valid for the given `block_number` (usually parent block).
    fn snapshot(&self, block_number: u64) -> Option<Snapshot>;

    /// Inserts (or replaces) the snapshot in the provider.
    fn insert(&self, snapshot: Snapshot);
    
    /// Returns the header for the given `block_number`.
    fn get_header(&self, block_number: u64) -> Option<alloy_consensus::Header>;

    fn get_header_by_hash(&self, _hash: &B256) -> Option<alloy_consensus::Header> {
        None
    }
}

/// `DbSnapshotProvider` wraps an MDBX database; it keeps a small in-memory LRU to avoid hitting
/// storage for hot epochs. The DB layer persists snapshots as CBOR blobs via the `ParliaSnapshots`
/// table that is already defined in `db.rs`.
/// 
/// Enhanced to include backward walking logic like reth-bsc-trail and bsc-erigon.
#[derive(Debug)]
pub struct DbSnapshotProvider<DB: Database> {
    db: DB,
    /// Front cache keyed by *block number*.
    cache: RwLock<LruMap<u64, Snapshot, ByLength>>,
}

/// Enhanced version with backward walking capability
#[derive(Debug)]
pub struct EnhancedDbSnapshotProvider<DB: Database> {
    base: DbSnapshotProvider<DB>,
    /// Chain spec for genesis snapshot creation
    chain_spec: Arc<BscChainSpec>,
    /// Parlia consensus instance
    parlia: Arc<Parlia<BscChainSpec>>,
}

impl<DB: Database> DbSnapshotProvider<DB> {
    pub fn new(db: DB, capacity: usize) -> Self {
        Self { 
            db, 
            cache: RwLock::new(LruMap::new(ByLength::new(capacity as u32))),
        }
    }
}

impl<DB: Database> EnhancedDbSnapshotProvider<DB> {
    pub fn new(
        db: DB, 
        capacity: usize, 
        chain_spec: Arc<BscChainSpec>,
    ) -> Self {
        let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));
        Self { 
            base: DbSnapshotProvider::new(db, capacity),
            chain_spec,
            parlia,
        }
    }
}

impl<DB: Database + Clone> Clone for DbSnapshotProvider<DB> {
    fn clone(&self) -> Self {
        // Create a new instance with the same database but a fresh cache
        Self::new(self.db.clone(), 2048)
    }
}

impl<DB: Database + Clone> Clone for EnhancedDbSnapshotProvider<DB> {
    fn clone(&self) -> Self {
        Self {
            base: self.base.clone(),
            chain_spec: self.chain_spec.clone(),
            parlia: self.parlia.clone(),
        }
    }
}

impl<DB: Database> DbSnapshotProvider<DB> {
    fn load_from_db(&self, block_number: u64) -> Option<Snapshot> {
        let tx = self.db.tx().ok()?;
        
        // Try to get the exact snapshot for the requested block number
        if let Ok(Some(raw_blob)) = tx.get::<crate::consensus::parlia::db::ParliaSnapshots>(block_number) {
            let raw = &raw_blob.0;
            if let Ok(decoded) = Snapshot::decompress(raw) {
                tracing::debug!("Succeed to find snapshot for block {} from DB (snapshot_block={})", block_number, decoded.block_number);
                return Some(decoded);
            }
        }
        
        tracing::debug!("Failed to find snapshot for block {}, searching for fallback...", block_number);
        
        // If exact snapshot not found, look for the most recent snapshot before this block
        let mut cursor = tx
            .cursor_read::<crate::consensus::parlia::db::ParliaSnapshots>()
            .ok()?;
        let mut iter = cursor.walk_range(..block_number).ok()?;
        let mut last: Option<Snapshot> = None;
        let mut found_count = 0;
        
        while let Some(Ok((db_block_num, raw_blob))) = iter.next() {
            let raw = &raw_blob.0;
            if let Ok(decoded) = Snapshot::decompress(raw) {
                found_count += 1;
                tracing::trace!("Scan snapshot in DB, block {} -> snapshot_block {}", db_block_num, decoded.block_number);
                last = Some(decoded);
            }
        }
        
        if let Some(ref snap) = last {
            tracing::debug!("Succeed to find fallback snapshot for block {} at block {} in DB (searched {} snapshots)", block_number, snap.block_number, found_count);
        } else {
            tracing::debug!("Failed to find snapshot for block {} from DB", block_number);
        }
        last
    }

    fn persist_to_db(&self, snap: &Snapshot) -> Result<(), DatabaseError> {
        let tx = self.db.tx_mut()?;
        tx.put::<crate::consensus::parlia::db::ParliaSnapshots>(snap.block_number, ParliaSnapshotBlob(snap.clone().compress()))?;
        tx.commit()?;
        tracing::debug!("Succeed to insert snapshot block {} to DB", snap.block_number);
        Ok(())
    }
}

impl<DB: Database + 'static> SnapshotProvider for DbSnapshotProvider<DB> {
    fn snapshot(&self, block_number: u64) -> Option<Snapshot> {
        // fast path: cache
        {
            let mut guard = self.cache.write();
            if let Some(snap) = guard.get(&block_number) {
                return Some(snap.clone());
            }
        }

        // slow path: DB scan
        let snap = self.load_from_db(block_number)?;
        self.cache.write().insert(block_number, snap.clone());
        Some(snap)
    }

    fn insert(&self, snapshot: Snapshot) {
        self.cache.write().insert(snapshot.block_number, snapshot.clone());
        if snapshot.block_number % crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL == 0 {
            match self.persist_to_db(&snapshot) {
                Ok(()) => {
                    tracing::debug!("Succeed to persist snapshot for block {} to DB", snapshot.block_number);
                },
                Err(e) => {
                    tracing::error!("Failed to persist snapshot for block {} to DB due to {:?}", snapshot.block_number, e);
                }
            }
        }
    }
    
    fn get_header(&self, _block_number: u64) -> Option<alloy_consensus::Header> {
        unimplemented!("DbSnapshotProvider doesn't have access to headers");
    }
}

// Simplified version based on reth-bsc-trail's approach - much faster and simpler
impl<DB: Database + 'static> SnapshotProvider for EnhancedDbSnapshotProvider<DB>
{
    fn snapshot(&self, block_number: u64) -> Option<Snapshot> {
        { // fast path query.
            let mut cache_guard = self.base.cache.write();
            if let Some(cached_snap) = cache_guard.get(&block_number) {
                tracing::debug!("Succeed to query snapshot from cache, request {} -> found snapshot for block {}", block_number, cached_snap.block_number);
                return Some(cached_snap.clone());
            }
        }
        
        // Cache miss, starting backward walking.
        // simple backward walking + proper epoch updates.
        let mut current_block = block_number;
        let mut headers_to_apply = Vec::new();
        let base_snapshot = loop {
            { // fast path query.
                let mut cache_guard = self.base.cache.write();
                if let Some(snap) = cache_guard.get(&current_block) {
                    break snap.clone();
                }
            }

            // Check database at checkpoint intervals (every 1024 blocks)
            if current_block % crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL == 0 {
                if let Some(snap) = self.base.load_from_db(current_block) {
                    tracing::debug!("Succeed to load snap, block_number: {}, snap_block_number: {}, wanted_block_number: {}", current_block, snap.block_number, block_number);
                    if snap.block_number == current_block {
                        self.base.cache.write().insert(current_block, snap.clone());
                        break snap;
                    } else {
                        tracing::warn!("Returned wrong snapshot: requested block {} but got snapshot for block {} - this indicates the snapshot hasn't been created yet", current_block, snap.block_number);
                        // Don't break here - continue backward walking to find a valid parent snapshot
                    }
                } else {
                    tracing::debug!("Failed to load snapshot in DB for block {}", current_block);
                }
            }

            // Collect header for forward application - fail if not available 
            if let Some(header) = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(current_block) {
                    if header.number == 0 {
                        let ValidatorsInfo { consensus_addrs, vote_addrs } =
                            self.parlia.parse_validators_from_header(&header).map_err(|err| {
                                BscBlockExecutionError::ParliaConsensusInnerError { error: err.into() }
                            }).ok()?;
                        let genesis_snap = Snapshot::new(
                            consensus_addrs,
                            0, // Genesis block number
                            header.hash_slow(),
                            self.parlia.epoch,
                            vote_addrs,
                        );
                        self.base.cache.write().insert(0, genesis_snap.clone());
                        self.base.persist_to_db(&genesis_snap).ok()?;
                        tracing::info!("Succeed to persist genesis snapshot for block 0 to DB");
                        break genesis_snap;
                    }
                    headers_to_apply.push(header.clone());
                    current_block = current_block.saturating_sub(1);
                } else {
                    tracing::error!("Failed to get snap due to load header in DB for block {}", current_block);
                    return None;
                }
            };

            headers_to_apply.reverse();
            let mut working_snapshot = base_snapshot;
            tracing::info!("Start to apply headers to base snapshot, base_snapshot: {:?}, target_snapshot: {}, apply_length: {}", 
                working_snapshot.block_number, block_number, headers_to_apply.len());

            for (_index, header) in headers_to_apply.iter().enumerate() {
                let epoch_remainder = header.number % working_snapshot.epoch_num;
                let miner_check_len = working_snapshot.miner_history_check_len();
                let is_epoch_boundary = header.number > 0 && epoch_remainder == miner_check_len;
                
                let validators_info = if is_epoch_boundary {
                    let checkpoint_block_number = header.number - miner_check_len;
                    let checkpoint_header = headers_to_apply.iter()
                        .find(|h| h.number == checkpoint_block_number);
                    
                    if let Some(checkpoint_header) = checkpoint_header {
                        let parsed = self.parlia.parse_validators_from_header(checkpoint_header);                    
                        parsed
                    } else {
                        match crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(checkpoint_block_number) {
                            Some(header_ref) => {
                                let parsed = self.parlia.parse_validators_from_header(&header_ref);                    
                                parsed
                            },
                            None => {
                                tracing::warn!("Failed to find checkpoint header for block {} - header not found", checkpoint_block_number);
                                return None;
                            }
                        }
                    }
                } else {
                    Ok(ValidatorsInfo {
                        consensus_addrs: Vec::new(),
                        vote_addrs: None,
                    })
                }.ok()?;

                let new_validators = validators_info.consensus_addrs;
                let vote_addrs = validators_info.vote_addrs;
                let turn_length = self.parlia.get_turn_length_from_header(header).ok()?;

                // Parse attestation from header for vote tracking
                let attestation = self.parlia.get_vote_attestation_from_header(header).ok()?;

                // Apply header to snapshot (now determines hardfork activation internally)
                working_snapshot = match working_snapshot.apply(
                    header.beneficiary,
                    header,
                    new_validators,
                    vote_addrs,
                    attestation,
                    turn_length,
                    &*self.chain_spec,
                ) {
                    Some(snap) => snap,
                    None => {
                        tracing::warn!("Failed to apply header {} to snapshot, snapshot: {:?}", header.number, working_snapshot);
                        return None;
                    }
                };

                self.base.cache.write().insert(working_snapshot.block_number, working_snapshot.clone());
                if working_snapshot.block_number % crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL == 0 {
                    tracing::debug!("Succeed to rebuild snapshot checkpoint for block {} to DB", working_snapshot.block_number);
                    self.base.insert(working_snapshot.clone());
                }
            }
        
        Some(working_snapshot)
    }

    fn insert(&self, snapshot: Snapshot) {
        self.base.insert(snapshot);
    }
    
    fn get_header(&self, block_number: u64) -> Option<alloy_consensus::Header> {
        let header = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(block_number);
        tracing::info!("Succeed to fetch header, is_none: {} for block {} in enhanced snapshot provider", header.is_none(), block_number);
        header
    }

    fn get_header_by_hash(&self, block_hash: &B256) -> Option<alloy_consensus::Header> {
        let header = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_hash(block_hash);
        tracing::info!("Succeed to fetch header, is_none: {} for block {} in enhanced snapshot provider", header.is_none(), block_hash);
        header
    }
}
