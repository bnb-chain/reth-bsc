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
            "BscForkChoiceRule: Checking if reorg is needed"
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