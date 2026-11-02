//! BSC transaction rules for evm2.
//!
//! Block finalization, protocol system transactions, and fork-specific precompiles are supplied
//! by the block executor. This registry executes ordinary transactions with BSC fee settlement
//! and authorization rules.

pub mod env;
pub mod evm;
pub mod hardfork;
pub mod system;
pub mod types;

use alloy_consensus::Transaction;
use alloy_primitives::{address, Address, U256};
use evm2::{
    ethereum::{
        self, eip1559, eip2930, eip4844, eip7702, legacy, LazyTxEip7702, PreparedTx, TxEnvelope,
    },
    evm::handler::{GasSettlement, TxHandlerHooks},
    registry::{handler, HandlerResult, TxRegistry, TxRequest},
    Evm, EvmFeatures, EvmTypes, SpecId, TxResult,
};

/// BSC fee collector; Parlia distributes these fees during block finalization.
pub const SYSTEM_ADDRESS: Address = address!("fffffffffffffffffffffffffffffffffffffffe");

/// Nano authorities that cannot receive EIP-7702 delegations.
pub const NANO_BLACKLIST: [Address; 3] = [
    address!("489A8756C18C0b8B24EC2a2b9FF3D4d447F79BEc"),
    address!("Fd6042Df3D74ce9959922FeC559d7995F3933c55"),
    address!("db789Eb5BDb4E559beD199B8b82dED94e1d056C9"),
];

/// Builds the registry for ordinary BSC transactions under the inherited Ethereum specification.
pub fn bsc_user_tx_registry<T: EvmTypes<Tx = TxEnvelope>>(
    spec: SpecId,
) -> TxRegistry<T, TxResult<T>> {
    let mut registry = TxRegistry::new().with_handler(
        0,
        TxEnvelope::as_legacy,
        handler(legacy::prepare_with_hooks::<T, BscHooks>, legacy::execute_prepared::<T, BscHooks>),
    );
    if spec.enables(SpecId::BERLIN) {
        registry.register(
            1,
            TxEnvelope::as_eip2930,
            handler(
                eip2930::prepare_with_hooks::<T, BscHooks>,
                eip2930::execute_prepared::<T, BscHooks>,
            ),
        );
    }
    if spec.enables(SpecId::LONDON) {
        registry.register(
            2,
            TxEnvelope::as_eip1559,
            handler(
                eip1559::prepare_with_hooks::<T, BscHooks>,
                eip1559::execute_prepared::<T, BscHooks>,
            ),
        );
    }
    if spec.enables(SpecId::CANCUN) {
        registry.register(
            3,
            TxEnvelope::as_eip4844,
            handler(
                eip4844::prepare_with_hooks::<T, BscHooks>,
                eip4844::execute_prepared::<T, BscHooks>,
            ),
        );
    }
    if spec.enables(SpecId::PRAGUE) {
        registry.register(
            4,
            TxEnvelope::as_eip7702,
            handler(eip7702::prepare_with_hooks::<T, BscHooks>, execute_authorizations::<T>),
        );
    }
    registry
}

struct BscHooks;

impl<T: EvmTypes<Tx = TxEnvelope>> TxHandlerHooks<T> for BscHooks {
    fn settle_transaction(
        host: &mut Evm<'_, T>,
        envelope: &TxEnvelope,
        gas: GasSettlement<T>,
    ) -> HandlerResult<TxResult<T>> {
        let caller = gas.caller;
        let gas_price = gas.gas_price;
        let gas_limit = gas.gas_limit;
        let result = ethereum::finalize_gas(host, gas)?;
        if host.feature(EvmFeatures::FEE_CHARGE) {
            let gas_used = result.tx_gas_used();
            let refund = U256::from(gas_limit.saturating_sub(gas_used)) * gas_price;
            host.state_mut().account(&caller)?.add_balance(refund);
            // BSC collects the full effective fee, including the blob fee, instead of burning
            // base fees or rewarding the block beneficiary during transaction execution.
            let blob_fee = U256::from(envelope.blob_gas_used().unwrap_or_default()) *
                host.block().blob_basefee;
            let fee = (U256::from(gas_used) * gas_price).saturating_add(blob_fee);
            host.state_mut().account(&SYSTEM_ADDRESS)?.add_balance(fee);
        }
        Ok(result)
    }
}

fn execute_authorizations<T: EvmTypes<Tx = TxEnvelope>>(
    req: TxRequest<'_, '_, T, LazyTxEip7702>,
    prepared: PreparedTx,
) -> HandlerResult<TxResult<T>> {
    if !req
        .tx
        .authorization_list
        .iter()
        .any(|auth| auth.authority().is_some_and(|a| NANO_BLACKLIST.contains(&a)))
    {
        return eip7702::execute_prepared::<T, BscHooks>(req, prepared);
    }

    // Preparation charges intrinsic gas for the original list. Filter only the application
    // phase so rejected authorities neither warm state nor receive a nonce, code, or refund.
    // Preserve the original envelope for inspectors and settlement.
    let mut filtered = (*req.tx.inner()).clone();
    filtered
        .authorization_list
        .retain(|auth| !auth.authority().is_some_and(|a| NANO_BLACKLIST.contains(&a)));
    let tx = alloy_consensus::transaction::Recovered::new_unchecked(&filtered, req.tx.signer());
    eip7702::execute_prepared::<T, BscHooks>(TxRequest { tx, ..req }, prepared)
}

#[cfg(test)]
mod tests;
