//! A memo is a 32-byte hash, which says nothing about how the payload behind it is
//! encoded. These two entry points let a transfer declare the format its payload
//! follows — one bytes32 naming a format definition maintained off chain. The chain
//! records the declaration and nothing more: it never sees the payload, so it never
//! verifies the claim, and it keeps no registry of formats. A transfer through the
//! plain memo methods has declared nothing. Ported from core/vm/cas20_memo.go.

use super::{
    abi::*, errors::*, permit::read_to_amount_memo, sigs::*, storage::addr_key, token::Token,
};
use alloy_primitives::B256;

impl Token<'_, '_> {
    pub(crate) fn dispatch_memo_format(
        &mut self,
        sel: Selector,
        args: &[u8],
    ) -> Option<R<Vec<u8>>> {
        Some(match sel {
            SEL_TRANSFER_WITH_MEMO_FORMAT => (|| {
                let (to, amount, memo) = read_to_amount_memo(args)?;
                let format = read_word(args, 3)?;
                let caller = self.ctx.caller;
                self.transfer_with_memo_format(format, memo, |t| t.transfer(caller, to, amount))
            })(),
            SEL_TRANSFER_FROM_WITH_MEMO_FORMAT => (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                let format = read_word(args, 4)?;
                let caller = self.ctx.caller;
                // Always transferFrom, so a holder moving its own balance spends its
                // self-approval exactly as through transferFromWithMemo.
                self.transfer_with_memo_format(format, memo, |t| {
                    t.transfer_from(caller, from, to, amount)
                })
            })(),
            _ => return None,
        })
    }

    /// Runs the entry point's own transfer, then the memo and the declaration. A
    /// zero format is refused: the plain memo methods are how a transfer declares
    /// nothing.
    fn transfer_with_memo_format(
        &mut self,
        format: B256,
        memo: B256,
        transfer: impl FnOnce(&mut Self) -> R<Vec<u8>>,
    ) -> R<Vec<u8>> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if format.is_zero() {
            return Err(rev(ERR_INVALID_FORMAT_ID, &[]));
        }
        let ret = transfer(self)?;
        if !self.emit_memo(memo) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = self.ctx.caller;
        // Emitted right after Memo, so an indexer ties the declaration to its
        // transfer by log position rather than by a memo hash two transfers may share.
        if !self
            .ctx
            .add_log(vec![TOPIC_MEMO_FORMAT_DECLARED, addr_key(caller), memo, format], Vec::new())
        {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(ret)
    }
}
