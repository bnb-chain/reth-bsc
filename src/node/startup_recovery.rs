//! Preserve persisted finality across Reth's destructive startup consistency recovery.
//!
//! This protects the finalized checkpoint already committed by Reth, not a newer in-memory
//! checkpoint awaiting its next persistence batch. The marker survives successful recovery:
//! otherwise another restart could lose it before the Engine persists restored finality.

use crate::node::engine_api::payload::BscPayloadTypes;
use alloy_eips::BlockNumHash;
use alloy_rpc_types_engine::{ForkchoiceState, PayloadStatusEnum};
use reth_engine_primitives::ConsensusEngineHandle;
use reth_node_builder::sync::{BackfillContext, BackfillSyncBuilder, PipelineBackfill};
use reth_provider::{
    providers::ProviderNodeTypes, BlockIdReader, BlockNumReader, ChainStateBlockReader, DBProvider,
    DatabaseProviderFactory, HeaderProvider, MetadataProvider, MetadataWriter, ProviderFactory,
};
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        OnceLock,
    },
    time::Duration,
};

const METADATA_KEY: &str = "bsc_startup_finalized";
const RETRY_INTERVAL: Duration = Duration::from_secs(3);
const ENGINE_TIMEOUT: Duration = Duration::from_secs(10);
static RECOVERY: OnceLock<StartupRecovery> = OnceLock::new();

/// Delegates ordinary staged sync, capturing finality before consistency checks can unwind it.
#[derive(Debug, Default)]
pub struct BscRecoveryBackfill;

impl<N: ProviderNodeTypes, C> BackfillSyncBuilder<N, C> for BscRecoveryBackfill {
    type Backfill = <PipelineBackfill as BackfillSyncBuilder<N, C>>::Backfill;

    fn recover(&mut self, factory: &ProviderFactory<N>) -> eyre::Result<()> {
        let checkpoint = capture_checkpoint(factory)?;
        RECOVERY
            .set(StartupRecovery::new(checkpoint))
            .map_err(|_| eyre::eyre!("BSC startup recovery already initialized"))?;
        Ok(())
    }

    fn build(self, ctx: BackfillContext<N, C>) -> eyre::Result<Self::Backfill> {
        PipelineBackfill.build(ctx)
    }
}

struct StartupRecovery {
    checkpoint: Option<BlockNumHash>,
    ready: AtomicBool,
}

impl StartupRecovery {
    fn new(checkpoint: Option<BlockNumHash>) -> Self {
        Self { checkpoint, ready: AtomicBool::new(checkpoint.is_none()) }
    }

    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    /// Header tables can be ahead of execution after restart. Check the executed canonical head
    /// first, then its checkpoint hash and the Engine's restored finalized marker.
    fn recovery_target<P>(&self, provider: &P) -> eyre::Result<Option<ForkchoiceState>>
    where
        P: BlockNumReader + HeaderProvider + BlockIdReader,
    {
        let Some(checkpoint) = self.checkpoint else { return Ok(None) };
        let head = provider.chain_info()?;
        let head_hash = if head.best_number < checkpoint.number {
            checkpoint.hash
        } else {
            let header = provider.sealed_header(checkpoint.number)?.ok_or_else(|| {
                eyre::eyre!(
                    "BSC startup recovery: missing canonical header at finalized #{} ({})",
                    checkpoint.number,
                    checkpoint.hash,
                )
            })?;
            eyre::ensure!(
                header.hash() == checkpoint.hash,
                "BSC startup recovery: finalized hash conflict at #{}: expected {}, got {}",
                checkpoint.number,
                checkpoint.hash,
                header.hash(),
            );
            if provider.finalized_block_num_hash()?.is_some_and(|finalized| {
                finalized.number >= checkpoint.number &&
                    finalized.number <= head.best_number &&
                    (finalized.number != checkpoint.number || finalized.hash == checkpoint.hash)
            }) {
                return Ok(None);
            }
            head.best_hash
        };
        Ok(Some(ForkchoiceState {
            head_block_hash: head_hash,
            safe_block_hash: checkpoint.hash,
            finalized_block_hash: checkpoint.hash,
        }))
    }

    async fn run<P>(
        &self,
        provider: P,
        engine: ConsensusEngineHandle<BscPayloadTypes>,
    ) -> eyre::Result<()>
    where
        P: BlockNumReader + HeaderProvider + BlockIdReader,
    {
        loop {
            let Some(target) = self.recovery_target(&provider)? else {
                self.ready.store(true, Ordering::Release);
                tracing::info!(target: "bsc::startup_recovery", checkpoint = ?self.checkpoint,
                    "Persisted finalized history restored; mining and voting enabled");
                return Ok(());
            };
            tracing::warn!(target: "bsc::startup_recovery", checkpoint = ?self.checkpoint,
                "Waiting to restore persisted finalized history; mining and voting disabled");
            match tokio::time::timeout(ENGINE_TIMEOUT, engine.fork_choice_updated(target, None)).await {
                Ok(Ok(response)) => match response.payload_status.status {
                    PayloadStatusEnum::Invalid { validation_error } => eyre::bail!(
                        "BSC startup recovery: Engine rejected finalized recovery: {validation_error}"
                    ),
                    // VALID acknowledges an FCU. Re-read execution and finality before signing.
                    PayloadStatusEnum::Valid if self.recovery_target(&provider)?.is_none() => continue,
                    _ => {}
                },
                Ok(Err(error)) => eyre::bail!("BSC startup recovery: Engine request failed: {error}"),
                Err(_) => tracing::warn!(target: "bsc::startup_recovery",
                    "Finalized recovery request timed out; mining and voting remain disabled"),
            }
            tokio::time::sleep(RETRY_INTERVAL).await;
        }
    }
}

/// Shared signing gate. The launcher installs it before any BSC components can start.
pub(crate) fn is_ready() -> bool {
    RECOVERY.get().is_none_or(StartupRecovery::is_ready)
}

/// Restore the saved checkpoint through the running Engine before enabling local signing.
/// Returns after recovery; the caller runs this as a critical task so conflicts stop the node.
pub async fn run<P>(provider: P, engine: ConsensusEngineHandle<BscPayloadTypes>) -> eyre::Result<()>
where
    P: BlockNumReader + HeaderProvider + BlockIdReader,
{
    let recovery = RECOVERY
        .get()
        .ok_or_else(|| eyre::eyre!("BSC startup recovery was not initialized by the launcher"))?;
    if !recovery.is_ready() {
        recovery.run(provider, engine).await?;
    }
    Ok(())
}

fn capture_checkpoint<N: ProviderNodeTypes>(
    factory: &ProviderFactory<N>,
) -> eyre::Result<Option<BlockNumHash>> {
    let provider = factory.database_provider_ro()?;
    let saved = provider
        .get_metadata(METADATA_KEY)?
        .map(|bytes| serde_json::from_slice::<BlockNumHash>(&bytes))
        .transpose()
        .map_err(|error| {
            eyre::eyre!("BSC startup recovery: invalid finalized metadata: {error}")
        })?;
    if let Some(checkpoint) = saved {
        eyre::ensure!(
            checkpoint.number > 0 && !checkpoint.hash.is_zero(),
            "BSC startup recovery: invalid finalized checkpoint"
        );
    }
    let finalized = provider.last_finalized_block_number()?.filter(|number| *number > 0);
    let checkpoint = match (saved, finalized) {
        (Some(saved), Some(number)) if number >= saved.number => {
            if let Some(previous) = provider.sealed_header(saved.number)? {
                eyre::ensure!(
                    previous.hash() == saved.hash,
                    "BSC startup recovery: finalized hash conflict at #{}: expected {}, got {}",
                    saved.number,
                    saved.hash,
                    previous.hash(),
                );
            } else {
                // Storage healing may not have lowered ChainState yet. The saved hash is enough
                // to recover this checkpoint, but not to authenticate a newer one.
                eyre::ensure!(
                    number == saved.number,
                    "BSC startup recovery: missing saved finalized header #{}",
                    saved.number
                );
            }
            if number == saved.number {
                Some(saved)
            } else {
                Some(read_checkpoint(&provider, number)?)
            }
        }
        (Some(saved), _) => Some(saved),
        (None, Some(number)) => Some(read_checkpoint(&provider, number)?),
        (None, None) => None,
    };
    drop(provider);
    if checkpoint != saved {
        let provider = factory.database_provider_rw()?;
        provider.write_metadata(
            METADATA_KEY,
            serde_json::to_vec(&checkpoint.expect("new checkpoint exists"))?,
        )?;
        provider.commit()?;
    }
    Ok(checkpoint)
}

fn read_checkpoint(provider: &impl HeaderProvider, number: u64) -> eyre::Result<BlockNumHash> {
    let header = provider.sealed_header(number)?.ok_or_else(|| {
        eyre::eyre!("BSC startup recovery: missing persisted finalized header #{number}")
    })?;
    Ok(BlockNumHash { number, hash: header.hash() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::Header;
    use alloy_primitives::B256;
    use alloy_rpc_types_engine::PayloadStatus;
    use reth_engine_primitives::{BeaconEngineMessage, OnForkChoiceUpdated};
    use reth_primitives_traits::SealedHeader;
    use reth_provider::{
        providers::BlockchainProvider,
        test_utils::{create_test_provider_factory, insert_headers, MockNodeTypesWithDB},
        CanonChainTracker, ChainStateBlockWriter, StaticFileProviderFactory, StaticFileSegment,
        StaticFileWriter,
    };
    use tokio::sync::mpsc;

    fn header(number: u64) -> SealedHeader<Header> {
        let mut parent = B256::ZERO;
        for number in 0..number {
            parent = Header { number, parent_hash: parent, ..Default::default() }.hash_slow();
        }
        SealedHeader::seal_slow(Header { number, parent_hash: parent, ..Default::default() })
    }

    fn checkpoint(number: u64) -> BlockNumHash {
        BlockNumHash { number, hash: header(number).hash() }
    }

    fn write_header(factory: &ProviderFactory<MockNodeTypesWithDB>, header: &SealedHeader<Header>) {
        let files = factory.static_file_provider();
        let mut start = files
            .get_highest_static_file_block(StaticFileSegment::Headers)
            .map_or(0, |number| number + 1);
        if start > header.number {
            let mut writer = files.latest_writer(StaticFileSegment::Headers).unwrap();
            writer.prune_headers(start - header.number).unwrap();
            writer.commit().unwrap();
            start = header.number;
        }
        let mut headers = (start..header.number).map(self::header).collect::<Vec<_>>();
        headers.push(header.clone());
        insert_headers(factory, &headers);
    }

    fn write_finalized(factory: &ProviderFactory<MockNodeTypesWithDB>, number: u64) {
        let provider = factory.database_provider_rw().unwrap();
        provider.save_finalized_block_number(number).unwrap();
        provider.commit().unwrap();
    }

    #[test]
    fn captures_before_unwind_and_survives_repeated_restart() {
        let factory = create_test_provider_factory();
        // The persisted block tip is ahead of finality, which is ahead of the partial trie.
        write_header(&factory, &header(64));
        write_finalized(&factory, 21);
        assert_eq!(capture_checkpoint(&factory).unwrap(), Some(checkpoint(21)));

        // Reth's startup unwind lowers finalized and removes the original finalized header.
        let provider = factory.database_provider_rw().unwrap();
        provider.save_finalized_block_number(16).unwrap();
        provider.commit().unwrap();
        let files = factory.static_file_provider();
        let mut writer = files.latest_writer(StaticFileSegment::Headers).unwrap();
        writer.prune_headers(64 - 16).unwrap();
        writer.commit().unwrap();
        drop(writer);
        assert_eq!(capture_checkpoint(&factory).unwrap(), Some(checkpoint(21)));
        assert_eq!(capture_checkpoint(&factory).unwrap(), Some(checkpoint(21)));

        // A stale ChainState pointer can outlive its static-file header before storage healing.
        write_finalized(&factory, 21);
        assert_eq!(capture_checkpoint(&factory).unwrap(), Some(checkpoint(21)));
        write_finalized(&factory, 22);
        assert_eq!(
            capture_checkpoint(&factory).unwrap_err().to_string(),
            "BSC startup recovery: missing saved finalized header #21"
        );
        write_finalized(&factory, 16);

        // The executed head, rather than stale persisted headers, determines recovery readiness.
        let provider = BlockchainProvider::with_latest(factory, header(16)).unwrap();
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        let target = recovery.recovery_target(&provider).unwrap().unwrap();
        assert_eq!(target.head_block_hash, checkpoint(21).hash);
        assert_eq!(target.finalized_block_hash, checkpoint(21).hash);
        assert!(!recovery.is_ready());
    }

    #[test]
    fn advances_saved_checkpoint_only_on_matching_history() {
        let factory = create_test_provider_factory();
        write_header(&factory, &header(21));
        write_finalized(&factory, 21);
        capture_checkpoint(&factory).unwrap();
        write_header(&factory, &header(30));
        write_finalized(&factory, 30);
        assert_eq!(capture_checkpoint(&factory).unwrap(), Some(checkpoint(30)));

        let conflicting =
            SealedHeader::seal_slow(Header { number: 30, timestamp: 1, ..Default::default() });
        write_header(&factory, &conflicting);
        assert_eq!(
            capture_checkpoint(&factory).unwrap_err().to_string(),
            format!(
                "BSC startup recovery: finalized hash conflict at #30: expected {}, got {}",
                checkpoint(30).hash,
                conflicting.hash(),
            )
        );
        let saved =
            factory.database_provider_ro().unwrap().get_metadata(METADATA_KEY).unwrap().unwrap();
        assert_eq!(serde_json::from_slice::<BlockNumHash>(&saved).unwrap(), checkpoint(30));
    }

    #[test]
    fn first_start_and_genesis_finality_need_no_marker_or_sync() {
        let factory = create_test_provider_factory();
        assert_eq!(capture_checkpoint(&factory).unwrap(), None);
        write_finalized(&factory, 0);
        assert_eq!(capture_checkpoint(&factory).unwrap(), None);
        assert!(factory
            .database_provider_ro()
            .unwrap()
            .get_metadata(METADATA_KEY)
            .unwrap()
            .is_none());
        assert!(StartupRecovery::new(None).is_ready());
    }

    #[test]
    fn refuses_missing_finalized_header_and_invalid_metadata() {
        let factory = create_test_provider_factory();
        write_finalized(&factory, 21);
        assert_eq!(
            capture_checkpoint(&factory).unwrap_err().to_string(),
            "BSC startup recovery: missing persisted finalized header #21"
        );
        let provider = factory.database_provider_rw().unwrap();
        provider.write_metadata(METADATA_KEY, b"{".to_vec()).unwrap();
        provider.commit().unwrap();
        assert_eq!(capture_checkpoint(&factory).unwrap_err().to_string(),
            "BSC startup recovery: invalid finalized metadata: EOF while parsing an object at line 1 column 1");
        let provider = factory.database_provider_rw().unwrap();
        provider
            .write_metadata(
                METADATA_KEY,
                serde_json::to_vec(&BlockNumHash { number: 21, hash: B256::ZERO }).unwrap(),
            )
            .unwrap();
        provider.commit().unwrap();
        assert_eq!(
            capture_checkpoint(&factory).unwrap_err().to_string(),
            "BSC startup recovery: invalid finalized checkpoint"
        );
    }

    #[test]
    fn header_presence_and_height_do_not_prove_executed_recovery() {
        let factory = create_test_provider_factory();
        write_header(&factory, &header(21));
        let provider = BlockchainProvider::with_latest(factory, header(16)).unwrap();
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        assert!(recovery.recovery_target(&provider).unwrap().is_some());
        assert!(!recovery.is_ready());

        provider.set_canonical_head(header(21));
        assert!(
            recovery.recovery_target(&provider).unwrap().is_some(),
            "finality still needs restoring"
        );
        provider.set_finalized(header(21));
        assert!(recovery.recovery_target(&provider).unwrap().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn syncing_valid_and_connected_engine_cannot_bypass_recovery() {
        let factory = create_test_provider_factory();
        write_header(&factory, &header(21));
        let provider = BlockchainProvider::with_latest(factory, header(16)).unwrap();
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        let (sender, mut requests) = mpsc::unbounded_channel();
        let work = recovery.run(provider.clone(), ConsensusEngineHandle::new(sender));
        tokio::pin!(work);

        for status in [PayloadStatusEnum::Syncing, PayloadStatusEnum::Valid] {
            let request = tokio::select! {
                request = requests.recv() => request.unwrap(),
                result = &mut work => panic!("recovery completed early: {result:?}"),
            };
            let BeaconEngineMessage::ForkchoiceUpdated { state, tx, .. } = request else {
                panic!("expected FCU")
            };
            assert_eq!(state.head_block_hash, checkpoint(21).hash);
            assert_eq!(state.safe_block_hash, checkpoint(21).hash);
            assert_eq!(state.finalized_block_hash, checkpoint(21).hash);
            tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(status, None)))).unwrap();
            assert!(futures::poll!(&mut work).is_pending());
            assert!(!recovery.is_ready());
            tokio::time::advance(RETRY_INTERVAL).await;
        }

        // Replaying the correct history and restoring Engine finality releases signing.
        provider.set_canonical_head(header(21));
        provider.set_finalized(header(21));
        work.await.unwrap();
        assert!(recovery.is_ready());
    }

    #[tokio::test]
    async fn wrong_hash_at_finalized_height_fails_closed() {
        let factory = create_test_provider_factory();
        let conflicting =
            SealedHeader::seal_slow(Header { number: 21, timestamp: 1, ..Default::default() });
        write_header(&factory, &conflicting);
        let provider = BlockchainProvider::with_latest(factory, conflicting.clone()).unwrap();
        provider.set_finalized(conflicting.clone());
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        let (sender, mut requests) = mpsc::unbounded_channel();
        assert_eq!(
            recovery
                .run(provider, ConsensusEngineHandle::new(sender))
                .await
                .unwrap_err()
                .to_string(),
            format!(
                "BSC startup recovery: finalized hash conflict at #21: expected {}, got {}",
                checkpoint(21).hash,
                conflicting.hash(),
            )
        );
        assert!(!recovery.is_ready());
        assert!(requests.try_recv().is_err());
    }

    #[tokio::test]
    async fn already_restored_startup_needs_no_engine_request_or_runtime_write() {
        let factory = create_test_provider_factory();
        write_header(&factory, &header(21));
        write_finalized(&factory, 21);
        let saved = capture_checkpoint(&factory).unwrap();
        let provider = BlockchainProvider::with_latest(factory.clone(), header(64)).unwrap();
        let recovery = StartupRecovery::new(saved);
        let (sender, mut requests) = mpsc::unbounded_channel();
        recovery.run(provider.clone(), ConsensusEngineHandle::new(sender)).await.unwrap();
        assert!(recovery.is_ready());
        assert!(requests.try_recv().is_err());

        // Readiness remains an atomic load as the ordinary executed head advances.
        for number in 65..=100 {
            provider.set_canonical_head(header(number));
            assert!(recovery.is_ready());
        }
        assert_eq!(
            factory.database_provider_ro().unwrap().last_finalized_block_number().unwrap(),
            Some(21)
        );
        assert_eq!(capture_checkpoint(&factory).unwrap(), saved);
    }

    #[tokio::test]
    async fn engine_rejection_keeps_signing_disabled() {
        let provider =
            BlockchainProvider::with_latest(create_test_provider_factory(), header(16)).unwrap();
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        let (sender, mut requests) = mpsc::unbounded_channel();
        let work = recovery.run(provider, ConsensusEngineHandle::new(sender));
        tokio::pin!(work);
        let request = tokio::select! {
            request = requests.recv() => request.unwrap(),
            result = &mut work => panic!("premature recovery: {result:?}"),
        };
        let BeaconEngineMessage::ForkchoiceUpdated { tx, .. } = request else {
            panic!("expected FCU")
        };
        tx.send(Ok(OnForkChoiceUpdated::valid(PayloadStatus::new(
            PayloadStatusEnum::Invalid { validation_error: "checkpoint execution failed".into() },
            None,
        ))))
        .unwrap();
        assert_eq!(
            work.await.unwrap_err().to_string(),
            "BSC startup recovery: Engine rejected finalized recovery: checkpoint execution failed"
        );
        assert!(!recovery.is_ready());
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_retries_the_same_checkpoint_without_enabling_signing() {
        let provider =
            BlockchainProvider::with_latest(create_test_provider_factory(), header(16)).unwrap();
        let recovery = StartupRecovery::new(Some(checkpoint(21)));
        let (sender, mut requests) = mpsc::unbounded_channel();
        let work = recovery.run(provider, ConsensusEngineHandle::new(sender));
        tokio::pin!(work);
        let request = tokio::select! {
            request = requests.recv() => request.unwrap(),
            result = &mut work => panic!("premature recovery: {result:?}"),
        };
        let BeaconEngineMessage::ForkchoiceUpdated { tx: unanswered, .. } = request else {
            panic!("expected FCU")
        };
        tokio::time::advance(ENGINE_TIMEOUT).await;
        assert!(futures::poll!(&mut work).is_pending());
        assert!(!recovery.is_ready());
        tokio::time::advance(RETRY_INTERVAL).await;
        let retry = tokio::select! {
            request = requests.recv() => request.unwrap(),
            result = &mut work => panic!("timeout bypassed recovery: {result:?}"),
        };
        assert!(matches!(retry, BeaconEngineMessage::ForkchoiceUpdated { state, .. }
            if state.head_block_hash == checkpoint(21).hash && state.finalized_block_hash == checkpoint(21).hash));
        assert!(!recovery.is_ready());
        drop(unanswered);
    }

    #[tokio::test]
    async fn cold_start_does_not_need_a_head_or_engine() {
        let provider = reth_provider::test_utils::MockEthProvider::default();
        let recovery = StartupRecovery::new(None);
        let (sender, mut requests) = mpsc::unbounded_channel();
        recovery.run(provider, ConsensusEngineHandle::new(sender)).await.unwrap();
        assert!(recovery.is_ready());
        assert!(requests.try_recv().is_err());
    }
}
