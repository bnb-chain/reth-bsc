//! The ERC-20 surface shared by both variants and the dispatcher that fans out to
//! the admin, metadata, permit and memo tables. Ported from core/vm/cas20_token.go.

use super::{
    abi::*,
    ctx::Ctx,
    errors::*,
    policy::PolicyReg,
    sigs::*,
    storage::{addr_key, Store},
};
use alloy_primitives::{Address, B256, U256};

/// pause feature bits in the paused bitmask (slot 11).
pub(crate) const PAUSE_TRANSFER: u8 = 0;
pub(crate) const PAUSE_MINT: u8 = 1;
pub(crate) const PAUSE_BURN: u8 = 2;
pub(crate) const PAUSE_SEIZE: u8 = 3;

pub(crate) struct Token<'f, 'a> {
    pub(crate) ctx: Ctx<'f, 'a>,
    pub(crate) decimals: u8,
    /// Marks the factory's bootstrap frame: role and transfer-side policy gates are
    /// skipped there, MINT_RECEIVER and the renounce freeze are not.
    pub(crate) privileged: bool,
    /// Set for the internal calls of an announce, so a nested announce reverts.
    pub(crate) in_announce: bool,
}

impl<'f, 'a> Token<'f, 'a> {
    pub(crate) fn new(ctx: Ctx<'f, 'a>, decimals: u8) -> Self {
        Self { ctx, decimals, privileged: false, in_announce: false }
    }

    pub(crate) fn bootstrap(ctx: Ctx<'f, 'a>, decimals: u8) -> Self {
        let mut t = Self::new(ctx, decimals);
        t.privileged = true;
        t
    }

    /// The token's own storage.
    pub(crate) fn s(&mut self) -> Store<'_, 'f, 'a> {
        let at = self.ctx.self_addr;
        Store::new(&mut self.ctx, at)
    }

    pub(crate) fn dispatch(&mut self, input: &[u8]) -> R<Vec<u8>> {
        if input.len() < 4 {
            return Err(revert());
        }
        let sel: Selector = input[..4].try_into().unwrap();
        let args = &input[4..];

        match sel {
            SEL_NAME => {
                let v = self.s().name().ok_or(Cas20Err::OutOfGas)?;
                return Ok(enc_string(&v));
            }
            SEL_SYMBOL => {
                let v = self.s().symbol().ok_or(Cas20Err::OutOfGas)?;
                return Ok(enc_string(&v));
            }
            SEL_DECIMALS => return Ok(enc_u256(U256::from(self.decimals))),
            SEL_TOTAL_SUPPLY => {
                let v = self.s().total_supply();
                return Ok(enc_u256(v));
            }
            SEL_BALANCE_OF => {
                let a = read_address(args, 0)?;
                let v = self.s().balance_of(a);
                return Ok(enc_u256(v));
            }
            SEL_ALLOWANCE => {
                let owner = read_address(args, 0)?;
                let spender = read_address(args, 1)?;
                let v = self.s().allowance(owner, spender);
                return Ok(enc_u256(v));
            }
            SEL_APPROVE => {
                let spender = read_address(args, 0)?;
                let amount = read_u256(args, 1)?;
                return self.approve(self.ctx.caller, spender, amount);
            }
            SEL_TRANSFER => {
                let to = read_address(args, 0)?;
                let amount = read_u256(args, 1)?;
                return self.transfer(self.ctx.caller, to, amount);
            }
            SEL_TRANSFER_FROM => {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                return self.transfer_from(self.ctx.caller, from, to, amount);
            }
            _ => {}
        }
        if let Some(r) = self.dispatch_admin(sel, args) {
            return r;
        }
        if let Some(r) = self.dispatch_metadata(sel, args) {
            return r;
        }
        if let Some(r) = self.dispatch_permit_memo(sel, args) {
            return r;
        }
        if let Some(r) = self.dispatch_memo_format(sel, args) {
            return r;
        }
        Err(revert())
    }

    // --- ERC-20 core ------------------------------------------------------------

    pub(crate) fn approve(&mut self, owner: Address, spender: Address, amount: U256) -> R<Vec<u8>> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        // owner is msg.sender, so this only trips in a frame with no caller; the
        // check is declared anyway.
        if owner.is_zero() {
            return Err(rev(ERR_INVALID_APPROVER, &[addr_key(owner)]));
        }
        if spender.is_zero() {
            return Err(rev(ERR_INVALID_SPENDER, &[addr_key(spender)]));
        }
        // Neither the pause features (BEP-702 3.9) nor the policy scopes (3.8) name approve.
        self.s().set_allowance(owner, spender, amount);
        if !self.emit(TOPIC_APPROVAL, owner, spender, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub(crate) fn transfer(&mut self, from: Address, to: Address, amount: U256) -> R<Vec<u8>> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(PAUSE_TRANSFER) {
            return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_TRANSFER)]));
        }
        self.move_balance(from, to, amount)?;
        if !self.emit(TOPIC_TRANSFER, from, to, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub(crate) fn transfer_from(
        &mut self,
        spender: Address,
        from: Address,
        to: Address,
        amount: U256,
    ) -> R<Vec<u8>> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(PAUSE_TRANSFER) {
            return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_TRANSFER)]));
        }
        // Zero-address checks before the allowance: a bad receiver is reported as
        // such whatever the allowance is. move_balance repeats them for the direct path.
        if to.is_zero() {
            return Err(rev(ERR_INVALID_RECEIVER, &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev(ERR_INVALID_SENDER, &[addr_key(from)]));
        }
        let slot = self.s().allowance_slot(from, spender);
        let allowed = self.s().get_u256_at(slot);
        let infinite = allowed == U256::MAX;
        if !infinite && allowed < amount {
            return Err(rev(
                ERR_INSUFFICIENT_ALLOWANCE,
                &[addr_key(spender), w_u256(allowed), w_u256(amount)],
            ));
        }
        // After the allowance: an unauthorized executor with too little allowance is
        // told about the allowance.
        if !self.privileged && spender != from {
            let (_, _, executor) = self.s().transfer_policies();
            if !self.policy_allows(executor, spender) {
                return Err(rev(ERR_POLICY_FORBIDS, &[SCOPE_TRANSFER_EXECUTOR, w_u64(executor)]));
            }
        }
        if !infinite {
            self.s().set_u256_at(slot, allowed - amount);
        }
        self.move_balance(from, to, amount)?;
        if !self.emit(TOPIC_TRANSFER, from, to, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(enc_bool(true))
    }

    pub(crate) fn policy_allows(&mut self, id: u64, account: Address) -> bool {
        if id == 0 {
            return true;
        }
        PolicyReg::new(&mut self.ctx).is_authorized(id, account)
    }

    pub(crate) fn move_balance(&mut self, from: Address, to: Address, amount: U256) -> R<()> {
        if to.is_zero() {
            return Err(rev(ERR_INVALID_RECEIVER, &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev(ERR_INVALID_SENDER, &[addr_key(from)]));
        }
        if !self.privileged {
            let (sender, receiver, _) = self.s().transfer_policies();
            if !self.policy_allows(sender, from) {
                return Err(rev(ERR_POLICY_FORBIDS, &[SCOPE_TRANSFER_SENDER, w_u64(sender)]));
            }
            if !self.policy_allows(receiver, to) {
                return Err(rev(ERR_POLICY_FORBIDS, &[SCOPE_TRANSFER_RECEIVER, w_u64(receiver)]));
            }
        }
        // Both writes happen even when from == to or amount is zero: bytecode would
        // perform them, and skipping them would underprice the native token (BEP-702 3.14).
        let from_slot = self.s().balance_slot(from);
        let bal = self.s().get_u256_at(from_slot);
        if bal < amount {
            return Err(rev(
                ERR_INSUFFICIENT_BALANCE,
                &[addr_key(from), w_u256(bal), w_u256(amount)],
            ));
        }
        self.s().set_u256_at(from_slot, bal - amount);
        let to_slot = self.s().balance_slot(to);
        let to_bal = self.s().get_u256_at(to_slot);
        self.s().set_u256_at(to_slot, to_bal.wrapping_add(amount));
        Ok(())
    }

    pub(crate) fn is_paused(&mut self, bit: u8) -> bool {
        self.s().paused().bit(bit as usize)
    }

    pub(crate) fn emit(&mut self, topic0: B256, a: Address, b: Address, value: U256) -> bool {
        self.ctx.add_log(vec![topic0, addr_key(a), addr_key(b)], value.to_be_bytes::<32>().to_vec())
    }
}
