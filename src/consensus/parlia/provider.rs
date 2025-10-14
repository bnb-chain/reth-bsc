use super::snapshot::Snapshot;
use parking_lot::RwLock;
use std::sync::Arc;

use crate::chainspec::BscChainSpec;

use crate::consensus::parlia::{Parlia, VoteAddress};
use crate::node::evm::error::{BscBlockExecutionError, BscBlockValidationError};
use alloy_primitives::{Address, B256};
use alloy_primitives::{BlockNumber, BlockHash};

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
    /// Returns the snapshot that is valid for the given `block_number`.
    fn snapshot(&self, block_number: BlockNumber) -> Option<Snapshot>;

    /// Returns the snapshot that is valid for the given `block_hash`.
    fn snapshot_by_hash(&self, _block_hash: &BlockHash) -> Option<Snapshot> {
        None
    }

    /// Inserts (or replaces) the snapshot in the provider.
    fn insert(&self, snapshot: Snapshot);
    
    /// Returns the header for the given `block_number`.
    fn get_header(&self, block_number: BlockNumber) -> Option<alloy_consensus::Header>;

    /// Returns the header for the given `hash`.
    fn get_header_by_hash(&self, _block_hash: &BlockHash) -> Option<alloy_consensus::Header> {
        None
    }
}

/// `DbSnapshotProvider` wraps an MDBX database; it keeps a small in-memory LRU to avoid hitting
/// storage for hot epochs. The DB layer persists snapshots as CBOR blobs via the `ParliaSnapshots`
/// table that is already defined in `db.rs`.
#[derive(Debug)]
pub struct DbSnapshotProvider<DB: Database> {
    db: DB,
    /// Cache for snapshots by block number
    cache: RwLock<LruMap<BlockNumber, Snapshot, ByLength>>,
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
            cache: RwLock::new(LruMap::new(ByLength::new(capacity as u32))),
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
        
        tracing::warn!("Failed to find snapshot for block {}, searching for fallback...", block_number);
        
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
            tracing::warn!("Failed to find snapshot for block {} from DB", block_number);
        }
        last
    }

    fn query_db_by_hash(&self, block_hash: &B256) -> Option<Snapshot> {
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
        tx.put::<crate::consensus::parlia::db::ParliaSnapshots>(snap.block_number, ParliaSnapshotBlob(snap.clone().compress()))?;
        tx.put::<crate::consensus::parlia::db::ParliaSnapshotsByHash>(snap.block_hash, ParliaSnapshotBlob(snap.clone().compress()))?;
        tx.commit()?;
        tracing::debug!("Succeed to insert snapshot to db, block_number: {}, block_hash: {}", snap.block_number, snap.block_hash);
        Ok(())
    }
}

impl<DB: Database + 'static> SnapshotProvider for DbSnapshotProvider<DB> {
    fn snapshot(&self, block_number: u64) -> Option<Snapshot> {
        { // fast path: cache
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

    fn snapshot_by_hash(&self, block_hash: &B256) -> Option<Snapshot> {
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
        self.cache.write().insert(snapshot.block_number, snapshot.clone());
        self.cache_by_hash.write().insert(snapshot.block_hash, snapshot.clone());
        if snapshot.block_number.is_multiple_of(crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL) {
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
        // Incremental snapshot building to avoid OOM with large header collections
        let mut current_block = block_number;
        let base_snapshot = loop {
            { // fast path query.
                let mut cache_guard = self.base.cache.write();
                if let Some(snap) = cache_guard.get(&current_block) {
                    break snap.clone();
                }
            }

            // Check database at checkpoint intervals (every 1024 blocks)
            if current_block.is_multiple_of(crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL) {
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

            // Check if we need to handle genesis
            if current_block == 0 {
                if let Some(header) = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(0) {
                    let ValidatorsInfo { consensus_addrs, vote_addrs } =
                        self.parlia.parse_validators_from_header(&header, self.parlia.epoch).map_err(|err| {
                            BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
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
                } else {
                    tracing::error!("Failed to get genesis header for block 0");
                    return None;
                }
            }

            current_block = current_block.saturating_sub(1);
        };

        // Incremental forward building from base_snapshot to target block
        self.build_snapshot_incrementally(base_snapshot, block_number)
    }

    // query snapshot by hash, note that it will try to rebuild snapshot if not found.
    fn snapshot_by_hash(&self, block_hash: &BlockHash) -> Option<Snapshot> {
        if let Some(snap) = self.base.snapshot_by_hash(block_hash) {
            Some(snap)
        } else if let Some(target_header) = self.get_header_by_hash(block_hash) {
            if target_header.number == 0 {
                return self.init_genesis_snapshot(&target_header);
            }
            let snap= self.try_rebuild(&target_header);
            if snap.is_some() {
                self.base.cache.write().insert(target_header.number, snap.clone().unwrap());
                self.base.cache_by_hash.write().insert(target_header.hash_slow(), snap.clone().unwrap());
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
    
    fn get_header(&self, block_number: u64) -> Option<alloy_consensus::Header> {
        let header = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(block_number);
        tracing::debug!("Succeed to fetch header, is_none: {} for block {} in enhanced snapshot provider", header.is_none(), block_number);
        header
    }

    fn get_header_by_hash(&self, block_hash: &B256) -> Option<alloy_consensus::Header> {
        let header = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_hash(block_hash);
        tracing::debug!("Succeed to fetch header by hash, is_none: {} for hash {} in enhanced snapshot provider", header.is_none(), block_hash);
        header
    }
}

impl<DB: Database + 'static> EnhancedDbSnapshotProvider<DB> {
    fn init_genesis_snapshot(&self, genesis_header: &alloy_consensus::Header) -> Option<Snapshot> {
        let ValidatorsInfo { consensus_addrs, vote_addrs } =
            self.parlia.parse_validators_from_header(
                genesis_header, 
                self.parlia.epoch)
                .map_err(|err| {
                    BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError { error: err.into() })
                })
                .ok()?;
        let genesis_snapshot = Snapshot::new(
            consensus_addrs,
            0,
            genesis_header.hash_slow(),
            self.parlia.epoch,
            vote_addrs,
        );
        self.base.insert(genesis_snapshot.clone());
        Some(genesis_snapshot)
    }

    fn try_rebuild(&self, target_header: &alloy_consensus::Header) -> Option<Snapshot> {
        let mut skip_block_hashes = Vec::new();
         let base_snapshot = {
            let mut parent_block_hash = target_header.parent_hash;
            skip_block_hashes.push(target_header.hash_slow());
            loop {
                let parent_header = self.get_header_by_hash(&parent_block_hash);
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
                skip_block_hashes.push(parent_block_hash);
                tracing::debug!("Succeed to walk to parent block, parent_block_number: {}", parent_header.clone().unwrap().number);
                parent_block_hash = parent_header.clone().unwrap().parent_hash;
            }
        };
        if base_snapshot.is_none() {
            tracing::warn!("Failed to rebuild snapshot due to not found base snapshot");
            return None;
        }
        tracing::debug!("try rebuild snapshot, from block: {}, to block: {}, skip block len: {:?}", 
            base_snapshot.clone().unwrap().block_number, target_header.number, skip_block_hashes.len());

        skip_block_hashes.reverse();
        let mut working_snapshot = base_snapshot.clone().unwrap();
        for block_hash in skip_block_hashes {
            let apply_header = self.get_header_by_hash(&block_hash);
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
                    
                if let Some(checkpoint_header) = self.get_header(checkpoint_block_number) {
                    let parsed = 
                        self.parlia.parse_validators_from_header(&checkpoint_header, working_snapshot.epoch_num);
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
                self.parlia.get_vote_attestation_from_header(header.as_ref(), working_snapshot.epoch_num).map_err(|err| {
                tracing::error!("Failed to get vote attestation from header, block_number: {}, epoch_num: {}, error: {:?}", 
                    header.number, working_snapshot.epoch_num, err);
                err
            }).ok()?;

            // Apply header to snapshot
            working_snapshot = match working_snapshot.apply(
                header.beneficiary,
                header.as_ref(),
                new_validators,
                vote_addrs,
                attestation,
                turn_length,
                &*self.chain_spec,
            ) {
                Some(snap) => snap,
                None => {
                    tracing::warn!("Failed to apply header {} to snapshot", header.number);
                    return None;
                }
            };

            // rebuild snapshot is not refresh cache.
            if working_snapshot.block_number.is_multiple_of(crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL) {
                self.base.persist_to_db(&working_snapshot).ok()?;
            }
        }
        Some(working_snapshot)
    }

    /// Build snapshot incrementally to avoid OOM by processing headers in small chunks
    fn build_snapshot_incrementally(&self, base_snapshot: Snapshot, target_block: u64) -> Option<Snapshot> {
        const CHUNK_SIZE: u64 = 1024; // Process headers in chunks to avoid OOM
        
        let mut working_snapshot = base_snapshot;
        let mut current_block = working_snapshot.block_number + 1;
        
        tracing::debug!("Starting incremental snapshot build from block {} to {}", working_snapshot.block_number, target_block);
        
        while current_block <= target_block {
            let chunk_end = std::cmp::min(current_block + CHUNK_SIZE - 1, target_block);
            let mut headers_chunk = Vec::with_capacity((chunk_end - current_block + 1) as usize);
            
            // Collect headers for this chunk
            for block_num in current_block..=chunk_end {
                if let Some(header) = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(block_num) {
                    headers_chunk.push(header);
                } else {
                    tracing::error!("Failed to get header for block {} during incremental rebuild", block_num);
                    return None;
                }
            }
            
            tracing::trace!("Processing chunk: blocks {} to {} ({} headers)", current_block, chunk_end, headers_chunk.len());
            
            // Apply headers in this chunk
            for header in headers_chunk.iter() {
                let epoch_remainder = header.number % working_snapshot.epoch_num;
                let miner_check_len = working_snapshot.miner_history_check_len();
                let is_epoch_boundary = header.number > 0 && epoch_remainder == miner_check_len;
                let mut turn_length = None;
                
                let validators_info = if is_epoch_boundary {
                    let checkpoint_block_number = header.number - miner_check_len;
                    tracing::debug!("Updating validator set at epoch boundary, checkpoint_block: {}, current_block: {}", checkpoint_block_number, header.number);
                    
                    if let Some(checkpoint_header) = crate::node::evm::util::HEADER_CACHE_READER.lock().unwrap().get_header_by_number(checkpoint_block_number) {
                        let parsed = self.parlia.parse_validators_from_header(&checkpoint_header, working_snapshot.epoch_num);
                        turn_length = self.parlia.get_turn_length_from_header(&checkpoint_header, working_snapshot.epoch_num).map_err(|err| {
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
                let attestation = self.parlia.get_vote_attestation_from_header(header, working_snapshot.epoch_num).map_err(|err| {
                    tracing::error!("Failed to get vote attestation from header, block_number: {}, epoch_num: {}, error: {:?}", 
                        header.number, working_snapshot.epoch_num, err);
                    err
                }).ok()?;

                // Apply header to snapshot
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
                        tracing::warn!("Failed to apply header {} to snapshot", header.number);
                        return None;
                    }
                };

                // Cache and persist snapshots at checkpoints
                self.base.cache.write().insert(working_snapshot.block_number, working_snapshot.clone());
                if working_snapshot.block_number.is_multiple_of(crate::consensus::parlia::snapshot::CHECKPOINT_INTERVAL) {
                    tracing::info!("Persisting snapshot checkpoint for block {}", working_snapshot.block_number);
                    self.base.insert(working_snapshot.clone());
                }
            }
            
            current_block = chunk_end + 1;
            
            // Log progress and memory usage every 50k blocks
            if current_block.is_multiple_of(50000) {
                let mem_info = Self::get_memory_usage();
                tracing::info!("Incremental rebuild progress: {} / {} blocks completed, Memory: RSS={}MB VSZ={}MB", 
                    current_block - 1, target_block, mem_info.0, mem_info.1);
            }
        }
        
        tracing::info!("Completed incremental snapshot build to block {}", target_block);
        Some(working_snapshot)
    }

    /// Get current memory usage (RSS, VSZ) in MB
    fn get_memory_usage() -> (u64, u64) {
        #[cfg(target_os = "linux")]
        {
            if let Ok(contents) = std::fs::read_to_string("/proc/self/status") {
                let mut rss_kb = 0;
                let mut vsz_kb = 0;
                
                for line in contents.lines() {
                    if line.starts_with("VmRSS:") {
                        if let Some(value) = line.split_whitespace().nth(1) {
                            rss_kb = value.parse().unwrap_or(0);
                        }
                    } else if line.starts_with("VmSize:") {
                        if let Some(value) = line.split_whitespace().nth(1) {
                            vsz_kb = value.parse().unwrap_or(0);
                        }
                    }
                }
                
                return (rss_kb / 1024, vsz_kb / 1024); // Convert KB to MB
            }
        }
        
        // Fallback for non-Linux or if reading /proc fails
        (0, 0)
    }
}
