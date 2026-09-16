//! Business-rule failures are ABI custom errors; malformed calldata and unknown
//! selectors revert with empty returndata (BEP-702 3.2).

use super::{
    abi::{abi_bytes, abi_string, encode_tuple},
    ctx::Ctx,
    sigs::*,
};
use alloy_primitives::{B256, U256};

/// How a CAS20 handler fails. `Revert` carries the returndata, empty for a decode
/// failure or an unknown selector; the other three are exceptional exits the entry
/// point turns into the shape BEP-702 3.2 prescribes.
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

/// What a finished call hands back to the EVM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Exit {
    Return(Vec<u8>),
    Revert(Vec<u8>),
    OutOfGas,
}

/// A refused call form is an ABI error, not an exceptional halt (BEP-702 3.2).
pub(crate) fn finish(res: R<Vec<u8>>) -> Exit {
    match res {
        Ok(ret) => Exit::Return(ret),
        Err(Cas20Err::Revert(data)) => Exit::Revert(data),
        Err(Cas20Err::DelegateCall) => Exit::Revert(rev_data(ERR_DELEGATE_CALL_NOT_ALLOWED)),
        Err(Cas20Err::WriteProtection) => Exit::Revert(rev_data(ERR_STATIC_CALL_NOT_ALLOWED)),
        Err(Cas20Err::OutOfGas) => Exit::OutOfGas,
    }
}

/// Write protection, then an exhausted budget, outrank whatever the logic returned.
pub(crate) fn finish_metered(ctx: &Ctx<'_, '_>, res: R<Vec<u8>>) -> Exit {
    if ctx.write_protection_violated() {
        return finish(Err(Cas20Err::WriteProtection));
    }
    if ctx.out_of_gas() {
        return Exit::OutOfGas;
    }
    finish(res)
}

fn rev_data(sel: Selector) -> Vec<u8> {
    match rev(sel, &[]) {
        Cas20Err::Revert(d) => d,
        _ => unreachable!(),
    }
}
