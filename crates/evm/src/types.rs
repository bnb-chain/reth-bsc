//! Runtime types shared by BSC execution and RPC inspection.

use crate::hardfork::BscHardfork;
use alloy_primitives::Address;
use evm2::{ethereum::TxEnvelope, BaseEvmConfigSelector, Evm, EvmTypesHost};

/// Native BSC type family. Fork identity is retained even when forks share opcodes.
#[derive(Clone, Copy, Debug)]
pub struct BscEvmTypes;

impl EvmTypesHost for BscEvmTypes {
    type ConfigSelector = BaseEvmConfigSelector;
    type SpecId = BscHardfork;
    type Tx = TxEnvelope;
    type EvmExt = BscRuntime;
    type MessageExt = ();
    type MessageResultExt = ();
    type TxEnvExt = ();
    type TxResultExt = ();
    type BlockEnvExt = ();
    type Host<'a> = Evm<'a, Self>;
}

/// Execution mode that must not be inferred from the backing database type.
#[derive(Clone, Debug)]
pub struct BscRuntime {
    /// Marks an internal call which does not carry the validator's signature.
    pub system_transaction: bool,
    /// Reconstructs the fee-distribution credit when tracing a signed system transaction.
    pub trace_replay: bool,
    /// Canonical system-contract membership, supplied by the chain configuration.
    pub is_system_contract: fn(&Address) -> bool,
}

impl Default for BscRuntime {
    fn default() -> Self {
        Self { system_transaction: false, trace_replay: false, is_system_contract: |_| false }
    }
}
