use crate::{
    hardforks::BscHardforks, 
    node::{BscNode, engine_api::payload::BscPayloadTypes}, 
    BscBlock, BscBlockBody, BscPrimitives,
    chainspec::BscChainSpec,
    consensus::{
        ParliaConsensusErr,
        parlia::{provider::EnhancedDbSnapshotProvider, Parlia, util::calculate_millisecond_timestamp, BscForkChoiceRule, HeaderForForkchoice},
    },
    metrics::{BscFinalityMetrics},
    shared
};
use alloy_consensus::{Header, TxReceipt};
use alloy_primitives::{B256, Bytes};
use alloy_eips::Encodable2718;
use alloy_rpc_types::engine::{ForkchoiceState, PayloadStatusEnum};
use reth::{
    api::FullNodeTypes,
    builder::{components::ConsensusBuilder, BuilderContext},
    consensus::{ConsensusError, FullConsensus, Consensus, HeaderValidator},
    beacon_consensus::EthBeaconConsensus,
    consensus_common::validation::{validate_against_parent_hash_number, validate_against_parent_4844},
    primitives::{SealedHeader, SealedBlock, RecoveredBlock},
    providers::BlockExecutionResult,
};
use reth_chainspec::EthChainSpec;
use reth_primitives::{gas_spent_by_transactions, GotExpected};
use reth_ethereum_primitives::Receipt;
use reth_engine_primitives::ConsensusEngineHandle;
use reth_payload_primitives::EngineApiMessageVersion;
use reth_provider::{BlockNumReader, HeaderProvider};
use std::sync::Arc;

/// A basic Bsc consensus builder.
#[derive(Debug, Default, Clone, Copy)]
#[non_exhaustive]
pub struct BscConsensusBuilder;

impl<Node> ConsensusBuilder<Node> for BscConsensusBuilder
where
    Node: FullNodeTypes<Types = BscNode>,
{
    type Consensus = Arc<dyn FullConsensus<BscPrimitives, Error = ConsensusError>>;

    /// return a parlia consensus instance, automatically called by the ComponentsBuilder framework.
    async fn build_consensus(self, ctx: &BuilderContext<Node>) -> eyre::Result<Self::Consensus> {
        let snapshot_provider = create_snapshot_provider(ctx)
            .unwrap_or_else(|e| {
                panic!("Failed to initialize snapshot provider, due to {e}");
            });
        
        crate::shared::set_snapshot_provider(
            snapshot_provider as Arc<dyn crate::consensus::parlia::SnapshotProvider + Send + Sync>,
        ).unwrap_or_else(|_| panic!("Failed to set global snapshot provider"));

        crate::shared::set_header_provider(Arc::new(ctx.provider().clone()))
            .unwrap_or_else(|e| panic!("Failed to set global header provider: {e}"));

        Ok(Arc::new(BscConsensus::new(ctx.chain_spec())))
    }
}

/// BSC consensus implementation.
///
/// Provides basic checks as outlined in the execution specs.
#[derive(Debug, Clone)]
pub struct BscConsensus<ChainSpec> {
    base: EthBeaconConsensus<ChainSpec>,
    parlia: Arc<Parlia<ChainSpec>>,
    chain_spec: Arc<ChainSpec>,
}

impl<ChainSpec: EthChainSpec + BscHardforks + 'static> BscConsensus<ChainSpec> {
    pub fn new(chain_spec: Arc<ChainSpec>) -> Self {
        Self { 
            base: EthBeaconConsensus::new(chain_spec.clone()), 
            parlia: Arc::new(Parlia::new(chain_spec.clone(), 200)), 
            chain_spec,
        }
    }
}

/// header stage validation.
impl<ChainSpec: EthChainSpec + BscHardforks + 'static> HeaderValidator<Header> 
    for BscConsensus<ChainSpec> {
    fn validate_header(&self, header: &SealedHeader) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating header, block_number: {:?}", header.number);
        if let Err(err) = self.parlia.validate_header(header) {
            tracing::warn!("Failed to validate_header, block_number: {}, err: {:?}", header.number, err);
            return Err(err);
        }
        Ok(())
    }

    fn validate_header_against_parent(
        &self,
        header: &SealedHeader,
        parent: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating header against parent, block_number: {:?}", header.number);
        if let Err(err) = validate_against_parent_hash_number(header.header(), parent) {
            tracing::warn!("Failed to validate_against_parent_hash_number, block_number: {}, err: {:?}", header.number, err);
            return Err(err)
        }

        let header_ts = calculate_millisecond_timestamp(header.header());
        let parent_ts = calculate_millisecond_timestamp(parent.header());
        if header_ts <= parent_ts {
            tracing::warn!("Failed to check timestamp, block_number: {}", header.number);
            return Err(ConsensusError::TimestampIsInPast {
                parent_timestamp: parent_ts,
                timestamp: header_ts,
            })
        }

        // ensure that the blob gas fields for this block
        if BscHardforks::is_cancun_active_at_timestamp(&*self.chain_spec, header.header().number, header.header().timestamp) {
            if let Some(blob_params) = self.chain_spec.blob_params_at_timestamp(header.timestamp) {
                if let Err(err) = validate_against_parent_4844(header.header(), parent.header(), blob_params) {
                    tracing::warn!("Failed to validate_against_parent_4844, block_number: {}, err: {:?}", header.number, err);
                    return Err(err)
                }
            }
        }

        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> Consensus<BscBlock>
    for BscConsensus<ChainSpec>
{
    type Error = ConsensusError;

    /// live-sync validation.
    fn validate_body_against_header(
        &self,
        body: &BscBlockBody,
        header: &SealedHeader,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating body against header, block_number: {:?}", header.number);
        Consensus::<BscBlock>::validate_body_against_header(&self.base, body, header)
    }

    /// body stage validation.
    fn validate_block_pre_execution(
        &self,
        block: &SealedBlock<BscBlock>,
    ) -> Result<(), ConsensusError> {
        // tracing::debug!("Validating block pre-execution, block_number: {:?}", block.header().number);
        self.parlia.validate_block_pre_execution(block)?;
        Ok(())
    }
}

impl<ChainSpec: EthChainSpec<Header = Header> + BscHardforks + 'static> FullConsensus<BscPrimitives>
    for BscConsensus<ChainSpec>
{
    /// execution stage validation.
    fn validate_block_post_execution(
        &self,
        block: &RecoveredBlock<BscBlock>,
        result: &BlockExecutionResult<Receipt>,
    ) -> Result<(), ConsensusError> {
        let receipts = &result.receipts;
        let requests = &result.requests;
        let chain_spec = &self.chain_spec;

        // Check if gas used matches the value set in header.
        let cumulative_gas_used =
            receipts.last().map(|receipt| receipt.cumulative_gas_used).unwrap_or(0);
        if block.header().gas_used != cumulative_gas_used {
            return Err(ConsensusError::BlockGasUsed {
                gas: GotExpected { got: cumulative_gas_used, expected: block.header().gas_used },
                gas_spent_by_tx: gas_spent_by_transactions(receipts),
            })
        }

        // Before Byzantium, receipts contained state root that would mean that expensive
        // operation as hashing that is required for state root got calculated in every
        // transaction This was replaced with is_success flag.
        // See more about EIP here: https://eips.ethereum.org/EIPS/eip-658
        if chain_spec.is_byzantium_active_at_block(block.header().number) {
            if let Err(error) = verify_receipts(block.header().receipts_root, block.header().logs_bloom, receipts)
            {
                let receipts = receipts
                    .iter()
                    .map(|r| Bytes::from(r.with_bloom_ref().encoded_2718()))
                    .collect::<Vec<_>>();
                tracing::debug!(%error, ?receipts, "receipts verification failed");
                return Err(error)
            }
        }

        // Validate that the header requests hash matches the calculated requests hash
        if chain_spec.is_prague_active_at_block_and_timestamp(block.header().number, block.header().timestamp) {
            let Some(header_requests_hash) = block.header().requests_hash else {
                return Err(ConsensusError::RequestsHashMissing)
            };
            let requests_hash = requests.requests_hash();
            if requests_hash != header_requests_hash {
                return Err(ConsensusError::BodyRequestsHashDiff(
                    GotExpected::new(requests_hash, header_requests_hash).into(),
                ))
            }
        }
        Ok(())
    }
}

/// Calculate the receipts root, and compare it against the expected receipts root and logs bloom.
/// This is a direct copy of reth's implementation from:
/// https://github.com/paradigmxyz/reth/blob/616e492c79bb4143071ac6bf0831a249a504359f/crates/ethereum/consensus/src/validation.rs#L71
fn verify_receipts<R: reth_primitives_traits::Receipt>(
    expected_receipts_root: B256,
    expected_logs_bloom: alloy_primitives::Bloom,
    receipts: &[R],
) -> Result<(), reth::consensus::ConsensusError> {
    // Calculate receipts root.
    let receipts_with_bloom = receipts.iter().map(TxReceipt::with_bloom_ref).collect::<Vec<_>>();
    let receipts_root = alloy_consensus::proofs::calculate_receipt_root(&receipts_with_bloom);

    // Calculate header logs bloom.
    let logs_bloom = receipts_with_bloom.iter().fold(alloy_primitives::Bloom::ZERO, |bloom, r| bloom | r.bloom_ref());

    compare_receipts_root_and_logs_bloom(
        receipts_root,
        logs_bloom,
        expected_receipts_root,
        expected_logs_bloom,
    )?;

    Ok(())
}

/// Compare the calculated receipts root with the expected receipts root, also compare
/// the calculated logs bloom with the expected logs bloom.
/// This is a direct copy of reth's implementation.
fn compare_receipts_root_and_logs_bloom(
    calculated_receipts_root: B256,
    calculated_logs_bloom: alloy_primitives::Bloom,
    expected_receipts_root: B256,
    expected_logs_bloom: alloy_primitives::Bloom,
) -> Result<(), reth::consensus::ConsensusError> {
    if calculated_receipts_root != expected_receipts_root {
        return Err(reth::consensus::ConsensusError::BodyReceiptRootDiff(
            GotExpected { got: calculated_receipts_root, expected: expected_receipts_root }.into(),
        ))
    }

    if calculated_logs_bloom != expected_logs_bloom {
        return Err(reth::consensus::ConsensusError::BodyBloomLogDiff(
            GotExpected { got: calculated_logs_bloom, expected: expected_logs_bloom }.into(),
        ))
    }

    Ok(())
}

fn create_snapshot_provider<Node>(
    ctx: &BuilderContext<Node>,
) -> eyre::Result<Arc<EnhancedDbSnapshotProvider<Arc<reth_db::DatabaseEnv>>>>
where
    Node: FullNodeTypes<Types = BscNode>,
{

    let datadir = ctx.config().datadir.clone();
    let main_dir = datadir.resolve_datadir(ctx.chain_spec().chain());
    let db_path = main_dir.data_dir().join("parlia_snapshots");
    use reth_db::{init_db, mdbx::DatabaseArguments};
    let snapshot_db = Arc::new(init_db(
        &db_path,
        DatabaseArguments::new(Default::default())
    ).map_err(|e| eyre::eyre!("Failed to initialize snapshot database: {}", e))?);
    tracing::info!("Succeed to create a separate database instance for persistent snapshots");

    let snapshot_provider = Arc::new(EnhancedDbSnapshotProvider::new(
        snapshot_db,
        2048, // Production LRU cache size
        ctx.chain_spec().clone(),
    ));
    tracing::info!("Succeed to create EnhancedDbSnapshotProvider with backward walking capability");

    Ok(snapshot_provider)
}

/// BSC Fork Choice Engine
/// 
/// Manages fork choice decisions for BSC/Parlia consensus, including:
/// - Evaluating incoming blocks against current canonical head
/// - Applying fast finality rules (justified/finalized blocks)
/// - Communicating fork choice updates to the consensus engine
#[derive(Debug, Clone)]
pub struct BscForkChoiceEngine<P> {
    /// The provider for reading block information
    pub(crate) provider: P,
    /// The engine handle for communicating with the consensus engine
    pub(crate) engine_handle: ConsensusEngineHandle<BscPayloadTypes>,
    /// Chain specification
    chain_spec: Arc<BscChainSpec>,
    /// The fork choice rule
    forkchoice_rule: Arc<BscForkChoiceRule>,
    /// Cache for header total difficulties
    header_td_cache: Arc<parking_lot::RwLock<schnellru::LruMap<B256, Option<alloy_primitives::U256>, schnellru::ByLength>>>,
    /// Finality metrics
    finality_metrics: BscFinalityMetrics,
}

impl<P> BscForkChoiceEngine<P>
where
    P: BlockNumReader + HeaderProvider<Header = Header> + Clone,
{
    /// Creates a new `BscForkChoiceEngine` instance.
    pub fn new(
        provider: P,
        engine_handle: ConsensusEngineHandle<BscPayloadTypes>,
        chain_spec: Arc<BscChainSpec>,
    ) -> Self {
        Self {
            provider,
            engine_handle,
            chain_spec: chain_spec.clone(),
            forkchoice_rule: Arc::new(BscForkChoiceRule::new(chain_spec)),
            header_td_cache: Arc::new(parking_lot::RwLock::new(schnellru::LruMap::new(schnellru::ByLength::new(128)))),
            finality_metrics: BscFinalityMetrics::default(),
        }
    }

    /// Returns a reference to the chain specification.
    pub fn chain_spec(&self) -> &Arc<BscChainSpec> {
        &self.chain_spec
    }

    /// Updates the fork choice based on the incoming header.
    /// 
    /// This function evaluates whether the incoming header should become the new canonical head
    /// according to BSC's fork choice rules (Parlia consensus with fast finality).
    ///
    /// # Arguments
    ///
    /// * `incoming_header` - The incoming header to evaluate for fork choice
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the fork choice was successfully updated, or an error if the update failed.
    pub async fn update_forkchoice(&self, incoming_header: &Header) -> Result<(), ParliaConsensusErr> {
        tracing::debug!(
            target: "bsc::forkchoice",
            block_number = incoming_header.number,
            block_hash = ?incoming_header.hash_slow(),
            "Updating fork choice with incoming header"
        );

        let current_number = self.provider.chain_info()?.best_number;
        tracing::trace!(target: "bsc::forkchoice", "Best canonical number: {:?}, new_header = {:?}", current_number, incoming_header);
        
        let current_head = self.provider.header_by_number(current_number)?.ok_or(ParliaConsensusErr::HeadHashNotFound)?;
        
        // Determine if we need to reorg using fork choice rules 
        let need_reorg = self.is_need_reorg(incoming_header, &current_head).await?;
        
        // The new canonical head is the incoming header if reorg is needed, otherwise current
        let new_canonical_head = if need_reorg {
            incoming_header
        } else {
            &current_head
        };
        
        // Get safe block and finalized block with new canonical head
        // ref: https://github.com/bnb-chain/bsc/blob/f70aaa8399ccee429804eecf3fc4c6fd8d9e6cab/eth/api_backend.go#L72
        let (safe_block_number, safe_block_hash) = self.get_justified_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));
        let (finalized_block_number, finalized_block_hash) = self.get_finalized_number_and_hash(new_canonical_head).unwrap_or((0, B256::ZERO));
        
        // Update finality metrics
        self.finality_metrics.justified_block_height.set(safe_block_number as f64);
        self.finality_metrics.safe_block_height.set(safe_block_number as f64);
        self.finality_metrics.finalized_block_height.set(finalized_block_number as f64);
        
        let state = ForkchoiceState {
            head_block_hash: new_canonical_head.hash_slow(),
            safe_block_hash,
            finalized_block_hash,
        };

        tracing::debug!(
            target: "bsc::forkchoice",
            ?state,
            new_canonical_head_number = new_canonical_head.number,
            new_canonical_head_hash = ?new_canonical_head.hash_slow(),
            incoming_header_number = incoming_header.number,
            incoming_header_hash = ?incoming_header.hash_slow(),
            safe_block_number,
            finalized_block_number,
            "Fork choice updated"
        );
        
        match self.engine_handle.fork_choice_updated(state, None, EngineApiMessageVersion::default()).await
        {
            Ok(response) => match response.payload_status.status {
                PayloadStatusEnum::Invalid { validation_error } => 
                    Err(ParliaConsensusErr::ForkChoiceUpdateError(validation_error)),
                _ => Ok(()),
            },
            Err(err) => Err(ParliaConsensusErr::ForkChoiceUpdateError(err.to_string())),
        }
    }

    /// Determines if a chain reorganization is needed based on fork choice rules.
    ///
    /// This function compares the incoming header with the current canonical header
    /// and decides whether the incoming chain should replace the current canonical chain.
    /// The decision is based on BSC's Parlia consensus rules with fast finality support.
    ///
    /// This function handles TD (Total Difficulty) fetching internally.
    ///
    /// # Arguments
    ///
    /// * `incoming_header` - The incoming header from a potentially better chain
    /// * `current_header` - The current canonical head header
    ///
    /// # Returns
    ///
    /// Returns `Ok(true)` if a reorg is needed (incoming should become canonical),
    /// `Ok(false)` if no reorg is needed (current remains canonical),
    /// or an error if the comparison failed.
    pub async fn is_need_reorg(
        &self,
        incoming_header: &Header,
        current_header: &Header,
    ) -> Result<bool, ParliaConsensusErr> {
        let (incoming_td, current_td) = self.header_td_fcu(&self.engine_handle, incoming_header, current_header).await?;
        let incoming_justified_num = self.get_justified_number_and_hash(incoming_header)
            .map(|(num, _)| num)
            .unwrap_or(0);
        let current_justified_num = self.get_justified_number_and_hash(current_header)
            .map(|(num, _)| num)
            .unwrap_or(0);

        let incoming_for_fc = HeaderForForkchoice::new(incoming_header, incoming_td, incoming_justified_num);
        let current_for_fc = HeaderForForkchoice::new(current_header, current_td, current_justified_num);

        self.forkchoice_rule.is_need_reorg(&incoming_for_fc, &current_for_fc)
    }

    /// Gets the justified number and hash from the header's snapshot.
    fn get_justified_number_and_hash(&self, header: &Header) -> Option<(u64, B256)> {
        if !self.chain_spec.is_luban_active_at_block(header.number) {
            return None;
        }
        
        let sp = shared::get_snapshot_provider()?;
        
        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => Some((snap.vote_data.target_number, snap.vote_data.target_hash)),
            None => {
                tracing::warn!(
                    target: "bsc::forkchoice",
                    header_hash = ?header.hash_slow(),
                    "Missing snapshot for header when get justified number and hash"
                );
                None
            }
        }
    }

    /// Gets the finalized number and hash from the header's snapshot.
    fn get_finalized_number_and_hash(&self, header: &Header) -> Option<(u64, B256)> {
        if !self.chain_spec.is_plato_active_at_block(header.number) {
            return None;
        }
        
        let sp = shared::get_snapshot_provider()?;
        
        match sp.snapshot_by_hash(&header.hash_slow()) {
            Some(snap) => Some((snap.vote_data.source_number, snap.vote_data.source_hash)),
            None => {
                tracing::warn!(
                    target: "bsc::forkchoice",
                    header_hash = ?header.hash_slow(),
                    "Missing snapshot for header when get finalized number and hash"
                );
                None
            }
        }
    }

    /// Gets the total difficulty for both incoming and current headers.
    ///
    /// This private method queries the total difficulty (TD) from the engine for both headers,
    /// with fallback logic for the incoming header if not found directly.
    async fn header_td_fcu(
        &self,
        engine: &ConsensusEngineHandle<BscPayloadTypes>,
        incoming: &Header,
        current: &Header,
    ) -> Result<(Option<alloy_primitives::U256>, Option<alloy_primitives::U256>), ParliaConsensusErr> {
        let current_td = self.header_td(engine, current.number, current.hash_slow()).await?;
        let incoming_td = match self.header_td(engine, incoming.number, incoming.hash_slow()).await {
            Ok(td) => td,
            Err(e) => {
                tracing::debug!(target: "bsc::forkchoice", "Failed to get incoming header TD: {:?}, try to query parent block TD", e);
                match self.header_td(engine, incoming.number - 1, incoming.parent_hash).await? {
                    Some(td) => Some(td + incoming.difficulty),
                    None => {
                        tracing::debug!(target: "bsc::forkchoice", "Failed to get parent header TD, return None");
                        None
                    }
                }
            },
        };
        Ok((incoming_td, current_td))
    }

    /// Gets the total difficulty for a specific header.
    ///
    /// This private method queries the TD from the engine and caches it for future use.
    async fn header_td(
        &self,
        engine: &ConsensusEngineHandle<BscPayloadTypes>,
        number: u64,
        hash: B256,
    ) -> Result<Option<alloy_primitives::U256>, ParliaConsensusErr> {
        if let Some(td) = self.header_td_cache.write().get(&hash) {
            return Ok(*td);
        }
        let td = engine.query_td(number, hash).await.map_err(ParliaConsensusErr::internal)?;
        self.header_td_cache.write().insert(hash, td);
        Ok(td)
    }
}