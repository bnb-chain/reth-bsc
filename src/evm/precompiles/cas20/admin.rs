//! Roles, pausing, minting, burning, seizing and the configurable fields.
//! Ported from core/vm/cas20_admin.go.

use super::{
    abi::*,
    errors::*,
    policy::PolicyReg,
    sigs::*,
    storage::{
        addr_key, OFF_MINT_RECEIVER, OFF_SEIZE_HOLDER, OFF_SEIZE_RECEIVER, OFF_TRANSFER_EXECUTOR,
        OFF_TRANSFER_RECEIVER, OFF_TRANSFER_SENDER, SLOT_MINT_POLICY, SLOT_SEIZE_POLICIES,
        SLOT_TRANSFER_POLICIES,
    },
    token::{Token, PAUSE_BURN, PAUSE_MINT, PAUSE_SEIZE},
    NO_SUPPLY_CAP,
};
use alloy_primitives::{Address, B256, U256};

/// One table for policyId and updatePolicy, so the two cannot disagree on a lane.
pub(crate) fn policy_lane(scope: B256) -> Option<(u64, usize)> {
    Some(match scope {
        SCOPE_TRANSFER_SENDER => (SLOT_TRANSFER_POLICIES, OFF_TRANSFER_SENDER),
        SCOPE_TRANSFER_RECEIVER => (SLOT_TRANSFER_POLICIES, OFF_TRANSFER_RECEIVER),
        SCOPE_TRANSFER_EXECUTOR => (SLOT_TRANSFER_POLICIES, OFF_TRANSFER_EXECUTOR),
        SCOPE_MINT_RECEIVER => (SLOT_MINT_POLICY, OFF_MINT_RECEIVER),
        SCOPE_SEIZE_HOLDER => (SLOT_SEIZE_POLICIES, OFF_SEIZE_HOLDER),
        SCOPE_SEIZE_RECEIVER => (SLOT_SEIZE_POLICIES, OFF_SEIZE_RECEIVER),
        _ => return None,
    })
}

fn read_role_account(args: &[u8]) -> R<(B256, Address)> {
    Ok((read_word(args, 0)?, read_address(args, 1)?))
}

impl Token<'_, '_> {
    pub(crate) fn dispatch_admin(&mut self, sel: Selector, args: &[u8]) -> Option<R<Vec<u8>>> {
        Some(match sel {
            SEL_DEFAULT_ADMIN_ROLE => Ok(enc_word(ROLE_DEFAULT_ADMIN)),
            SEL_MINT_ROLE => Ok(enc_word(ROLE_MINT)),
            SEL_BURN_ROLE => Ok(enc_word(ROLE_BURN)),
            SEL_SEIZE_ROLE => Ok(enc_word(ROLE_SEIZE)),
            SEL_PAUSE_ROLE => Ok(enc_word(ROLE_PAUSE)),
            SEL_UNPAUSE_ROLE => Ok(enc_word(ROLE_UNPAUSE)),
            SEL_METADATA_ROLE => Ok(enc_word(ROLE_METADATA)),

            SEL_HAS_ROLE => (|| {
                let role = read_word(args, 0)?;
                let acct = read_address(args, 1)?;
                Ok(enc_bool(self.s().has_role(role, acct)))
            })(),
            SEL_GET_ROLE_ADMIN => (|| {
                let role = read_word(args, 0)?;
                Ok(enc_word(self.s().role_admin(role)))
            })(),

            SEL_GRANT_ROLE => (|| {
                let (role, acct) = read_role_account(args)?;
                self.grant_role(role, acct).map(|_| Vec::new())
            })(),
            SEL_REVOKE_ROLE => (|| {
                let (role, acct) = read_role_account(args)?;
                self.revoke_role(role, acct).map(|_| Vec::new())
            })(),
            SEL_RENOUNCE_ROLE => (|| {
                let (role, confirm) = read_role_account(args)?;
                self.renounce_role(role, confirm).map(|_| Vec::new())
            })(),
            SEL_SET_ROLE_ADMIN => (|| {
                let role = read_word(args, 0)?;
                let new_admin = read_word(args, 1)?;
                self.set_role_admin(role, new_admin).map(|_| Vec::new())
            })(),
            SEL_RENOUNCE_LAST_ADMIN => self.renounce_last_admin().map(|_| Vec::new()),

            SEL_IS_PAUSED => (|| {
                let w = read_word(args, 0)?;
                if !is_enum_word(w, PAUSE_SEIZE) {
                    return Err(revert());
                }
                Ok(enc_bool(self.is_paused(w.0[31])))
            })(),
            SEL_PAUSE => self.set_pause(args, true).map(|_| Vec::new()),
            SEL_UNPAUSE => self.set_pause(args, false).map(|_| Vec::new()),

            SEL_MINT => (|| {
                let to = read_address(args, 0)?;
                let amount = read_u256(args, 1)?;
                self.mint(to, amount).map(|_| Vec::new())
            })(),
            SEL_BURN => (|| {
                let amount = read_u256(args, 0)?;
                self.burn(self.ctx.caller, amount).map(|_| Vec::new())
            })(),
            SEL_SEIZE_WITH_MEMO => (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                self.seize_with_memo(from, to, amount, memo)?;
                Ok(enc_bool(true))
            })(),

            SEL_UPDATE_SUPPLY_CAP => (|| {
                let cap = read_u256(args, 0)?;
                self.update_supply_cap(cap).map(|_| Vec::new())
            })(),
            SEL_UPDATE_POLICY => (|| {
                let scope = read_word(args, 0)?;
                let id = read_u64(args, 1)?;
                self.update_policy(scope, id).map(|_| Vec::new())
            })(),
            SEL_TRANSFER_SENDER_SCOPE => Ok(enc_word(SCOPE_TRANSFER_SENDER)),
            SEL_TRANSFER_RECEIVER_SCOPE => Ok(enc_word(SCOPE_TRANSFER_RECEIVER)),
            SEL_TRANSFER_EXECUTOR_SCOPE => Ok(enc_word(SCOPE_TRANSFER_EXECUTOR)),
            SEL_MINT_RECEIVER_SCOPE => Ok(enc_word(SCOPE_MINT_RECEIVER)),
            SEL_SEIZE_HOLDER_SCOPE => Ok(enc_word(SCOPE_SEIZE_HOLDER)),
            SEL_SEIZE_RECEIVER_SCOPE => Ok(enc_word(SCOPE_SEIZE_RECEIVER)),
            SEL_POLICY_ID => (|| {
                let scope = read_word(args, 0)?;
                let Some((slot, off)) = policy_lane(scope) else {
                    return Err(rev(ERR_UNSUPPORTED_SCOPE, &[scope]));
                };
                Ok(enc_u256(U256::from(self.s().get_packed_u64(slot, off))))
            })(),
            _ => return None,
        })
    }

    // --- RoleManaged ------------------------------------------------------------

    pub(crate) fn grant_role(&mut self, role: B256, account: Address) -> R<()> {
        self.ensure_role_mutable(role)?;
        if !self.s().has_role(role, account) {
            self.s().set_role(role, account, true);
            if role == ROLE_DEFAULT_ADMIN {
                let n = self.s().admin_count();
                self.s().set_admin_count(n.wrapping_add(U256::from(1)));
            }
            let caller = self.ctx.caller;
            if !self.ctx.add_log(
                vec![TOPIC_ROLE_GRANTED, role, addr_key(account), addr_key(caller)],
                Vec::new(),
            ) {
                return Err(Cas20Err::OutOfGas);
            }
        }
        Ok(())
    }

    pub(crate) fn revoke_role(&mut self, role: B256, account: Address) -> R<()> {
        self.ensure_role_mutable(role)?;
        if role == ROLE_DEFAULT_ADMIN
            && self.s().has_role(role, account)
            && self.s().admin_count() == U256::from(1)
        {
            return Err(rev(ERR_LAST_ADMIN_CANNOT_RENOUNCE, &[]));
        }
        if !self.remove_role(role, account) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn renounce_role(&mut self, role: B256, confirmation: Address) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = self.ctx.caller;
        if confirmation != caller {
            return Err(rev(ERR_AC_BAD_CONFIRMATION, &[]));
        }
        if role == ROLE_DEFAULT_ADMIN
            && self.s().has_role(role, caller)
            && self.s().admin_count() == U256::from(1)
        {
            return Err(rev(ERR_LAST_ADMIN_CANNOT_RENOUNCE, &[]));
        }
        if !self.remove_role(role, caller) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn renounce_last_admin(&mut self) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = self.ctx.caller;
        // A stranger is unauthorized; NotSoleAdmin is for an admin who is not the last.
        if !self.s().has_role(ROLE_DEFAULT_ADMIN, caller) {
            return Err(rev(ERR_AC_UNAUTHORIZED, &[addr_key(caller), ROLE_DEFAULT_ADMIN]));
        }
        if self.s().admin_count() != U256::from(1) {
            return Err(rev(ERR_NOT_SOLE_ADMIN, &[]));
        }
        self.s().set_role(ROLE_DEFAULT_ADMIN, caller, false);
        self.s().set_admin_count(U256::ZERO);
        self.ctx.admin_renounced = true;
        if !self.ctx.add_log(
            vec![TOPIC_ROLE_REVOKED, ROLE_DEFAULT_ADMIN, addr_key(caller), addr_key(caller)],
            Vec::new(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        if !self.ctx.add_log(vec![TOPIC_LAST_ADMIN_RENOUNCED, addr_key(caller)], Vec::new()) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn set_role_admin(&mut self, role: B256, new_admin_role: B256) -> R<()> {
        self.ensure_role_mutable(role)?;
        let prev = self.s().role_admin(role);
        self.s().set_role_admin(role, new_admin_role);
        if !self.ctx.add_log(vec![TOPIC_ROLE_ADMIN_CHANGED, role, prev, new_admin_role], Vec::new())
        {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    fn remove_role(&mut self, role: B256, account: Address) -> bool {
        if !self.s().has_role(role, account) {
            return true;
        }
        self.s().set_role(role, account, false);
        if role == ROLE_DEFAULT_ADMIN {
            let n = self.s().admin_count();
            self.s().set_admin_count(n.wrapping_sub(U256::from(1)));
        }
        let caller = self.ctx.caller;
        self.ctx.add_log(
            vec![TOPIC_ROLE_REVOKED, role, addr_key(account), addr_key(caller)],
            Vec::new(),
        )
    }

    pub(crate) fn ensure_role(&mut self, role: B256) -> R<()> {
        let caller = self.ctx.caller;
        if self.privileged || self.s().has_role(role, caller) {
            return Ok(());
        }
        Err(rev(ERR_AC_UNAUTHORIZED, &[addr_key(caller), role]))
    }

    fn ensure_role_mutable(&mut self, role: B256) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        let caller = self.ctx.caller;
        // The bootstrap window may configure an ownerless token, but not one this
        // frame has just renounced: that transition is permanent.
        if (!self.privileged || self.ctx.admin_renounced) && self.s().admin_count().is_zero() {
            let admin = self.s().role_admin(role);
            return Err(rev(ERR_AC_UNAUTHORIZED, &[addr_key(caller), admin]));
        }
        if !self.privileged {
            let admin = self.s().role_admin(role);
            if !self.s().has_role(admin, caller) {
                // Read again, as the reference does: the second derivation is metered.
                let admin = self.s().role_admin(role);
                return Err(rev(ERR_AC_UNAUTHORIZED, &[addr_key(caller), admin]));
            }
        }
        Ok(())
    }

    // --- Pausable ---------------------------------------------------------------

    fn set_pause(&mut self, args: &[u8], on: bool) -> R<()> {
        let features = read_uint8_array(args)?;
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(if on { ROLE_PAUSE } else { ROLE_UNPAUSE })?;
        if features.is_empty() {
            return Err(rev(ERR_EMPTY_FEATURE_SET, &[]));
        }
        // Read before the caller-sized loop: an unpaid read must be out-of-gas, not
        // an empty mask.
        let mut p = self.s().paused_checked().ok_or(Cas20Err::OutOfGas)?;
        let mut words = Vec::with_capacity(features.len());
        for &f in &features {
            if f > PAUSE_SEIZE {
                return Err(revert());
            }
            words.push(w_u8(f));
            let mask = U256::from(1) << (f as usize);
            if on {
                p |= mask;
            } else {
                p &= !mask;
            }
        }
        self.s().set_paused(p);
        let topic = if on { TOPIC_PAUSED } else { TOPIC_UNPAUSED };
        let caller = self.ctx.caller;
        if !self.ctx.add_log(vec![topic, addr_key(caller)], encode_tuple(&[abi_word_array(&words)]))
        {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    // --- Mintable / Burnable ----------------------------------------------------

    pub(crate) fn mint(&mut self, to: Address, amount: U256) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(PAUSE_MINT) {
            return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_MINT)]));
        }
        self.ensure_role(ROLE_MINT)?;
        self.mint_core(to, amount)
    }

    /// Assumes the caller has checked pause and role.
    pub(crate) fn mint_core(&mut self, to: Address, amount: U256) -> R<()> {
        if to.is_zero() {
            return Err(rev(ERR_INVALID_RECEIVER, &[addr_key(to)]));
        }
        // Enforced even during the privileged bootstrap.
        let mint_receiver = self.s().mint_receiver_policy();
        if !self.policy_allows(mint_receiver, to) {
            return Err(rev(ERR_POLICY_FORBIDS, &[SCOPE_MINT_RECEIVER, w_u64(mint_receiver)]));
        }
        let supply = self.s().total_supply();
        let (new_supply, overflow) = supply.overflowing_add(amount);
        if overflow {
            return Err(rev_panic(0x11));
        }
        let cap = self.s().supply_cap();
        if new_supply > cap {
            return Err(rev(ERR_SUPPLY_CAP_EXCEEDED, &[w_u256(cap), w_u256(new_supply)]));
        }
        let to_slot = self.s().balance_slot(to);
        let bal = self.s().get_u256_at(to_slot);
        self.s().set_u256_at(to_slot, bal.wrapping_add(amount));
        self.s().set_total_supply(new_supply);
        if !self.emit(TOPIC_TRANSFER, Address::ZERO, to, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn burn(&mut self, from: Address, amount: U256) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(PAUSE_BURN) {
            return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_BURN)]));
        }
        self.ensure_role(ROLE_BURN)?;
        let from_slot = self.s().balance_slot(from);
        let bal = self.s().get_u256_at(from_slot);
        if bal < amount {
            return Err(rev(
                ERR_INSUFFICIENT_BALANCE,
                &[addr_key(from), w_u256(bal), w_u256(amount)],
            ));
        }
        self.s().set_u256_at(from_slot, bal - amount);
        let supply = self.s().total_supply();
        if supply < amount {
            return Err(rev_panic(0x11));
        }
        self.s().set_total_supply(supply - amount);
        if !self.emit(TOPIC_TRANSFER, from, Address::ZERO, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    /// SEIZE_HOLDER is inverted: only a disallowed holder is seizable.
    fn seize_with_memo(&mut self, from: Address, to: Address, amount: U256, memo: B256) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if self.is_paused(PAUSE_SEIZE) {
            return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_SEIZE)]));
        }
        self.ensure_role(ROLE_SEIZE)?;
        // A self-seize is a no-op that would still emit Seized; a zero source is a
        // malformed argument, not an empty balance.
        if to.is_zero() || from == to {
            return Err(rev(ERR_INVALID_RECEIVER, &[addr_key(to)]));
        }
        if from.is_zero() {
            return Err(rev(ERR_INVALID_SENDER, &[addr_key(from)]));
        }
        let (seize_holder, seize_receiver) = self.s().seize_policies();
        if self.policy_allows(seize_holder, from) {
            return Err(rev(ERR_ACCOUNT_NOT_SEIZABLE, &[addr_key(from)]));
        }
        if !self.policy_allows(seize_receiver, to) {
            return Err(rev(ERR_POLICY_FORBIDS, &[SCOPE_SEIZE_RECEIVER, w_u64(seize_receiver)]));
        }
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
        if !self.emit(TOPIC_TRANSFER, from, to, amount) {
            return Err(Cas20Err::OutOfGas);
        }
        if !self.emit_memo(memo) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = self.ctx.caller;
        if !self.ctx.add_log(
            vec![TOPIC_SEIZED, addr_key(caller), addr_key(from), addr_key(to)],
            amount.to_be_bytes::<32>().to_vec(),
        ) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    // --- Configurable (subset) --------------------------------------------------

    fn update_supply_cap(&mut self, new_cap: U256) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(ROLE_DEFAULT_ADMIN)?;
        let supply = self.s().total_supply();
        if new_cap < supply || new_cap > NO_SUPPLY_CAP {
            return Err(rev(ERR_INVALID_SUPPLY_CAP, &[w_u256(supply), w_u256(new_cap)]));
        }
        let previous = self.s().supply_cap();
        self.s().set_supply_cap(new_cap);
        let caller = self.ctx.caller;
        let mut data = previous.to_be_bytes::<32>().to_vec();
        data.extend_from_slice(&new_cap.to_be_bytes::<32>());
        if !self.ctx.add_log(vec![TOPIC_SUPPLY_CAP_UPDATED, addr_key(caller)], data) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    /// Rejects a never-created id so the read path's empty-set tolerance cannot be
    /// bound on purpose.
    fn update_policy(&mut self, scope: B256, id: u64) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(ROLE_DEFAULT_ADMIN)?;
        // Scope before id: an unknown scope is reported as such whatever id accompanies it.
        let Some((slot, off)) = policy_lane(scope) else {
            return Err(rev(ERR_UNSUPPORTED_SCOPE, &[scope]));
        };
        if !PolicyReg::new(&mut self.ctx).policy_exists(id) {
            return Err(rev(ERR_POLICY_NOT_FOUND_ID, &[w_u64(id)]));
        }
        let previous = self.s().get_packed_u64(slot, off);
        self.s().set_packed_u64(slot, off, id);
        let mut data = w_u64(previous).to_vec();
        data.extend_from_slice(w_u64(id).as_slice());
        if !self.ctx.add_log(vec![TOPIC_POLICY_UPDATED, scope], data) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }
}
