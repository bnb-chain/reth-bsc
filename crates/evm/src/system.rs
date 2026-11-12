//! Signed Parlia system transactions, distinct from Ethereum protocol system calls.

use crate::{bsc_user_tx_registry, types::BscEvmTypes, NANO_BLACKLIST};
use alloy_consensus::{Transaction, Typed2718};
use alloy_primitives::U256;
use evm2::{
    env::TxEnvExt,
    ethereum::{self, eip7702, TxEnvelope},
    interpreter::GasTracker,
    registry::{HandlerResult, TxHandler, TxRegistry, TxRequest},
    Evm, SpecId, TxResult,
};

/// Builds handlers for both ordinary and Parlia system transactions.
pub fn bsc_tx_registry(spec: SpecId) -> TxRegistry<BscEvmTypes, TxResult<BscEvmTypes>> {
    let users = bsc_user_tx_registry(spec);
    let mut registry = TxRegistry::new();
    // System transactions skip transaction-type/fork checks. Ordinary transactions still
    // dispatch through the fork-gated registry.
    for id in 0..=4 {
        registry.register(id, |tx| Some(tx), BscHandler { users: users.clone() });
    }
    registry
}

struct BscHandler {
    users: TxRegistry<BscEvmTypes, TxResult<BscEvmTypes>>,
}

impl TxHandler<BscEvmTypes, TxEnvelope, TxResult<BscEvmTypes>> for BscHandler {
    fn validate(&self, req: TxRequest<'_, '_, BscEvmTypes, TxEnvelope>) -> HandlerResult<()> {
        if is_system(&req) {
            prepare_system(req.host, req.tx.signer(), req.envelope).map(|_| ())
        } else {
            self.users.try_get_by_type(req.envelope.ty())?.validate(req.tx, req.host)
        }
    }

    fn execute(
        &self,
        req: TxRequest<'_, '_, BscEvmTypes, TxEnvelope>,
    ) -> HandlerResult<TxResult<BscEvmTypes>> {
        if !is_system(&req) {
            return self.users.try_get_by_type(req.envelope.ty())?.execute(req.tx, req.host);
        }
        let caller = req.tx.signer();
        let nonce = prepare_system(req.host, caller, req.envelope)?;
        let tx = req.envelope;
        if let Some(tx) = tx.as_eip7702() {
            let chain_id = req.host.version().chain_id;
            let mut refunds = eip7702::AuthRefunds::new(req.host.version());
            eip7702::apply_auth_list(
                req.host,
                chain_id,
                tx.authorization_list
                    .iter()
                    .filter(|auth| !auth.authority().is_some_and(|a| NANO_BLACKLIST.contains(&a))),
                &mut refunds,
            )?;
            // Parlia never credits authorization or execution refunds for system transactions.
        }
        let gas_limit = tx.gas_limit();
        let mut gas = GasTracker::new_with_execution_gas_and_reservoir(gas_limit, 0);
        let env = TxEnvExt {
            origin: caller,
            gas_price: U256::ZERO,
            chain_id: U256::from(req.host.version().chain_id),
            blob_hashes: tx
                .blob_versioned_hashes()
                .unwrap_or_default()
                .iter()
                .map(|hash| U256::from_be_bytes(hash.0))
                .collect(),
            ..Default::default()
        };
        let frame = ethereum::prepare_initial_frame(
            req.host,
            caller,
            nonce,
            tx.kind(),
            tx.input(),
            tx.value(),
            &mut gas,
        )?;
        let result =
            ethereum::execute_initial_frame(req.host, &env, frame, &mut gas, gas_limit, 0)?;
        Ok(TxResult::<BscEvmTypes> {
            status: result.stop.is_success(),
            total_gas_spent: gas_limit.saturating_sub(result.gas.remaining()),
            stop: result.stop,
            output: result.output,
            created_address: result.created_address,
            ..Default::default()
        })
    }
}

fn is_system(req: &TxRequest<'_, '_, BscEvmTypes, TxEnvelope>) -> bool {
    req.host.ext().system_transaction ||
        (req.tx.signer() == req.host.block().beneficiary &&
            req.envelope.to().is_some_and(|to| (req.host.ext().is_system_contract)(&to)) &&
            req.envelope.effective_gas_price(Some(0)) == 0)
}

fn prepare_system(
    host: &mut Evm<'_, BscEvmTypes>,
    caller: alloy_primitives::Address,
    tx: &TxEnvelope,
) -> HandlerResult<u64> {
    if host.ext().trace_replay && !tx.value().is_zero() {
        let beneficiary = host.block().beneficiary;
        host.state_mut().account(&beneficiary)?.add_balance(tx.value());
    }
    // Nonce equality is deliberately skipped, but account-code and balance checks remain.
    // Increment the account's actual nonce, not the nonce supplied by the transaction.
    let nonce = host.state_mut().account(&caller)?.nonce();
    ethereum::validate_sender_and_bump_nonce(host, caller, nonce, tx.value())?;
    ethereum::warm_base_accounts(host, tx.kind());
    if let Some(access_list) = tx.access_list() {
        ethereum::warm_access_list(host, access_list);
    }
    Ok(nonce)
}
