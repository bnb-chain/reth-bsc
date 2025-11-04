use crate::hardforks::BscHardforks;
use crate::chainspec::BscChainSpec;
use alloy_consensus::Header;
use alloy_eips::eip4844;
use alloy_eips::eip7691;
use alloy_eips::eip7840::BlobParams;

/// Minimum blob gas price (1 wei)
pub const MIN_BLOB_GAS_PRICE: u128 = 1;

pub const BLOB_TX_BLOB_GAS_PER_BLOB: u64 = 1 << 17;

/// Cancun fork 的 update fraction
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

pub fn calc_excess_blob_gas<ChainSpec>(
    chain_spec: &ChainSpec,
    parent: &Header,
    head_timestamp: u64,
) -> u64 
where
    ChainSpec: BscHardforks,
{
    let parent_excess_blob_gas = parent.excess_blob_gas.unwrap_or(0);
    let parent_blob_gas_used = parent.blob_gas_used.unwrap_or(0);
    
    let blob_params = get_blob_params(chain_spec, head_timestamp);
    let excess_blob_gas = parent_excess_blob_gas+parent_blob_gas_used;

    let target_gas = blob_params.target_blob_gas_per_block() * BLOB_TX_BLOB_GAS_PER_BLOB;
    excess_blob_gas.saturating_sub(target_gas) as u64
}

fn get_blob_params<ChainSpec>(chain_spec: &ChainSpec, timestamp: u64) -> BlobParams 
where
    ChainSpec: BscHardforks,
{
    use crate::hardforks::bsc::BscHardfork;
    
    if chain_spec.bsc_fork_activation(BscHardfork::Fermi).active_at_timestamp(timestamp) {
        return BlobParams::prague();
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Maxwell).active_at_timestamp(timestamp) {
        return BlobParams::prague();
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Lorentz).active_at_timestamp(timestamp) {
        return BlobParams::prague();
    }
    
    if reth_chainspec::EthereumHardforks::is_prague_active_at_timestamp(chain_spec, timestamp) {
        return BlobParams::prague();
    }
    
    if chain_spec.bsc_fork_activation(BscHardfork::Cancun).active_at_timestamp(timestamp) {
        return BlobParams::cancun();
    }
    
    BlobParams::cancun()
}
