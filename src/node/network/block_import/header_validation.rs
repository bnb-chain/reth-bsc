//! Validation required before a peer's block may be relayed.
use crate::{
    chainspec::BscChainSpec,
    consensus::parlia::{header_verifier::HeaderVerifier, Parlia, SnapshotProvider},
    metrics::BscVoteMetrics,
    node::{consensus::BscConsensus, evm::util::get_header_by_hash_from_cache},
    BscBlock,
};
use alloy_consensus::{BlockHeader, Header};
use reth::consensus::{Consensus, ConsensusError, HeaderValidator};
use reth_network::import::BlockImportError;
use reth_primitives_traits::{SealedBlock, SealedHeader};
use reth_provider::HeaderProvider;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum RelayValidation {
    Valid,
    /// Local ancestry or snapshot data is unavailable. Import/recovery may proceed,
    /// but forwarding must wait for an authoritative successful validation.
    Deferred,
}

/// Runs on the blocking pool because snapshot reconstruction may access the database.
pub(super) fn validate_for_relay<P: HeaderProvider<Header = Header>>(
    block: &SealedBlock<BscBlock>,
    provider: &P,
    chain_spec: Arc<BscChainSpec>,
    snapshots: Option<&(dyn SnapshotProvider + Send + Sync)>,
) -> Result<RelayValidation, BlockImportError> {
    let consensus = BscConsensus::new(chain_spec.clone());
    match consensus.validate_header(block.sealed_header()) {
        // A clock difference is not proof of a bad peer. Leave handling to import,
        // without authorizing early relay.
        Err(ConsensusError::TimestampIsInFuture { .. }) => return Ok(RelayValidation::Deferred),
        result => result?,
    }
    // A signed header alone must not authorize forwarding an unrelated body.
    consensus.validate_block_pre_execution(block)?;

    let parent = match provider.header(block.parent_hash()) {
        Ok(Some(parent)) => parent,
        Ok(None) => match get_header_by_hash_from_cache(&block.parent_hash()) {
            Some(parent) => parent,
            None => return Ok(RelayValidation::Deferred),
        },
        Err(error) => {
            tracing::debug!(target: "bsc::block_import", %error, "Deferring relay: parent unavailable");
            return Ok(RelayValidation::Deferred);
        }
    };
    let parent = SealedHeader::seal_slow(parent);
    consensus.validate_header_against_parent(block.sealed_header(), &parent)?;

    let Some(snapshots) = snapshots else {
        return Ok(RelayValidation::Deferred);
    };
    let Some(snapshot) = snapshots.snapshot_by_hash(&block.parent_hash()) else {
        return Ok(RelayValidation::Deferred);
    };
    if snapshot.block_hash != parent.hash() || snapshot.block_number != parent.number {
        tracing::warn!(target: "bsc::block_import", "Deferring relay: parent snapshot mismatch");
        return Ok(RelayValidation::Deferred);
    }

    let parlia = Parlia::new(chain_spec, 200);
    let metrics = BscVoteMetrics::default();
    match HeaderVerifier::new(&parlia, Some(snapshots), &metrics).verify(
        block.header(),
        parent.header(),
        &snapshot,
    ) {
        Ok(()) => Ok(RelayValidation::Valid),
        Err(error) if error.as_validation().is_some() => {
            Err(BlockImportError::Other(Box::new(error)))
        }
        Err(error) => {
            // Missing attestation ancestors/snapshots are local availability failures,
            // not evidence that the sending peer supplied an invalid header.
            tracing::debug!(target: "bsc::block_import", %error, "Deferring relay: header context unavailable");
            Ok(RelayValidation::Deferred)
        }
    }
}
