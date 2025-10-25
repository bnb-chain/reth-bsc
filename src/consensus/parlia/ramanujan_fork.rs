use std::time::{SystemTime, UNIX_EPOCH};
use alloy_consensus::Header;
use crate::consensus::parlia::Snapshot;
use crate::consensus::parlia::util::calculate_millisecond_timestamp;
use crate::consensus::parlia::consensus::Parlia;
use crate::consensus::parlia::constants::DIFF_NOTURN;
use crate::hardforks::BscHardforks;
use reth_chainspec::EthChainSpec;
use rand::Rng;

const MILLISECONDS_UNIT: u64 = 250;
const WIGGLE_TIME_BEFORE_FORK_MS: u64 = 500; // 500ms
const FIXED_BACK_OFF_TIME_BEFORE_FORK_MS: u64 = 200; // 200ms

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
        
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        if new_block_ts < now {
            // Just to make the millisecond part of the time look more aligned.
            new_block_ts = now.div_ceil(MILLISECONDS_UNIT) * MILLISECONDS_UNIT;
        }
        
        new_block_ts
    }

    /// Calculate delay for Ramanujan fork, return in milliseconds.
    pub fn delay_for_ramanujan_fork(&self, snap: &Snapshot, header: &Header) -> u64 {
        let target_ts = calculate_millisecond_timestamp(header);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        
        let mut delay = target_ts.saturating_sub(now);
        
        if self.spec.is_ramanujan_active_at_block(header.number) {
            return delay;
        }
        
        if header.difficulty == DIFF_NOTURN {
            // It's not our turn explicitly to sign, delay it a bit
            let wiggle = (snap.validators.len() / 2 + 1) as u64 * WIGGLE_TIME_BEFORE_FORK_MS;
            let random_wiggle = rand::rng().random_range(0..wiggle);
            delay += FIXED_BACK_OFF_TIME_BEFORE_FORK_MS + random_wiggle;
        }
        
        delay
    }
}
