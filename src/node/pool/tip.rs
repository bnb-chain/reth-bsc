//! The fee floor shared by BSC admission and block construction.

/// Treat an unavailable tip as zero, matching the existing payload selection policy.
pub(crate) fn meets_tip_floor(effective_tip: Option<u128>, floor: u128) -> bool {
    effective_tip.unwrap_or_default() >= floor
}

pub(crate) fn current_tip_floor() -> u128 {
    crate::shared::get_miner_gas_tip().unwrap_or_default().into()
}
