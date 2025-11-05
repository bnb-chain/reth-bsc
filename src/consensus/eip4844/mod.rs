//! EIP-4844 implementation for BSC

mod blob_fee;

pub use blob_fee::{
    calc_blob_fee,
    CANCUN_UPDATE_FRACTION, MIN_BLOB_GAS_PRICE,
    BLOB_TX_BLOB_GAS_PER_BLOB,
};

// Re-export fake_exponential from alloy_eips
pub use alloy_eips::eip4844::fake_exponential;