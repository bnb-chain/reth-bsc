//! EIP-4844 implementation for BSC

mod blob_fee;
mod bep657;

pub use blob_fee::{
    calc_blob_fee,
    CANCUN_UPDATE_FRACTION, MIN_BLOB_GAS_PRICE,
    BLOB_TX_BLOB_GAS_PER_BLOB,
};
pub use bep657::{
    is_blob_eligible_block, next_block_excess_blob_gas_with_mendel,
    should_recalculate_excess_blob_gas, BLOB_ELIGIBLE_BLOCK_INTERVAL,
};

// Re-export fake_exponential from alloy_eips
pub use alloy_eips::eip4844::fake_exponential;
