//! BSC's bounded, short-lived holding area for otherwise valid blob transactions with nonce gaps.
//!
//! Reth's normal blob pool remains gapless. Buffered transactions are neither advertised nor
//! executable, and retain their sidecars in memory until they can be revalidated and inserted.

use alloy_consensus::{Transaction, Typed2718};
use alloy_eips::{
    eip4844::{BlobAndProofV1, BlobAndProofV2, BlobCellsAndProofsV1},
    eip7594::{BlobCellMask, BlobTransactionSidecarVariant},
};
use alloy_primitives::{map::AddressSet, Address, TxHash, B256};
use futures::StreamExt;
use parking_lot::Mutex;
use reth_eth_wire_types::HandleMempoolData;
use reth_execution_types::ChangedAccount;
use reth_primitives_traits::Recovered;
use reth_provider::{AccountReader, StateProviderFactory};
use reth_tasks::TaskExecutor;
use reth_transaction_pool::{
    blobstore::{BlobStore, BlobStoreError},
    error::{Eip4844PoolTransactionError, InvalidPoolTransactionError, PoolError, PoolErrorKind},
    pool::{AddedTransactionState, QueuedReason},
    AddedTransactionOutcome, AllPoolTransactions, AllTransactionsEvents, BestTransactions,
    BestTransactionsAttributes, BlockInfo, CanonicalStateUpdate, CoinbaseTipOrdering,
    EthPooledTransaction, GetPooledTransactionLimit, NewBlobSidecar, NewTransactionEvent, Pool,
    PoolResult, PoolSize, PoolTransaction, PropagatedTransactions, TransactionEvent,
    TransactionEvents, TransactionListenerKind, TransactionOrigin, TransactionPool,
    TransactionPoolExt, TransactionValidationOutcome, TransactionValidator, ValidPoolTransaction,
    ValidatingPool,
};
use std::{collections::HashSet, fmt, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, mpsc::Receiver, Notify},
    time::Instant,
};

const MAX_GAPPED: usize = 128;
const MAX_BLOBS_PER_ACCOUNT: usize = 16;
const GAPPED_LIFETIME: Duration = Duration::from_secs(60);

type InnerPool<V, S> = Pool<V, CoinbaseTipOrdering<EthPooledTransaction>, S>;

/// Retained only for a valid blob transaction that might need temporary buffering.
struct GapCandidate {
    transaction: EthPooledTransaction,
    origin: TransactionOrigin,
    state_nonce: u64,
}

impl GapCandidate {
    fn new(
        original: Option<EthPooledTransaction>,
        origin: TransactionOrigin,
        validated: &TransactionValidationOutcome<EthPooledTransaction>,
    ) -> Option<Self> {
        let transaction = original?;
        match validated {
            TransactionValidationOutcome::Valid { state_nonce, .. } => {
                Some(Self { transaction, origin, state_nonce: *state_nonce })
            }
            _ => None,
        }
    }
}

fn ensure_not_buffered(buffer: &[BufferedBlob], hash: TxHash) -> PoolResult<()> {
    if buffer.iter().any(|entry| entry.transaction.hash() == &hash) {
        return Err(PoolError::new(hash, PoolErrorKind::AlreadyImported));
    }
    Ok(())
}

/// Adds Go-BSC-compatible blob gap admission without changing the upstream Reth pool.
pub struct BscTransactionPool<V, S, C> {
    inner: InnerPool<V, S>,
    provider: C,
    buffer: Arc<Mutex<Vec<BufferedBlob>>>,
    wake: Arc<Notify>,
    tasks: TaskExecutor,
}

impl<V, S, C: Clone> Clone for BscTransactionPool<V, S, C> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            provider: self.provider.clone(),
            buffer: self.buffer.clone(),
            wake: self.wake.clone(),
            tasks: self.tasks.clone(),
        }
    }
}

impl<V, S, C> fmt::Debug for BscTransactionPool<V, S, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BscTransactionPool")
            .field("buffered", &self.buffer.lock().len())
            .finish_non_exhaustive()
    }
}

struct BufferedBlob {
    transaction: EthPooledTransaction,
    origin: TransactionOrigin,
    received: Instant,
    listeners: Vec<mpsc::UnboundedSender<TransactionEvent>>,
}

impl BufferedBlob {
    fn admission(&mut self) -> AddedTransactionOutcome {
        AddedTransactionOutcome {
            hash: *self.transaction.hash(),
            state: AddedTransactionState::Queued(QueuedReason::NonceGap),
        }
    }

    fn subscribe(&mut self) -> TransactionEvents {
        let (sender, receiver) = mpsc::unbounded_channel();
        let _ = sender.send(TransactionEvent::Queued);
        self.listeners.push(sender);
        TransactionEvents::new(*self.transaction.hash(), receiver)
    }

    fn finish(self, event: TransactionEvent) {
        for listener in self.listeners {
            let _ = listener.send(event.clone());
        }
    }
}

fn is_nonce_gap(error: &PoolError) -> bool {
    matches!(
        error.kind,
        PoolErrorKind::InvalidTransaction(InvalidPoolTransactionError::Eip4844(
            Eip4844PoolTransactionError::Eip4844NonceGap
        ))
    )
}

/// Go-BSC: min(floor(log10(on-chain nonce + 1)), 16 - pooled count) - buffered count.
fn allowance(nonce: u64, pooled: usize, buffered: usize) -> usize {
    // Use u128 so nonce + 1 cannot overflow, and integer arithmetic at powers of ten.
    let earned = (u128::from(nonce) + 1).ilog10() as usize;
    earned.min(MAX_BLOBS_PER_ACCOUNT.saturating_sub(pooled)).saturating_sub(buffered)
}

fn expire(buffer: &mut Vec<BufferedBlob>) {
    let now = Instant::now();
    let mut i = 0;
    while i < buffer.len() {
        if now.duration_since(buffer[i].received) >= GAPPED_LIFETIME {
            buffer.remove(i).finish(TransactionEvent::Discarded);
        } else {
            i += 1;
        }
    }
}

impl<V, S, C> BscTransactionPool<V, S, C>
where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    S: BlobStore + Clone + 'static,
    C: StateProviderFactory + Clone + Send + Sync + 'static,
{
    pub(super) fn new(inner: InnerPool<V, S>, provider: C, tasks: TaskExecutor) -> Self {
        Self { inner, provider, tasks, buffer: Default::default(), wake: Default::default() }
    }

    /// One worker serializes promotion; admission only locks while inserting into the pool/buffer.
    pub(super) async fn run(self) {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = self.wake.notified() => {},
                _ = interval.tick() => {},
            }
            self.promote().await;
        }
    }

    /// Preserve the upstream insertion result unless an eligible blob nonce gap can be buffered.
    /// The caller holds the buffer lock across pool insertion and this fallback.
    fn buffer_gap<T>(
        &self,
        result: PoolResult<T>,
        candidate: Option<GapCandidate>,
        buffer: &mut Vec<BufferedBlob>,
        response: impl FnOnce(&mut BufferedBlob) -> T,
    ) -> PoolResult<T> {
        let error = match result {
            Ok(result) => {
                if !buffer.is_empty() {
                    self.wake.notify_one();
                }
                return Ok(result);
            }
            Err(error) if is_nonce_gap(&error) => error,
            Err(error) => return Err(error),
        };
        if let Some(GapCandidate { transaction, origin, state_nonce }) = candidate {
            // Also handles duplicate gap transactions within the same batch.
            ensure_not_buffered(buffer, *transaction.hash())?;
            // A later occurrence in the same batch may have succeeded after its filler.
            if self.inner.contains(transaction.hash()) {
                return Err(error);
            }
            let sender = transaction.sender();
            let buffered =
                buffer.iter().filter(|entry| entry.transaction.sender() == sender).count();
            let pooled = self.inner.get_transactions_by_sender(sender).len();
            if buffer.len() < MAX_GAPPED && allowance(state_nonce, pooled, buffered) > 0 {
                let mut entry = BufferedBlob {
                    transaction,
                    origin,
                    received: Instant::now(),
                    listeners: Vec::new(),
                };
                let result = response(&mut entry);
                buffer.push(entry);
                // A filler may already have been inserted, including later in this same batch.
                self.wake.notify_one();
                return Ok(result);
            }
        }
        Err(error)
    }

    /// Insert the validated batch once, in input order, with upstream's single capacity check.
    fn add_validated_batch(
        &self,
        transactions: impl IntoIterator<
            Item = (
                TransactionOrigin,
                TransactionValidationOutcome<EthPooledTransaction>,
                Option<EthPooledTransaction>,
            ),
        >,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        let mut buffer = self.buffer.lock();
        expire(&mut buffer);
        let mut candidates = Vec::new();
        let mut batch = Vec::new();
        for (origin, validated, original) in transactions {
            let duplicate = ensure_not_buffered(&buffer, validated.tx_hash()).err();
            let candidate = GapCandidate::new(original, origin, &validated);
            if duplicate.is_none() {
                batch.push((origin, validated));
            }
            candidates.push((candidate, duplicate));
        }
        let mut results = self.inner.inner().add_transactions_with_origins(batch).into_iter();
        candidates
            .into_iter()
            .map(|(candidate, duplicate)| {
                if let Some(error) = duplicate {
                    return Err(error);
                }
                let result = results.next().expect("one result per inserted transaction");
                self.buffer_gap(result, candidate, &mut buffer, BufferedBlob::admission)
            })
            .collect()
    }

    async fn promote(&self) {
        let senders: AddressSet = {
            let mut buffer = self.buffer.lock();
            expire(&mut buffer);
            buffer.iter().map(|entry| entry.transaction.sender()).collect()
        };
        if senders.is_empty() {
            return;
        }
        let provider = self.provider.clone();
        // Database reads must not block the async runtime. One state snapshot serves the buffer.
        let nonces = self
            .tasks
            .spawn_blocking(move || {
                let state = provider.latest()?;
                senders
                    .into_iter()
                    .map(|sender| {
                        state
                            .basic_account(&sender)
                            .map(|account| (sender, account.unwrap_or_default().nonce))
                    })
                    .collect::<Result<Vec<_>, _>>()
            })
            .await;
        let nonces = match nonces {
            Ok(Ok(nonces)) => nonces,
            error => {
                tracing::debug!(target: "bsc::txpool", ?error, "Cannot refresh blob gap buffer");
                return;
            }
        };
        for (sender, state_nonce) in nonces {
            let mut attempted = HashSet::new();
            loop {
                let candidate = {
                    let mut buffer = self.buffer.lock();
                    expire(&mut buffer);
                    let mut i = 0;
                    while i < buffer.len() {
                        if buffer[i].transaction.sender() == sender &&
                            buffer[i].transaction.nonce() < state_nonce
                        {
                            buffer.remove(i).finish(TransactionEvent::Discarded);
                        } else {
                            i += 1;
                        }
                    }
                    let next = self
                        .inner
                        .get_highest_consecutive_transaction_by_sender(sender, state_nonce)
                        .map_or(state_nonce, |tx| tx.nonce().saturating_add(1));
                    // min_by_key keeps arrival order for equal nonces, like Go-BSC's stable sort.
                    buffer
                        .iter()
                        .filter(|entry| {
                            entry.transaction.sender() == sender &&
                                entry.transaction.nonce() <= next &&
                                !attempted.contains(entry.transaction.hash())
                        })
                        .min_by_key(|entry| entry.transaction.nonce())
                        .map(|entry| (entry.transaction.clone(), entry.origin, entry.received))
                };
                let Some((transaction, origin, received)) = candidate else {
                    break;
                };
                let hash = *transaction.hash();
                let nonce = transaction.nonce();
                attempted.insert(hash);
                let validated =
                    self.inner.validator().validate_transaction(origin, transaction).await;
                let mut buffer = self.buffer.lock();
                expire(&mut buffer);
                // A concurrent removal/expiry must not resurrect the transaction after validation.
                let Some(index) = buffer.iter().position(|entry| {
                    entry.transaction.hash() == &hash && entry.received == received
                }) else {
                    continue;
                };
                // Fillers may have used the sender's remaining slots while this tx waited.
                if self.inner.get_transactions_by_sender(sender).len() >= MAX_BLOBS_PER_ACCOUNT &&
                    self.inner.get_transaction_by_sender_and_nonce(sender, nonce).is_none()
                {
                    buffer.remove(index).finish(TransactionEvent::Discarded);
                    continue;
                }
                let subscribe = !buffer[index].listeners.is_empty();
                let result = if subscribe {
                    self.inner.inner().add_transaction_and_subscribe(origin, validated).map(Some)
                } else {
                    self.inner
                        .inner()
                        .add_transactions(origin, std::iter::once(validated))
                        .pop()
                        .expect("one input has one result")
                        .map(|_| None)
                };
                match result {
                    Ok(events) => {
                        let entry = buffer.remove(index);
                        if let Some(mut events) = events {
                            self.tasks.spawn_task(async move {
                                let mut listeners = entry.listeners;
                                while let Some(event) = events.next().await {
                                    listeners
                                        .retain(|listener| listener.send(event.clone()).is_ok());
                                    if listeners.is_empty() || event.is_final() {
                                        break;
                                    }
                                }
                            });
                        }
                    }
                    Err(error) if is_nonce_gap(&error) => {}
                    Err(error) => {
                        tracing::debug!(target: "bsc::txpool", ?hash, %error, "Dropping buffered blob after revalidation");
                        buffer.remove(index).finish(TransactionEvent::Invalid);
                    }
                }
            }
        }
    }

    fn remove_buffered(&self, mut predicate: impl FnMut(&EthPooledTransaction) -> bool) {
        let mut buffer = self.buffer.lock();
        let mut i = 0;
        while i < buffer.len() {
            if predicate(&buffer[i].transaction) {
                buffer.remove(i).finish(TransactionEvent::Discarded);
            } else {
                i += 1;
            }
        }
    }
}

mod traits;

#[cfg(test)]
mod tests;
