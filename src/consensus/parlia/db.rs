use reth_db::table::Table;
use alloy_primitives::BlockHash;

/// Table: epoch boundary block number (u64) -> compressed snapshot bytes.
#[derive(Debug)]
pub struct ParliaSnapshots;

impl Table for ParliaSnapshots {
    const NAME: &'static str = "ParliaSnapshots";
    const DUPSORT: bool = false;
    type Key = u64;
    /// Raw compressed bytes produced by `Snapshot::compress()`.
    type Value = reth_db::models::ParliaSnapshotBlob;
} 

/// Table: epoch boundary block hash (BlockHash) -> compressed snapshot bytes.
#[derive(Debug)]
pub struct ParliaSnapshotsByHash;

impl Table for ParliaSnapshotsByHash {
    const NAME: &'static str = "ParliaSnapshotsByHash";
    const DUPSORT: bool = false;
    type Key = BlockHash;
    /// Raw compressed bytes produced by `Snapshot::compress()`.
    type Value = reth_db::models::ParliaSnapshotBlob;
} 