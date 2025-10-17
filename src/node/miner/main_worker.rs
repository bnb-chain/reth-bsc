
use alloy_primitives::Address;
use futures::FutureExt;
use reth_payload_primitives::BuiltPayload;
use alloy_consensus::BlockHeader;
use reth_primitives_traits::BlockBody;
use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::node::{
    engine::BscBuiltPayload,
    miner::bsc_miner::MiningContext,
};


/// A payload building job that constructs blocks for BSC
pub struct PayloadJob {
    /// Job identifier
    job_id: u64,
    /// Block number being built
    block_number: u64,
}

impl PayloadJob {
    /// Creates a new PayloadJob instance
    pub fn new(job_id: u64) -> Self {
        Self { 
            job_id,
            block_number: 0, // Will be set when we have mining context
        }
    }

    /// Creates a new PayloadJob with block number
    pub fn new_with_block(job_id: u64, block_number: u64) -> Self {
        Self { 
            job_id,
            block_number,
        }
    }

    /// Returns the job ID
    pub fn job_id(&self) -> u64 {
        self.job_id
    }

    /// Returns the block number
    pub fn block_number(&self) -> u64 {
        self.block_number
    }
}

impl Future for PayloadJob {
    type Output = Result<BscBuiltPayload, Box<dyn std::error::Error + Send + Sync>>;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Empty implementation - immediately return mock BscBuiltPayload
        let mock_payload = BscBuiltPayload::default();
        Poll::Ready(Ok(mock_payload))
    }
}

pub struct MainkWorker<Pool, Provider> {
    validator_address: Address,
    pool: Pool,
    provider: Provider,
    chain_spec: Arc<crate::chainspec::BscChainSpec>,
    parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
    mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
    next_job_id: u64,
    running_payload_job: Option<PayloadJob>,
}

impl<Pool, Provider> MainkWorker<Pool, Provider> {
    /// Creates a new MainkWorker instance
    pub fn new(
        validator_address: Address,
        pool: Pool,
        provider: Provider,
        chain_spec: Arc<crate::chainspec::BscChainSpec>,
        parlia: Arc<crate::consensus::parlia::Parlia<crate::chainspec::BscChainSpec>>,
        mining_queue_rx: mpsc::UnboundedReceiver<MiningContext>,
    ) -> Self {
        Self {
            validator_address,
            pool,
            provider,
            chain_spec,
            parlia,
            mining_queue_rx,
            next_job_id: 1,
            running_payload_job: None,
        }
    }
}

impl<Pool, Provider> Future for MainkWorker<Pool, Provider>
where
    Pool: Send + Sync + Unpin,
    Provider: Send + Sync + Unpin,
{
    type Output = Result<(), Box<dyn std::error::Error + Send + Sync>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        
        // Poll the currently running payload job if it exists
        if let Some(ref mut job) = this.running_payload_job {
            match job.poll_unpin(cx) {
                Poll::Ready(Ok(payload)) => {
                    let job_id = job.job_id();
                    info!("PayloadJob {} completed successfully, built payload: block hash: 0x{:x}, fees: {}", 
                          job_id, payload.block().hash(), payload.fees());
                    
                    // TODO: Here you can process the successful BscBuiltPayload
                    // For example:
                    // - Submit the block to the network
                    // - Store the payload for later use  
                    // - Notify other components
                    // - Update metrics
                    
                    debug!("Block details: number={}, gas_used={}, tx_count={}", 
                           payload.block().number(), 
                           payload.block().gas_used(),
                           payload.block().body().transaction_count());
                    
                    this.running_payload_job = None; // Clear completed job
                    
                    // Wake up to check for new mining contexts after job completion
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Ready(Err(e)) => {
                    let job_id = job.job_id();
                    error!("PayloadJob {} failed with error: {}", job_id, e);
                    this.running_payload_job = None; // Clear failed job
                    
                    // Wake up to check for new mining contexts after job failure
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
                Poll::Pending => {
                    // Job is still running, check for new mining contexts
                }
            }
        }

        // Poll the mining queue for new contexts
        match this.mining_queue_rx.poll_recv(cx) {
            Poll::Ready(Some(ctx)) => {
                let next_block = ctx.parent_header().number() + 1;
                debug!("Received mining context for block {}", next_block);
                
                // Generate a new PayloadJob
                let job_id = this.next_job_id;
                this.next_job_id += 1;
                
                let payload_job = PayloadJob::new_with_block(job_id, next_block);
                info!("Created PayloadJob {} for block {}", job_id, next_block);
                
                // Store the payload job as the currently running job
                this.running_payload_job = Some(payload_job);
                debug!("PayloadJob {} assigned to running_payload_job for mining context", job_id);
                
                // Wake up to start polling the new job immediately
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            Poll::Ready(None) => {
                warn!("Mining queue closed, shutting down MainkWorker");
                return Poll::Ready(Ok(()));
            }
            Poll::Pending => {
                // No new mining contexts available, return Pending
                return Poll::Pending;
            }
        }
    }
}


// todo: think more