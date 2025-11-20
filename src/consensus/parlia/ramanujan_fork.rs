use alloy_consensus::Header;
use rand::Rng;
use crate::consensus::parlia::{Snapshot, FIXED_BACKOFF_TIME_BEFORE_FORK_MILLIS, WIGGLE_TIME_BEFORE_FORK_MILLIS, MILLISECONDS_UNIT};
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::constants::DIFF_NOTURN;
use crate::hardforks::BscHardforks;
use reth_chainspec::EthChainSpec;
use crate::node::evm::error::{BscBlockExecutionError, BscBlockValidationError};
use reth_evm::execute::BlockExecutionError;

impl<ChainSpec> Parlia<ChainSpec> 
where ChainSpec: EthChainSpec + BscHardforks + 'static,
{
    /// Calculate block time for Ramanujan fork, return in milliseconds.
    pub fn block_time_for_ramanujan_fork(&self, snap: &Snapshot, parent: &Header, header: &Header) -> u64 {
        let parent_ts = calculate_millisecond_timestamp(parent);
        let mut new_block_ts = parent_ts + snap.block_interval;
        if self.spec.is_ramanujan_active_at_block(header.number) {
            new_block_ts += self.back_off_time(snap, parent, header);
        }
        
        let now = self.present_millis_timestamp();
        if new_block_ts < now {
            // Just to make the millisecond part of the time look more aligned.
            new_block_ts = now.div_ceil(MILLISECONDS_UNIT) * MILLISECONDS_UNIT;
        }
        new_block_ts
    }
    
    /// Calculate delay for Ramanujan fork, return in milliseconds.
    pub fn delay_for_ramanujan_fork(&self, parent_snap: &Snapshot, header: &Header) -> u64 {
        let present_timestamp = self.present_millis_timestamp();
        let header_timestamp = calculate_millisecond_timestamp(header);
        let mut delay_ms = 0;
        if header_timestamp > present_timestamp {
            delay_ms = header_timestamp - present_timestamp;
        }
        tracing::trace!(
            target: "bsc::miner",
            block_number = header.number,
            block_timestamp = header_timestamp,
            present_timestamp = present_timestamp,
            delay_ms = delay_ms,
            "Block timestamp is in the future, waiting before submission"
        );

        if self.spec.is_ramanujan_active_at_block(header.number) {
            return delay_ms;
        }

        // It's not our turn explicitly to sign, delay it a bit
        if header.difficulty == DIFF_NOTURN {
            let wiggle = (parent_snap.validators.len() / 2 + 1) as u64 * WIGGLE_TIME_BEFORE_FORK_MILLIS;
            delay_ms += FIXED_BACKOFF_TIME_BEFORE_FORK_MILLIS + rand::rng().random_range(0..wiggle);
        }
        delay_ms
    }

    /// Verify block time for Ramanujan fork.
    pub fn block_time_verify_for_ramanujan_fork(&self, snap: &Snapshot, header: &Header, parent: &Header) -> Result<(), BlockExecutionError> {
        if self.spec.is_ramanujan_active_at_block(header.number) {
            let current_ts = calculate_millisecond_timestamp(header);
            let parent_ts = calculate_millisecond_timestamp(parent);
            let back_off_time = self.back_off_time(snap, parent, header);
            
            if current_ts < parent_ts + snap.block_interval + back_off_time {
                tracing::warn!(
                    "Block time is too early, block_number: {}, ts: {:?}, parent_ts: {:?}, block_interval: {:?}, back_off_time: {:?}", 
                    header.number, current_ts, parent_ts, snap.block_interval, back_off_time
                );
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::FutureBlock {
                        block_number: header.number,
                        hash: header.hash_slow(),
                    }
                ).into());
            }
        }
        Ok(())
    }
}
