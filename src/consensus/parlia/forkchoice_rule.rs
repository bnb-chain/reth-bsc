use crate::{chainspec::BscChainSpec, hardforks::BscHardforks};
use alloy_consensus::Header;
use alloy_primitives::U256;
use std::{sync::Arc, cmp::Ordering};

/// Header with additional fork choice metadata.
///
/// This struct encapsulates all the data needed for fork choice decisions,
/// including the header itself, total difficulty, and justified block information.
#[derive(Debug, Clone)]
pub struct HeaderForForkchoice<'a> {
    /// The block header
    pub header: &'a Header,
    /// Total difficulty up to this block
    pub td: Option<U256>,
    /// Justified block number (for fast finality)
    pub justified_num: u64,
}

impl<'a> HeaderForForkchoice<'a> {
    /// Creates a new `HeaderForForkchoice` instance.
    pub fn new(header: &'a Header, td: Option<U256>, justified_num: u64) -> Self {
        Self {
            header,
            td,
            justified_num,
        }
    }
}

#[derive(Debug, Clone)]
pub struct BscForkChoiceRule {
    pub spec: Arc<BscChainSpec>,
}

impl BscForkChoiceRule {
    /// Creates a new `BscForkChoiceRule` instance.
    pub fn new(spec: Arc<BscChainSpec>) -> Self {
        Self { spec }
    }

    /// Determines if a reorg is needed based on fork choice rules.
    ///
    /// This is the main entry point for fork choice decisions.
    pub fn is_need_reorg(
        &self,
        incoming: &HeaderForForkchoice,
        current: &HeaderForForkchoice,
    ) -> Result<bool, crate::consensus::ParliaConsensusErr> {
        tracing::debug!(
            target: "bsc::forkchoice",
            incoming_number = incoming.header.number,
            incoming_hash = ?incoming.header.hash_slow(),
            incoming_td = ?incoming.td,
            incoming_justified = incoming.justified_num,
            current_number = current.header.number,
            current_hash = ?current.header.hash_slow(),
            current_td = ?current.td,
            current_justified = current.justified_num,
            "BscForkChoiceRule: Checking whether reorg is needed"
        );

        // Try fast finality first (Plato fork)
        if let Some(need_reorg) = self.head_choice_with_fast_finality(incoming, current) {
            tracing::info!(
                target: "bsc::forkchoice",
                need_reorg,
                incoming_number = incoming.header.number,
                incoming_hash = ?incoming.header.hash_slow(),
                current_number = current.header.number,
                current_hash = ?current.header.hash_slow(),
                method = "fast_finality",
                "Fork choice decision made by fast finality"
            );
            return Ok(need_reorg);
        }

        // Fallback to TD-based comparison
        let result = self.head_choice_with_td(incoming, current)?;
        
        tracing::info!(
            target: "bsc::forkchoice",
            need_reorg = result,
            incoming_number = incoming.header.number,
            incoming_hash = ?incoming.header.hash_slow(),
            incoming_td = ?incoming.td,
            current_number = current.header.number,
            current_hash = ?current.header.hash_slow(),
            current_td = ?current.td,
            method = "total_difficulty",
            "Fork choice decision made by total difficulty comparison"
        );
        
        Ok(result)
    }

    /// Implements BSC fast finality fork choice similar to geth's `ReorgNeededWithFastFinality`.
    ///
    /// Returns `Some(bool)` if fast finality can make a decision, `None` if should fallback to TD.
    /// ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L129
    pub fn head_choice_with_fast_finality(
        &self,
        incoming: &HeaderForForkchoice,
        current: &HeaderForForkchoice,
    ) -> Option<bool> {
        // Check if Plato fork is active for either header
        if !self.spec.as_ref().is_plato_active_at_block(incoming.header.number) &&
           !self.spec.as_ref().is_plato_active_at_block(current.header.number) {
            return None;
        }

        tracing::debug!(
            target: "bsc::forkchoice",
            incoming_justified_num = incoming.justified_num,
            current_justified_num = current.justified_num,
            "Head choice with fast finality"
        );

        // If justified numbers differ, use fast finality rule
        if incoming.justified_num != current.justified_num {
            if incoming.justified_num > current.justified_num && incoming.header.number <= current.header.number {
                tracing::info!(
                    target: "bsc::forkchoice",
                    from_height = current.header.number,
                    from_hash = ?current.header.hash_slow(),
                    to_height = incoming.header.number,
                    to_hash = ?incoming.header.hash_slow(),
                    from_justified = current.justified_num,
                    to_justified = incoming.justified_num,
                    "Chain find higher justifiedNumber"
                );
            }
            return Some(incoming.justified_num > current.justified_num);
        }

        // Justified numbers are equal, need to fallback to TD comparison
        None
    }

    /// Implements BSC fork choice similar to geth's `ReorgNeeded`.
    ///
    /// ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L76
    pub fn head_choice_with_td(
        &self,
        incoming: &HeaderForForkchoice,
        current: &HeaderForForkchoice,
    ) -> Result<bool, crate::consensus::ParliaConsensusErr> {
        let current_td = current.td.ok_or(
            crate::consensus::ParliaConsensusErr::UnknownTotalDifficulty(current.header.hash_slow(), current.header.number)
        )?;
        let incoming_td = incoming.td.ok_or(
            crate::consensus::ParliaConsensusErr::UnknownTotalDifficulty(incoming.header.hash_slow(), incoming.header.number)
        )?;

        tracing::debug!(
            target: "bsc::forkchoice",
            incoming_number = incoming.header.number,
            incoming_hash = ?incoming.header.hash_slow(),
            ?incoming_td,
            current_number = current.header.number,
            current_hash = ?current.header.hash_slow(),
            ?current_td,
            "Head choice with TD"
        );

        // If the total difficulty is higher than our known, add it to the canonical chain
        match incoming_td.cmp(&current_td) {
            Ordering::Greater => Ok(true),
            Ordering::Less => Ok(false),
            Ordering::Equal => {
                // Local and external difficulty is identical.
                // Second clause in the if statement reduces the vulnerability to selfish mining.
                // Please refer to http://www.cs.cornell.edu/~ie53/publications/btcProcFC.pdf
                let reorg = if incoming.header.number < current.header.number {
                    true
                } else if incoming.header.number > current.header.number {
                    false
                } else {
                    // handle incoming_number == current_number case here.
                    if incoming.header.timestamp == current.header.timestamp {
                        if incoming.header.beneficiary == current.header.beneficiary {
                            incoming.header.hash_slow() < current.header.hash_slow()
                        } else {
                            // just rand select a fork.
                            // ref: https://github.com/bnb-chain/bsc/blob/3f345c855ebceb14cca98dc3776718185ba2014a/core/forkchoice.go#L118
                            rand::Rng::random::<f64>(&mut rand::rng()) < 0.5
                        }
                    } else {
                        incoming.header.timestamp < current.header.timestamp
                    }
                };
                Ok(reorg)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chainspec::{bsc::bsc_mainnet, bsc_rialto::bsc_qanet};
    use crate::consensus::parlia::{Snapshot, VoteData, VoteAttestation, EXTRA_VANITY_LEN, EXTRA_SEAL_LEN};
    use alloy_consensus::Header;
    use alloy_primitives::{B256, U256};

    /// Helper: create a header with an embedded VoteAttestation in extra_data
    fn header_with_attestation(number: u64, source_number: u64, target_number: u64) -> Header {
        let att = VoteAttestation {
            vote_address_set: 0,
            agg_signature: Default::default(),
            data: VoteData {
                source_number,
                source_hash: B256::ZERO,
                target_number,
                target_hash: B256::ZERO,
            },
            extra: bytes::Bytes::new(),
        };
        let mut extra = vec![0u8; EXTRA_VANITY_LEN];
        extra.extend_from_slice(alloy_rlp::encode(&att).as_ref());
        extra.extend_from_slice(&[0u8; EXTRA_SEAL_LEN]);
        Header { 
            number, 
            extra_data: alloy_primitives::Bytes::from(extra), 
            ..Default::default() 
        }
    }

    #[test]
    fn test_fork_choice_with_td() {
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_mainnet()));
        let rule = BscForkChoiceRule::new(chain_spec);

        let test_cases = [
            // ((current_number, current_td), (new_number, new_td), should_reorg)
            ((1, 2), (2, 4), true),   // Higher TD wins
            ((1, 2), (2, 1), false),  // Lower TD loses
            ((1, 2), (2, 2), false),  // Same TD, higher number loses (current stays)
            ((2, 2), (1, 2), true),   // Same TD, lower number wins
        ];

        for ((curr_number, curr_td), (new_number, new_td), should_reorg) in test_cases {
            let curr_header = Header { number: curr_number, ..Default::default() };
            let new_header = Header { number: new_number, ..Default::default() };
            
            let current = HeaderForForkchoice::new(&curr_header, Some(U256::from(curr_td)), 0);
            let incoming = HeaderForForkchoice::new(&new_header, Some(U256::from(new_td)), 0);
            
            let result = rule.is_need_reorg(&incoming, &current).unwrap();
            assert_eq!(
                result, should_reorg,
                "Failed for current=({}, {}), incoming=({}, {}): expected {}, got {}",
                curr_number, curr_td, new_number, new_td, should_reorg, result
            );
        }
    }

    #[test]
    fn test_fork_choice_with_fast_finality() {
        let chain_spec = Arc::new(crate::chainspec::BscChainSpec::from(bsc_qanet()));
        let rule = BscForkChoiceRule::new(chain_spec);

        // Setup snapshot provider for fast finality tests
        if crate::shared::get_snapshot_provider().is_none() {
            // Create a simple in-memory snapshot provider for tests
            use std::collections::HashMap as StdHashMap;
            use std::sync::RwLock;
            use crate::consensus::parlia::SnapshotProvider;

            #[derive(Debug)]
            struct TestSnapProvider {
                snaps: RwLock<StdHashMap<B256, Snapshot>>,
            }
            impl TestSnapProvider {
                fn new() -> Self {
                    Self { snaps: RwLock::new(StdHashMap::new()) }
                }
                fn insert_snap(&self, snap: Snapshot) {
                    if let Ok(mut m) = self.snaps.write() {
                        m.insert(snap.block_hash, snap);
                    }
                }
            }
            impl SnapshotProvider for TestSnapProvider {
                fn snapshot_by_hash(&self, block_hash: &B256) -> Option<Snapshot> {
                    self.snaps.read().ok().and_then(|m| m.get(block_hash).cloned())
                }
                fn insert(&self, snapshot: Snapshot) {
                    if let Ok(mut m) = self.snaps.write() {
                        m.insert(snapshot.block_hash, snapshot);
                    }
                }
            }

            let sp = Arc::new(TestSnapProvider::new());
            
            // Insert snapshots with vote data
            let test_cases = [
                // (number, source_num, target_num)
                (10, 8, 9),
                (11, 9, 10),
                (20, 18, 19),
                (21, 18, 19),
                (30, 28, 29),
                (31, 27, 28),
            ];
            
            for (number, source_num, target_num) in test_cases {
                let header = header_with_attestation(number, source_num, target_num);
                let snapshot = Snapshot {
                    block_hash: header.hash_slow(),
                    block_number: number,
                    vote_data: VoteData {
                        source_number: source_num,
                        target_number: target_num,
                        ..Default::default()
                    },
                    epoch_num: 200,
                    ..Default::default()
                };
                sp.insert_snap(snapshot);
            }
            
            let _ = crate::shared::set_snapshot_provider(sp);
        }

        let test_scenarios = [
            // ((current_number, current_td, current_source, current_target), 
            //  (incoming_number, incoming_td, incoming_source, incoming_target), should_reorg)
            ((10, 20, 8, 9), (11, 22, 9, 10), true),   // Higher justified number
            ((20, 40, 18, 19), (21, 40, 18, 19), false), // Equal justified, equal TD
            ((20, 40, 18, 19), (21, 42, 18, 19), true), // Equal justified, higher TD
            ((30, 60, 28, 29), (31, 62, 27, 28), false), // Lower justified, higher TD
        ];

        for ((curr_num, curr_td, curr_src, curr_tgt), (inc_num, inc_td, inc_src, inc_tgt), should_reorg) in test_scenarios {
            let current_header = header_with_attestation(curr_num, curr_src, curr_tgt);
            let incoming_header = header_with_attestation(inc_num, inc_src, inc_tgt);
            
            let current = HeaderForForkchoice::new(&current_header, Some(U256::from(curr_td)), curr_tgt);
            let incoming = HeaderForForkchoice::new(&incoming_header, Some(U256::from(inc_td)), inc_tgt);
            
            let result = rule.is_need_reorg(&incoming, &current).unwrap();
            assert_eq!(
                result, should_reorg,
                "Fast finality test failed for current=({}, justified={}), incoming=({}, justified={}): expected {}, got {}",
                curr_num, curr_tgt, inc_num, inc_tgt, should_reorg, result
            );
        }
    }
}