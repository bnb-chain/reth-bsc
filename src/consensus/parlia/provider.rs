use super::snapshot::Snapshot;
use alloy_consensus::Header;
use parking_lot::RwLock;
use std::sync::Arc;

use crate::chainspec::BscChainSpec;

use crate::consensus::parlia::{CHECKPOINT_INTERVAL, Parlia, VoteAddress};
use crate::node::evm::error::{BscBlockExecutionError, BscBlockValidationError};
use crate::node::evm::util::{get_cannonical_header_from_cache, get_header_by_hash_from_cache};
use alloy_primitives::{Address};
use alloy_primitives::{BlockHash};

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
use schnellru::{ByLength, LruMap};

pub trait SnapshotProvider: Send + Sync {
    /// Returns the snapshot that is valid for the given `block_hash`.
    fn snapshot_by_hash(&self, _block_hash: &BlockHash) -> Option<Snapshot>;
    /// Inserts (or replaces) the snapshot in the provider.
    fn insert(&self, snapshot: Snapshot);
}

/// `DbSnapshotProvider` wraps an MDBX database; it keeps a small in-memory LRU to avoid hitting
/// storage for hot epochs. The DB layer persists snapshots as CBOR blobs via the `ParliaSnapshots`
/// table that is already defined in `db.rs`.
#[derive(Debug)]
pub struct DbSnapshotProvider<DB: Database> {
    db: DB,
    /// Cache for snapshots by block hash
    cache_by_hash: RwLock<LruMap<BlockHash, Snapshot, ByLength>>,
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
            cache_by_hash: RwLock::new(LruMap::new(ByLength::new(capacity as u32))),
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
    fn query_db_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
        let tx = self.db.tx().ok()?;
        if let Ok(Some(raw_blob)) = tx.get::<crate::consensus::parlia::db::ParliaSnapshotsByHash>(*block_hash) {
            let raw = &raw_blob.0;
            if let Ok(decoded) = Snapshot::decompress(raw) {
                tracing::debug!("Succeed to query snapshot from db, block_number: {}, block_hash: {}", decoded.block_number, decoded.block_hash);
                return Some(decoded);
            }
        }
        None
    }

    fn persist_to_db(&self, snap: &Snapshot) -> Result<(), DatabaseError> {
        let tx = self.db.tx_mut()?;
        tx.put::<crate::consensus::parlia::db::ParliaSnapshotsByHash>(snap.block_hash, ParliaSnapshotBlob(snap.clone().compress()))?;
        tx.commit()?;
        tracing::debug!("Succeed to insert snapshot to db, block_number: {}, block_hash: {}", snap.block_number, snap.block_hash);
        Ok(())
    }

    /// Insert into in-memory cache only (no persistence).
    /// Used to cache intermediate snapshots during rebuild to avoid repeated long rebuilds.
    pub fn insert_cache_only(&self, snapshot: &Snapshot) {
        self.cache_by_hash.write().insert(snapshot.block_hash, snapshot.clone());
    }
}

impl<DB: Database + 'static> SnapshotProvider for DbSnapshotProvider<DB> {
    fn snapshot_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
        { // fast path: cache
            let mut guard = self.cache_by_hash.write();
            if let Some(snap) = guard.get(block_hash) {
                return Some(snap.clone());
            }
        }
        // slow path: query db
        let snap = self.query_db_by_hash(block_hash)?;
        self.cache_by_hash.write().insert(*block_hash, snap.clone());
        Some(snap)
    }

    fn insert(&self, snapshot: Snapshot) {
        self.cache_by_hash.write().insert(snapshot.block_hash, snapshot.clone());
        if snapshot.block_number.is_multiple_of(CHECKPOINT_INTERVAL) {
            match self.persist_to_db(&snapshot) {
                Ok(()) => {
                    tracing::debug!("Persisted snapshot for block {} to DB", snapshot.block_number);
                },
                Err(e) => {
                    tracing::error!("Failed to persist snapshot for block {} to DB: {:?}", snapshot.block_number, e);
                }
            }
        }
    }
}

// Simplified version based on reth-bsc-trail's approach - much faster and simpler
impl<DB: Database + 'static> SnapshotProvider for EnhancedDbSnapshotProvider<DB>
{
    // query snapshot by hash, note that it will try to rebuild snapshot if not found.
    fn snapshot_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
        // query snapshot from cache or db
        if let Some(snap) = self.base.snapshot_by_hash(block_hash) {
            Some(snap)
        } else if let Some(target_header) = get_header_by_hash_from_cache(block_hash) {
            if target_header.number == 0 {
                return self.init_genesis_snapshot(&target_header);
            }
            let snap= self.try_rebuild(&target_header);
            if let Some(s) = snap.as_ref() {
                self.base.insert(s.clone());
            }
            snap
        } else {
            tracing::warn!("Failed to query snapshot by hash due to not found header, block_hash: {}", block_hash);
            None
        }
    }

    fn insert(&self, snapshot: Snapshot) {
        self.base.insert(snapshot);
    }
}

impl<DB: Database + 'static> EnhancedDbSnapshotProvider<DB> {
    fn init_genesis_snapshot(&self, genesis_header: &Header) -> Option<Snapshot> {
        let ValidatorsInfo { consensus_addrs, vote_addrs } =
            self.parlia.parse_validators_from_header(
                genesis_header, 
                self.parlia.epoch)
                .map_err(|err| {
                    tracing::error!("Failed to parse validators from genesis header: {:?}", err);
                    BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
                })
                .ok()?;
        
        let genesis_snapshot = Snapshot::new(
            consensus_addrs.clone(),
            0,
            genesis_header.hash_slow(),
            self.parlia.epoch,
            vote_addrs.clone(),
        );
        
        tracing::info!("Genesis snapshot initialized: block=0, validators={}", 
            genesis_snapshot.validators.len());
        
        self.base.insert(genesis_snapshot.clone());
        Some(genesis_snapshot)
    }

    fn try_rebuild(&self, target_header: &Header) -> Option<Snapshot> {
        let mut rebuild_block_hashes = Vec::new();
         let base_snapshot = {
            let mut parent_block_hash = target_header.parent_hash;
            rebuild_block_hashes.push(target_header.hash_slow());
            loop {
                let parent_header = get_header_by_hash_from_cache(&parent_block_hash);
                if parent_header.is_none() {
                    tracing::warn!("Failed to query snapshot by hash due to not found header, block_hash: {}", parent_block_hash);
                    break None;
                }
                if parent_header.clone().unwrap().number == 0 {
                    self.init_genesis_snapshot(parent_header.as_ref().unwrap());
                }
                if let Some(snap) = self.base.snapshot_by_hash(&parent_block_hash) {
                    break Some(snap);
                }
                rebuild_block_hashes.push(parent_block_hash);
                tracing::trace!("Succeed to walk to parent block, parent_block_number: {}", parent_header.clone().unwrap().number);
                parent_block_hash = parent_header.clone().unwrap().parent_hash;
            }
        };
        if base_snapshot.is_none() {
            tracing::warn!("Failed to rebuild snapshot due to not found base snapshot");
            return None;
        }
        tracing::debug!("try rebuild snapshot, from_block: {}, to_block: {}, rebuild_block_len: {:?}", 
            base_snapshot.clone().unwrap().block_number, target_header.number, rebuild_block_hashes.len());

        rebuild_block_hashes.reverse();
        let mut working_snapshot = base_snapshot.clone().unwrap();
        for block_hash in rebuild_block_hashes {
            let apply_header = get_header_by_hash_from_cache(&block_hash);
            if apply_header.is_none() {
                tracing::warn!("Failed to query snapshot by hash due to not found header, block_hash: {}", block_hash);
                return None;
            }
            let header = apply_header.unwrap();
            let epoch_remainder = header.number % working_snapshot.epoch_num;
            let miner_check_len = working_snapshot.miner_history_check_len();
            let is_epoch_boundary = header.number > 0 && epoch_remainder == miner_check_len;
            let mut turn_length = None;
                
            let validators_info = if is_epoch_boundary {
                let checkpoint_block_number = header.number - miner_check_len;
                tracing::debug!("Updating validator set at epoch boundary, checkpoint_block: {}, current_block: {}", 
                    checkpoint_block_number, header.number);
                
                if let Some(checkpoint_header) = get_cannonical_header_from_cache(checkpoint_block_number) {
                    let parsed = 
                        self.parlia.parse_validators_from_header(&checkpoint_header, working_snapshot.epoch_num)
                            .map_err(|err| {
                                tracing::error!("Failed to parse validators from checkpoint header: {:?}", err);
                                err
                            });
                    
                    turn_length = 
                        self.parlia.get_turn_length_from_header(
                            &checkpoint_header, 
                            working_snapshot.epoch_num).map_err(|err| {
                        tracing::error!("Failed to get turn length from checkpoint header, block_number: {}, checkpoint_block_number: {}, epoch_num: {}, error: {:?}", 
                            header.number, checkpoint_block_number, working_snapshot.epoch_num, err);
                        err
                    }).ok()?;
                    parsed
                } else {
                    tracing::error!("Failed to find checkpoint header for block {}", checkpoint_block_number);
                    return None;
                }
            } else {
                Ok(ValidatorsInfo {
                    consensus_addrs: Vec::new(),
                    vote_addrs: None,
                })
            }.ok()?;

            let new_validators = validators_info.consensus_addrs;
            let vote_addrs = validators_info.vote_addrs;
            let attestation = 
                self.parlia.get_vote_attestation_from_header(&header, working_snapshot.epoch_num).map_err(|err| {
                tracing::error!("Failed to get vote attestation from header, block_number: {}, epoch_num: {}, error: {:?}", 
                    header.number, working_snapshot.epoch_num, err);
                err
            }).ok()?;

            // Apply header to snapshot
            working_snapshot = match working_snapshot.apply(
                header.beneficiary,
                &header,
                new_validators,
                vote_addrs,
                attestation,
                turn_length,
                &*self.chain_spec,
            ) {
                Some(snap) => {
                    tracing::trace!(
                        "Successfully applied header: block_number={}, epoch_num={}, validators={}",
                        snap.block_number, snap.epoch_num, snap.validators.len()
                    );
                    // Cache intermediate snapshots in memory to avoid repeated long rebuilds
                    self.base.insert_cache_only(&snap);
                    snap
                },
                None => {
                    tracing::warn!("Failed to apply header {} to snapshot", header.number);
                    return None;
                }
            };

            // Persist at checkpoint boundaries to DB; intermediate snapshots are cached in-memory only.
            if working_snapshot.block_number.is_multiple_of(crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL) {
                self.base.persist_to_db(&working_snapshot).ok()?;
            }
        }
        Some(working_snapshot)
    }

}
