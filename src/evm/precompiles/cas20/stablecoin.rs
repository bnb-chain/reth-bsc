//! BEP-702 3.13: an immutable currency() and decimals fixed at 6. Ported from
//! core/vm/cas20_stablecoin.go.

use super::{abi::enc_string, errors::*, sigs::*, storage::offset_slot, token::Token};
use alloy_primitives::U256;

pub(crate) const NAMESPACE: &str = "bsc.cas20.stablecoin";
pub(crate) const SLOT_CURRENCY: u64 = 0;

pub(crate) fn stablecoin_slot(offset: u64) -> U256 {
    offset_slot(ROOT_STABLECOIN, offset)
}

impl Token<'_, '_> {
    pub(crate) fn currency(&mut self) -> Option<Vec<u8>> {
        self.s().get_string_at(stablecoin_slot(SLOT_CURRENCY))
    }

    pub(crate) fn set_currency(&mut self, v: &[u8]) -> bool {
        self.s().set_string_at(stablecoin_slot(SLOT_CURRENCY), v)
    }
}

pub(crate) fn stablecoin_dispatch(tok: &mut Token<'_, '_>, input: &[u8]) -> R<Vec<u8>> {
    if input.len() >= 4 && input[..4] == SEL_CURRENCY {
        let v = tok.currency().ok_or(Cas20Err::OutOfGas)?;
        return Ok(enc_string(&v));
    }
    tok.dispatch(input)
}
