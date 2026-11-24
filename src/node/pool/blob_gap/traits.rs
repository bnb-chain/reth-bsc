//! TransactionPool integration. Queries and announcements exclude the holding buffer.

use super::*;

impl<V, S, C> TransactionPool for BscTransactionPool<V, S, C>
where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    S: BlobStore + Clone + 'static,
    C: StateProviderFactory + Clone + Send + Sync + 'static,
{
    type Transaction = EthPooledTransaction;

    async fn add_transaction(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<AddedTransactionOutcome> {
        match self.insert(origin, transaction, false).await? {
            Submission::Added(outcome) => Ok(outcome),
            Submission::Subscribed(_) => unreachable!("subscription was not requested"),
        }
    }

    async fn add_transaction_and_subscribe(
        &self,
        origin: TransactionOrigin,
        transaction: Self::Transaction,
    ) -> PoolResult<TransactionEvents> {
        match self.insert(origin, transaction, true).await? {
            Submission::Subscribed(events) => Ok(events),
            Submission::Added(_) => unreachable!("subscription was requested"),
        }
    }

    async fn add_transactions(
        &self,
        origin: TransactionOrigin,
        transactions: Vec<Self::Transaction>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        self.add_transactions_with_origins(
            transactions.into_iter().map(|tx| (origin, tx)).collect(),
        )
        .await
    }

    async fn add_transactions_with_origins(
        &self,
        transactions: Vec<(TransactionOrigin, Self::Transaction)>,
    ) -> Vec<PoolResult<AddedTransactionOutcome>> {
        // Validate concurrently as upstream does; insertion holds only a short synchronous lock.
        futures::future::join_all(
            transactions.into_iter().map(|(origin, tx)| self.add_transaction(origin, tx)),
        )
        .await
    }

    fn transaction_event_listener(&self, tx_hash: TxHash) -> Option<TransactionEvents> {
        let mut buffer = self.buffer.lock();
        expire(&mut buffer);
        if let Some(entry) = buffer.iter_mut().find(|entry| entry.transaction.hash() == &tx_hash) {
            return Some(entry.subscribe());
        }
        self.inner.transaction_event_listener(tx_hash)
    }

    fn contains(&self, tx_hash: &TxHash) -> bool {
        self.buffer.lock().iter().any(|entry| {
            entry.transaction.hash() == tx_hash && entry.received.elapsed() < GAPPED_LIFETIME
        }) || self.inner.contains(tx_hash)
    }

    fn retain_unknown<A: HandleMempoolData>(&self, announcement: &mut A) {
        announcement.retain_by_hash(|hash| !self.contains(hash));
    }

    fn retain_contains<A: HandleMempoolData>(&self, announcement: &mut A) {
        announcement.retain_by_hash(|hash| self.contains(hash));
    }

    fn remove_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.remove_buffered(|tx| hashes.contains(tx.hash()));
        self.inner.remove_transactions(hashes)
    }

    fn remove_transactions_and_descendants(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        let mut roots: Vec<_> =
            self.inner.get_all(hashes.clone()).iter().map(|tx| (tx.sender(), tx.nonce())).collect();
        roots.extend(
            self.buffer
                .lock()
                .iter()
                .filter(|entry| hashes.contains(entry.transaction.hash()))
                .map(|entry| (entry.transaction.sender(), entry.transaction.nonce())),
        );
        self.remove_buffered(|tx| {
            roots.iter().any(|(sender, nonce)| tx.sender() == *sender && tx.nonce() >= *nonce)
        });
        self.inner.remove_transactions_and_descendants(hashes)
    }

    fn remove_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.remove_buffered(|tx| tx.sender() == sender);
        self.inner.remove_transactions_by_sender(sender)
    }

    fn prune_transactions(
        &self,
        hashes: Vec<TxHash>,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.remove_buffered(|tx| hashes.contains(tx.hash()));
        let result = self.inner.prune_transactions(hashes);
        self.wake.notify_one();
        result
    }
    // Pool queries deliberately exclude the temporary blob gap buffer.
    fn pool_size(&self) -> PoolSize {
        self.inner.pool_size()
    }

    fn block_info(&self) -> BlockInfo {
        self.inner.block_info()
    }

    fn all_transactions_event_listener(&self) -> AllTransactionsEvents<Self::Transaction> {
        self.inner.all_transactions_event_listener()
    }

    fn pending_transactions_listener_for(&self, kind: TransactionListenerKind) -> Receiver<TxHash> {
        self.inner.pending_transactions_listener_for(kind)
    }

    fn blob_transaction_sidecars_listener(&self) -> Receiver<NewBlobSidecar> {
        self.inner.blob_transaction_sidecars_listener()
    }

    fn new_transactions_listener_for(
        &self,
        kind: TransactionListenerKind,
    ) -> Receiver<NewTransactionEvent<Self::Transaction>> {
        self.inner.new_transactions_listener_for(kind)
    }

    fn pooled_transaction_hashes(&self) -> Vec<TxHash> {
        self.inner.pooled_transaction_hashes()
    }

    fn pooled_transaction_hashes_max(&self, max: usize) -> Vec<TxHash> {
        self.inner.pooled_transaction_hashes_max(max)
    }

    fn pooled_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions()
    }

    fn pooled_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pooled_transactions_max(max)
    }

    fn get_pooled_transaction_elements(
        &self,
        tx_hashes: Vec<TxHash>,
        limit: GetPooledTransactionLimit,
    ) -> Vec<<Self::Transaction as PoolTransaction>::Pooled> {
        self.inner.get_pooled_transaction_elements(tx_hashes, limit)
    }

    fn append_pooled_transaction_elements(
        &self,
        tx_hashes: &[TxHash],
        limit: GetPooledTransactionLimit,
        out: &mut Vec<<Self::Transaction as PoolTransaction>::Pooled>,
    ) {
        self.inner.append_pooled_transaction_elements(tx_hashes, limit, out)
    }

    fn get_pooled_transaction_element(
        &self,
        tx_hash: TxHash,
    ) -> Option<Recovered<<Self::Transaction as PoolTransaction>::Pooled>> {
        self.inner.get_pooled_transaction_element(tx_hash)
    }

    fn best_transactions(
        &self,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner.best_transactions()
    }

    fn best_transactions_with_attributes(
        &self,
        best_transactions_attributes: BestTransactionsAttributes,
    ) -> Box<dyn BestTransactions<Item = Arc<ValidPoolTransaction<Self::Transaction>>>> {
        self.inner.best_transactions_with_attributes(best_transactions_attributes)
    }

    fn pending_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions()
    }

    fn get_pending_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn pending_transactions_max(
        &self,
        max: usize,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.pending_transactions_max(max)
    }

    fn queued_transactions(&self) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.queued_transactions()
    }

    fn pending_and_queued_txn_count(&self) -> (usize, usize) {
        self.inner.pending_and_queued_txn_count()
    }

    fn all_transactions(&self) -> AllPoolTransactions<Self::Transaction> {
        self.inner.all_transactions()
    }

    fn all_transactions_by_sender(
        &self,
        sender: Address,
    ) -> AllPoolTransactions<Self::Transaction> {
        self.inner.all_transactions_by_sender(sender)
    }

    fn all_transaction_hashes(&self) -> Vec<TxHash> {
        self.inner.all_transaction_hashes()
    }

    fn get(&self, tx_hash: &TxHash) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get(tx_hash)
    }

    fn get_all(&self, txs: Vec<TxHash>) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_all(txs)
    }

    fn on_propagated(&self, txs: PropagatedTransactions) {
        self.inner.on_propagated(txs)
    }

    fn get_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_sender(sender)
    }

    fn get_pending_transactions_with_predicate(
        &self,
        predicate: impl FnMut(&ValidPoolTransaction<Self::Transaction>) -> bool,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_with_predicate(predicate)
    }

    fn get_pending_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_sender(sender)
    }

    fn get_queued_transactions_by_sender(
        &self,
        sender: Address,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_queued_transactions_by_sender(sender)
    }

    fn get_highest_transaction_by_sender(
        &self,
        sender: Address,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_highest_transaction_by_sender(sender)
    }

    fn get_highest_consecutive_transaction_by_sender(
        &self,
        sender: Address,
        on_chain_nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_highest_consecutive_transaction_by_sender(sender, on_chain_nonce)
    }

    fn get_transaction_by_sender_and_nonce(
        &self,
        sender: Address,
        nonce: u64,
    ) -> Option<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transaction_by_sender_and_nonce(sender, nonce)
    }

    fn get_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_transactions_by_origin(origin)
    }

    fn get_pending_transactions_by_origin(
        &self,
        origin: TransactionOrigin,
    ) -> Vec<Arc<ValidPoolTransaction<Self::Transaction>>> {
        self.inner.get_pending_transactions_by_origin(origin)
    }

    fn unique_senders(&self) -> AddressSet {
        self.inner.unique_senders()
    }

    fn get_blob(
        &self,
        tx_hash: TxHash,
    ) -> Result<Option<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.inner.get_blob(tx_hash)
    }

    fn get_all_blobs(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<(TxHash, Arc<BlobTransactionSidecarVariant>)>, BlobStoreError> {
        self.inner.get_all_blobs(tx_hashes)
    }

    fn get_all_blobs_exact(
        &self,
        tx_hashes: Vec<TxHash>,
    ) -> Result<Vec<Arc<BlobTransactionSidecarVariant>>, BlobStoreError> {
        self.inner.get_all_blobs_exact(tx_hashes)
    }

    fn get_blobs_for_versioned_hashes_v1(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV1>>, BlobStoreError> {
        self.inner.get_blobs_for_versioned_hashes_v1(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v2(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Option<Vec<BlobAndProofV2>>, BlobStoreError> {
        self.inner.get_blobs_for_versioned_hashes_v2(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v3(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<Option<BlobAndProofV2>>, BlobStoreError> {
        self.inner.get_blobs_for_versioned_hashes_v3(versioned_hashes)
    }

    fn get_blobs_for_versioned_hashes_v4(
        &self,
        versioned_hashes: &[B256],
        cell_mask: BlobCellMask,
    ) -> Result<Vec<Option<BlobCellsAndProofsV1>>, BlobStoreError> {
        self.inner.get_blobs_for_versioned_hashes_v4(versioned_hashes, cell_mask)
    }

    fn has_blobs_for_versioned_hashes(
        &self,
        versioned_hashes: &[B256],
    ) -> Result<Vec<bool>, BlobStoreError> {
        self.inner.has_blobs_for_versioned_hashes(versioned_hashes)
    }

    fn blob_store(&self) -> Box<dyn BlobStore> {
        TransactionPool::blob_store(&self.inner)
    }
}

impl<V, S, C> TransactionPoolExt for BscTransactionPool<V, S, C>
where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    S: BlobStore + Clone + 'static,
    C: StateProviderFactory + Clone + Send + Sync + 'static,
{
    type Block = V::Block;
    fn set_block_info(&self, info: BlockInfo) {
        self.inner.set_block_info(info);
        self.wake.notify_one();
    }
    fn on_canonical_state_change(&self, update: CanonicalStateUpdate<'_, Self::Block>) {
        // The same transaction can be mined by another node while still buffered here.
        // Serialize removal with promotion so an in-flight validation cannot reinsert it.
        let mut buffer = self.buffer.lock();
        let mut index = 0;
        while index < buffer.len() {
            if update.mined_transactions.contains(buffer[index].transaction.hash()) {
                buffer.remove(index).finish(TransactionEvent::Mined(update.hash()));
            } else {
                index += 1;
            }
        }
        self.inner.on_canonical_state_change(update);
        self.wake.notify_one();
    }
    fn update_accounts(&self, accounts: Vec<ChangedAccount>) {
        self.inner.update_accounts(accounts);
        self.wake.notify_one();
    }
    fn delete_blob(&self, tx: TxHash) {
        self.inner.delete_blob(tx);
    }
    fn delete_blobs(&self, txs: Vec<TxHash>) {
        self.inner.delete_blobs(txs);
    }
    fn cleanup_blobs(&self) {
        self.inner.cleanup_blobs();
    }
    fn sweep_expired_blobs(&self, max_age: Duration, max_deletes: usize) -> usize {
        self.inner.sweep_expired_blobs(max_age, max_deletes)
    }
}

impl<V, S, C> ValidatingPool for BscTransactionPool<V, S, C>
where
    V: TransactionValidator<Transaction = EthPooledTransaction> + 'static,
    S: BlobStore + Clone + 'static,
    C: StateProviderFactory + Clone + Send + Sync + 'static,
{
    type Validator = V;
    fn validator(&self) -> &Self::Validator {
        self.inner.validator()
    }
}
