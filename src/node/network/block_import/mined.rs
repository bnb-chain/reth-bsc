//! Confirm engine admission before selecting a locally executed block as the head.

use crate::{
    consensus::ParliaConsensusErr,
    node::{engine_api::payload::BscPayloadTypes, evm::config::BscEvmConfig},
    BscPrimitives,
};
use alloy_consensus::BlockHeader;
use reth_chain_state::ExecutedBlock;
use reth_engine_primitives::BeaconEngineMessage;
use reth_engine_tree::engine::EngineApiRequest;
use std::{future::Future, pin::Pin, time::Duration};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

// Keep the provider generic so the same submission path can be exercised against the real
// engine handler backed by test storage, without changing the engine request protocol.
type EngineTx<P> =
    UnboundedSender<EngineApiRequest<BscPayloadTypes, BscPrimitives, P, BscEvmConfig>>;

/// Resolves once the engine has admitted the inserted block, and fails if it discarded it.
pub(super) type AdmissionGate =
    Pin<Box<dyn Future<Output = Result<(), ParliaConsensusErr>> + Send>>;

const ADMISSION_TIMEOUT: Duration = Duration::from_secs(5);

/// `InsertExecutedBlock` is fire-and-forget and can silently discard a block whose height is
/// already canonical. Query the exact hash through the SAME channel, after insertion, so FIFO
/// ordering makes the query a barrier. A send, a yield, or parent-derived TD is not admission.
/// Never issue an FCU for a block the engine could not resolve after processing its insertion.
///
/// `update` receives the barrier as an [`AdmissionGate`] and must await it immediately before
/// selecting the head, not before evaluating fork choice. Waiting up front delayed the whole
/// evaluation into the busy window after the insert and roughly tripled FCU latency, which
/// gates the miner's next work.
pub(super) async fn insert_then_update<P, F, Fut>(
    engine_tx: Option<EngineTx<P>>,
    block: ExecutedBlock<BscPrimitives>,
    update: F,
) -> eyre::Result<()>
where
    F: FnOnce(AdmissionGate) -> Fut,
    Fut: Future<Output = Result<(), ParliaConsensusErr>>,
{
    let engine_tx = engine_tx.ok_or_else(|| eyre::eyre!("engine insertion channel unavailable"))?;
    let number = block.recovered_block().number();
    let hash = block.recovered_block().hash();
    engine_tx
        .send(EngineApiRequest::InsertExecutedBlock(block))
        .map_err(|_| eyre::eyre!("engine insertion channel closed"))?;

    let (tx, rx) = oneshot::channel();
    engine_tx
        .send(EngineApiRequest::Beacon(BeaconEngineMessage::QueryTd { number, hash, tx }))
        .map_err(|_| eyre::eyre!("engine admission query channel closed"))?;

    let gate: AdmissionGate = Box::pin(async move {
        let td = tokio::time::timeout(ADMISSION_TIMEOUT, rx)
            .await
            .map_err(|_| not_admitted("engine admission query timed out"))?
            .map_err(|_| not_admitted("engine admission query response dropped"))?
            .map_err(ParliaConsensusErr::internal)?;
        td.map(|_| ()).ok_or_else(|| not_admitted("engine could not resolve mined block TD"))
    });

    update(gate).await?;
    Ok(())
}

fn not_admitted(reason: &str) -> ParliaConsensusErr {
    ParliaConsensusErr::Internal(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shared::BscEngineApiTx;
    use alloy_consensus::Header;
    use alloy_primitives::U256;
    use reth_chain_state::ComputedTrieData;
    use reth_primitives_traits::RecoveredBlock;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use tokio::sync::mpsc;

    fn executed_block() -> ExecutedBlock<BscPrimitives> {
        ExecutedBlock::new(
            Arc::new(RecoveredBlock::new_unhashed(
                crate::BscBlock {
                    header: Header { number: 101, ..Default::default() },
                    body: Default::default(),
                },
                Vec::new(),
            )),
            Arc::default(),
            ComputedTrieData::default(),
        )
    }

    /// Stands in for `update_forkchoice_gated`: evaluate first, then select only through the
    /// gate. (The returned future has the same boxed type as the gate itself.)
    fn gated_update(
        evaluated: Arc<AtomicBool>,
        selected: Arc<AtomicBool>,
    ) -> impl FnOnce(AdmissionGate) -> AdmissionGate {
        move |gate| {
            Box::pin(async move {
                evaluated.store(true, Ordering::SeqCst);
                gate.await?;
                selected.store(true, Ordering::SeqCst);
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn mined_fcu_waits_for_exact_hash_admission_on_insertion_channel() {
        let (tx, mut rx): (BscEngineApiTx, _) = mpsc::unbounded_channel();
        let block = executed_block();
        let hash = block.recovered_block().hash();
        let evaluated = Arc::new(AtomicBool::new(false));
        let selected = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(insert_then_update(
            Some(tx),
            block,
            gated_update(evaluated.clone(), selected.clone()),
        ));

        // Simulate an engine stalled before insertion. FCU must not depend on scheduling yields.
        let Some(EngineApiRequest::InsertExecutedBlock(inserted)) = rx.recv().await else {
            panic!("insertion must precede its admission query");
        };
        assert_eq!(inserted.recovered_block().hash(), hash);
        let Some(EngineApiRequest::Beacon(BeaconEngineMessage::QueryTd {
            number,
            hash: queried_hash,
            tx,
        })) = rx.recv().await
        else {
            panic!("admission query missing");
        };
        assert_eq!((number, queried_hash), (101, hash));
        // Fork-choice evaluation overlaps the barrier; only selection waits for the answer.
        assert!(evaluated.load(Ordering::SeqCst));
        assert!(!selected.load(Ordering::SeqCst));
        assert!(!task.is_finished());

        tx.send(Ok(Some(U256::from(123)))).unwrap();
        task.await.unwrap().unwrap();
        assert!(selected.load(Ordering::SeqCst));
    }

    #[tokio::test(start_paused = true)]
    async fn mined_fcu_is_skipped_when_admission_cannot_be_confirmed() {
        for outcome in ["discarded", "no_td", "dropped", "timeout"] {
            let (tx, mut rx): (BscEngineApiTx, _) = mpsc::unbounded_channel();
            let block = executed_block();
            let hash = block.recovered_block().hash();
            let evaluated = Arc::new(AtomicBool::new(false));
            let selected = Arc::new(AtomicBool::new(false));
            let task = tokio::spawn(insert_then_update(
                Some(tx),
                block,
                gated_update(evaluated.clone(), selected.clone()),
            ));
            assert!(matches!(rx.recv().await, Some(EngineApiRequest::InsertExecutedBlock(_))));
            let Some(EngineApiRequest::Beacon(BeaconEngineMessage::QueryTd { tx, .. })) =
                rx.recv().await
            else {
                panic!("admission query missing");
            };
            // Evaluation already finished, which must not be mistaken for admission.
            assert!(evaluated.load(Ordering::SeqCst), "{outcome}");
            match outcome {
                // The incident: another block at height 101 wins, insertion is discarded,
                // and querying our hash returns HeaderNotFound. Parent TD is irrelevant.
                "discarded" => tx
                    .send(Err(reth_provider::ProviderError::HeaderNotFound(hash.into()).into()))
                    .unwrap(),
                "no_td" => tx.send(Ok(None)).unwrap(),
                "dropped" => drop(tx),
                "timeout" => {
                    tokio::time::advance(ADMISSION_TIMEOUT + Duration::from_secs(1)).await;
                    assert!(task.await.unwrap().is_err());
                    assert!(!selected.load(Ordering::SeqCst));
                    drop(tx);
                    continue;
                }
                _ => unreachable!(),
            }
            assert!(task.await.unwrap().is_err(), "{outcome}");
            assert!(!selected.load(Ordering::SeqCst), "{outcome} must not select a head");
        }
    }

    #[tokio::test]
    async fn mined_fcu_is_skipped_without_an_insertion_channel() {
        let evaluated = Arc::new(AtomicBool::new(false));
        let selected = Arc::new(AtomicBool::new(false));
        let missing_tx: Option<BscEngineApiTx> = None;
        let result = insert_then_update(
            missing_tx,
            executed_block(),
            gated_update(evaluated.clone(), selected.clone()),
        )
        .await;
        assert!(result.is_err());
        assert!(!evaluated.load(Ordering::SeqCst));
        assert!(!selected.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
#[path = "mined_engine_tests.rs"]
mod engine_tests;
