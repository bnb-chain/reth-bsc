//! Business-rule failures are ABI custom errors; malformed calldata and unknown
//! selectors revert with empty returndata (BEP-702 3.2).

use super::{
    abi::{abi_bytes, abi_string, encode_tuple},
    ctx::Frame,
    observer::CallStatus,
    sigs::*,
};
use alloy_primitives::{B256, U256};

/// Handler failures, translated to BEP-702 return data or an OOG halt at the entry point.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Cas20Err {
    Revert(Vec<u8>),
    OutOfGas,
    WriteProtection,
    DelegateCall,
}

pub(crate) type R<T> = Result<T, Cas20Err>;

/// The empty revert: malformed calldata, an unknown selector, a failed internal call.
pub(crate) fn revert() -> Cas20Err {
    Cas20Err::Revert(Vec::new())
}

pub(crate) fn rev(sel: Selector, words: &[B256]) -> Cas20Err {
    let mut data = Vec::with_capacity(4 + 32 * words.len());
    data.extend_from_slice(&sel);
    for w in words {
        data.extend_from_slice(w.as_slice());
    }
    Cas20Err::Revert(data)
}

pub(crate) fn rev_bytes(sel: Selector, payload: &[u8]) -> Cas20Err {
    let mut data = sel.to_vec();
    data.extend(encode_tuple(&[abi_bytes(payload)]));
    Cas20Err::Revert(data)
}

/// The shape BSC's system contracts use to report a rejected parameter change.
pub(crate) fn rev_string_bytes(sel: Selector, key: &[u8], value: &[u8]) -> Cas20Err {
    let mut data = sel.to_vec();
    data.extend(encode_tuple(&[abi_string(key), abi_bytes(value)]));
    Cas20Err::Revert(data)
}

/// Only 0x11 arises here: a malformed argument is a decode failure and reverts empty.
pub(crate) fn rev_panic(code: u8) -> Cas20Err {
    rev(ERR_PANIC, &[w_u8(code)])
}

pub(crate) fn w_u256(v: U256) -> B256 {
    B256::from(v)
}

pub(crate) fn w_u64(v: u64) -> B256 {
    B256::from(U256::from(v))
}

pub(crate) fn w_u8(v: u8) -> B256 {
    let mut h = B256::ZERO;
    h.0[31] = v;
    h
}

/// Exit precedence: database failure, write protection, OOG, then the handler result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Return(Vec<u8>),
    Revert(Vec<u8>),
    OutOfGas,
    Fatal(String),
}

impl Outcome {
    pub(crate) fn status(&self) -> CallStatus {
        match self {
            Self::Return(_) => CallStatus::Return,
            Self::Revert(_) => CallStatus::Revert,
            Self::OutOfGas => CallStatus::OutOfGas,
            Self::Fatal(_) => CallStatus::Fatal,
        }
    }
}

/// Returns the final outcome, gas used and refund. OOG and fatal errors exhaust the budget.
pub(crate) fn complete(frame: &mut Frame<'_>, result: R<Vec<u8>>) -> (Outcome, u64, i64) {
    if let Some(msg) = frame.fatal.take() {
        frame.gas.exhaust();
        return (Outcome::Fatal(msg), frame.gas.used(), 0);
    }
    let result = if frame.is_write_protected() {
        Err(Cas20Err::WriteProtection)
    } else if frame.is_out_of_gas() {
        Err(Cas20Err::OutOfGas)
    } else {
        result
    };
    let outcome = match result {
        Ok(data) => Outcome::Return(data),
        Err(Cas20Err::Revert(data)) => Outcome::Revert(data),
        // Refused call forms return ABI errors rather than exceptional halts (BEP-702 3.2).
        Err(Cas20Err::DelegateCall) => Outcome::Revert(ERR_DELEGATE_CALL_NOT_ALLOWED.to_vec()),
        Err(Cas20Err::WriteProtection) => Outcome::Revert(ERR_STATIC_CALL_NOT_ALLOWED.to_vec()),
        Err(Cas20Err::OutOfGas) => {
            frame.gas.exhaust();
            Outcome::OutOfGas
        }
    };
    let refund = if matches!(outcome, Outcome::Return(_)) { frame.gas.refund } else { 0 };
    (outcome, frame.gas.used(), refund)
}
