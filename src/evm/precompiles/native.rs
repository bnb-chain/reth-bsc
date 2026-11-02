//! evm2 dispatch for the fork-specific BSC precompiles.

use super::{cas20, BscPrecompiles};
use alloy_primitives::{Address, Bytes, U256};
use evm2::{
    evm::{
        precompile::{PrecompileOutput, PrecompileProvider},
        PrewarmSet,
    },
    interpreter::{GasTracker, Message},
    precompiles::{MovePrecompileError, PrecompileError, PrecompileHalt, PrecompileId},
};
use reth_bsc_evm::{env::BscEvmEnv, hardfork::BscHardfork, types::BscEvmTypes};
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
};

const MILLI_TIMESTAMP: Address = Address::with_last_byte(0x70);

#[derive(Clone, Debug)]
enum Entry {
    Pure(revm::precompile::Precompile),
    MilliTimestamp,
    Cas20(Address),
}

/// Each EVM owns its routing overrides. Stateful calls are never result-cached.
#[derive(Debug)]
pub struct BscNativePrecompiles {
    entries: BTreeMap<Address, Entry>,
    disabled: BTreeSet<Address>,
    jenner: bool,
    milli_remainder: u64,
}

impl BscNativePrecompiles {
    pub fn new(env: &BscEvmEnv) -> Self {
        let table = BscPrecompiles::new(env.spec).precompiles();
        let mut entries = table
            .addresses()
            .map(|address| (*address, Entry::Pure(table.get(address).unwrap().clone())))
            .collect::<BTreeMap<_, _>>();
        let jenner = env.spec >= BscHardfork::Jenner;
        if jenner {
            entries.insert(MILLI_TIMESTAMP, Entry::MilliTimestamp);
        }
        Self {
            entries,
            disabled: env.code_overrides.clone(),
            jenner,
            milli_remainder: env.milli_remainder,
        }
    }

    fn entry(&self, address: &Address) -> Option<Entry> {
        self.entries.get(address).cloned().or_else(|| {
            (self.jenner &&
                !self.disabled.contains(address) &&
                cas20::is_cas20_precompile(*address))
            .then_some(Entry::Cas20(*address))
        })
    }
}

impl PrecompileProvider<BscEvmTypes> for BscNativePrecompiles {
    fn addresses(&self) -> Vec<Address> {
        self.entries.keys().copied().collect()
    }
    fn warm_addresses(&self, warm: &mut PrewarmSet) {
        for address in self.entries.keys() {
            warm.warm(address);
        }
    }
    fn precompile_ids(&self) -> Vec<(Address, PrecompileId)> {
        self.entries
            .iter()
            .map(|(address, entry)| {
                let name = match entry {
                    Entry::Pure(precompile) => precompile.id().name().to_owned(),
                    Entry::MilliTimestamp => "milliTimestamp".into(),
                    Entry::Cas20(source) => {
                        cas20::native_id(*source).expect("resolved CAS20 address")
                    }
                };
                (*address, PrecompileId::Custom(Cow::Owned(name)))
            })
            .collect()
    }
    fn contains(&self, address: &Address) -> bool {
        self.entry(address).is_some()
    }
    fn move_precompiles(
        &mut self,
        moves: &[(Address, Address)],
    ) -> Result<(), MovePrecompileError> {
        let entries = moves
            .iter()
            .filter(|(from, to)| from != to)
            .map(|(from, to)| {
                self.entry(from)
                    .map(|entry| (*from, *to, entry))
                    .ok_or(MovePrecompileError::NotAPrecompile(*from))
            })
            .collect::<Result<Vec<_>, _>>()?;
        for (from, _, _) in &entries {
            self.entries.remove(from);
            self.disabled.insert(*from);
        }
        for (_, to, entry) in entries {
            self.entries.insert(to, entry);
        }
        Ok(())
    }
    fn execute(
        &mut self,
        evm: &mut evm2::Evm<'_, BscEvmTypes>,
        message: &Message<BscEvmTypes>,
        gas: &mut GasTracker,
    ) -> Option<Result<PrecompileOutput, PrecompileError>> {
        let entry = self.entry(&message.call_target)?;
        Some(match entry {
            Entry::Pure(precompile) => convert_output(
                precompile.execute(&message.input, gas.remaining(), gas.reservoir()),
                gas,
            ),
            Entry::MilliTimestamp => {
                if gas.spend(20).is_err() {
                    return Some(Err(PrecompileHalt::OutOfGas.into()));
                }
                let timestamp = evm
                    .block()
                    .timestamp
                    .saturating_to::<u64>()
                    .wrapping_mul(1000)
                    .wrapping_add(self.milli_remainder);
                Ok(PrecompileOutput::new(Bytes::copy_from_slice(
                    &U256::from(timestamp).to_be_bytes::<32>(),
                )))
            }
            Entry::Cas20(source) => cas20::run_native(source, evm, message, gas),
        })
    }
}

pub(super) fn convert_output(
    result: revm::precompile::PrecompileResult,
    gas: &mut GasTracker,
) -> Result<PrecompileOutput, PrecompileError> {
    use revm::precompile::PrecompileStatus;
    let output = result.map_err(|error| PrecompileError::Fatal(error.to_string().into()))?;
    if let PrecompileStatus::Halt(reason) = output.status {
        return Err(match reason {
            revm::precompile::PrecompileHalt::OutOfGas => PrecompileHalt::OutOfGas.into(),
            reason => PrecompileHalt::Other(reason.to_string().into()).into(),
        });
    }
    gas.spend(output.gas_used).map_err(|_| PrecompileHalt::OutOfGas)?;
    gas.record_refund(output.gas_refunded);
    // BSC's active forks do not have EIP-8037 state gas; these implementations charge
    // their complete cost as execution gas and leave the reservoir unchanged.
    debug_assert_eq!(output.state_gas_used, 0);
    debug_assert_eq!(output.reservoir, gas.reservoir());
    if matches!(output.status, PrecompileStatus::Revert) {
        Err(PrecompileError::Revert(output.bytes))
    } else {
        Ok(PrecompileOutput::new(output.bytes))
    }
}
