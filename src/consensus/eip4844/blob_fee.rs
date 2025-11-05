use crate::hardforks::BscHardforks;
use crate::chainspec::BscChainSpec;
use alloy_consensus::Header;
use alloy_eips::eip4844;
use alloy_eips::eip7691;

/// Minimum blob gas price (1 wei)
pub const MIN_BLOB_GAS_PRICE: u128 = 1;

pub const BLOB_TX_BLOB_GAS_PER_BLOB: u64 = 1 << 17;

pub const CANCUN_UPDATE_FRACTION: u64 = eip4844::BLOB_GASPRICE_UPDATE_FRACTION as u64;

pub fn calc_blob_fee(chain_spec: &BscChainSpec, header: &Header) -> u128 {
    let frac = get_update_fraction(chain_spec, header.timestamp);
    
    let excess_blob_gas = header.excess_blob_gas.unwrap_or(0);
    eip4844::fake_exponential(
        MIN_BLOB_GAS_PRICE, 
        u128::from(excess_blob_gas), 
        u128::from(frac)
    )
}

fn get_update_fraction(chain_spec: &BscChainSpec, timestamp: u64) -> u64 {
    use crate::hardforks::bsc::BscHardfork;
    
    if chain_spec.bsc_fork_activation(BscHardfork::Fermi).active_at_timestamp(timestamp) {
        return eip7691::BLOB_GASPRICE_UPDATE_FRACTION_PECTRA as u64;
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Maxwell).active_at_timestamp(timestamp) {
        return eip7691::BLOB_GASPRICE_UPDATE_FRACTION_PECTRA as u64;
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Lorentz).active_at_timestamp(timestamp) {
        return eip7691::BLOB_GASPRICE_UPDATE_FRACTION_PECTRA as u64;
    }
    
    if reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(chain_spec, timestamp) {
        return eip7691::BLOB_GASPRICE_UPDATE_FRACTION_PECTRA as u64;
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Cancun).active_at_timestamp(timestamp) {
        return CANCUN_UPDATE_FRACTION;
    }
    
    panic!("calculating blob fee on unsupported fork")
}
