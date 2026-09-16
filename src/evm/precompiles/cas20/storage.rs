//! The token storage layout (BEP-702 3.17) and the metered accessors over it.
//! Ported from go-bsc's core/vm/cas20_storage.go; testdata/cas20_layout.json is the
//! authoritative description both clients are held to.

use super::{
    ctx::{
        Ctx, COLD_SLOAD_COST, SSTORE_CLEARS_SCHEDULE_REFUND, SSTORE_RESET_GAS, SSTORE_SET_GAS,
        WARM_STORAGE_READ_COST,
    },
    sigs::ROOT_CORE,
};
use alloy_primitives::{keccak256, Address, B256, U256};

pub(crate) const NAMESPACE: &str = "bsc.cas20";

pub(crate) const SLOT_NAME: u64 = 0;
pub(crate) const SLOT_SYMBOL: u64 = 1;
pub(crate) const SLOT_CONTRACT_URI: u64 = 2;
pub(crate) const SLOT_TOTAL_SUPPLY: u64 = 3;
pub(crate) const SLOT_BALANCES: u64 = 4;
pub(crate) const SLOT_ALLOWANCES: u64 = 5;
pub(crate) const SLOT_ROLES: u64 = 6;
pub(crate) const SLOT_ROLE_ADMINS: u64 = 7;
pub(crate) const SLOT_ADMIN_COUNT: u64 = 8;
pub(crate) const SLOT_TRANSFER_POLICIES: u64 = 9;
pub(crate) const SLOT_MINT_POLICY: u64 = 10;
pub(crate) const SLOT_PAUSED: u64 = 11;
pub(crate) const SLOT_SUPPLY_CAP: u64 = 12;
pub(crate) const SLOT_NONCES: u64 = 13;
pub(crate) const SLOT_SEIZE_POLICIES: u64 = 14;

// The free lanes of each policy slot are reserved for that group, which is why the
// seize ids have a slot of their own rather than filling the mint slot.
pub(crate) const OFF_TRANSFER_SENDER: usize = 0;
pub(crate) const OFF_TRANSFER_RECEIVER: usize = 8;
pub(crate) const OFF_TRANSFER_EXECUTOR: usize = 16;
pub(crate) const OFF_MINT_RECEIVER: usize = 0;
pub(crate) const OFF_SEIZE_HOLDER: usize = 0;
pub(crate) const OFF_SEIZE_RECEIVER: usize = 8;

pub(crate) const MAX_STRING_LEN: u64 = 1 << 24;

pub(crate) fn erc7201_root(namespace: &str) -> B256 {
    let inner = U256::from_be_bytes(keccak256(namespace.as_bytes()).0) - U256::from(1);
    let mut root = keccak256(inner.to_be_bytes::<32>());
    root.0[31] = 0;
    root
}

pub(crate) fn slot_at(offset: u64) -> U256 {
    offset_slot(ROOT_CORE, offset)
}

pub(crate) fn offset_slot(root: B256, offset: u64) -> U256 {
    U256::from_be_bytes(root.0).wrapping_add(U256::from(offset))
}

pub(crate) fn mapping_slot(base: U256, key: B256) -> U256 {
    let mut buf = [0u8; 64];
    buf[..32].copy_from_slice(key.as_slice());
    buf[32..].copy_from_slice(&base.to_be_bytes::<32>());
    U256::from_be_bytes(keccak256(buf).0)
}

pub(crate) fn addr_key(a: Address) -> B256 {
    a.into_word()
}

pub(crate) fn u256_of(w: B256) -> U256 {
    U256::from_be_bytes(w.0)
}

/// A metered view over one account's storage. A token consulting a registry pays
/// as if the slot were its own: no account-access surcharge (BEP-702 3.14).
pub(crate) struct Store<'r, 'f, 'a> {
    pub(crate) ctx: &'r mut Ctx<'f, 'a>,
    pub(crate) at: Address,
}

impl<'r, 'f, 'a> Store<'r, 'f, 'a> {
    pub(crate) fn new(ctx: &'r mut Ctx<'f, 'a>, at: Address) -> Self {
        Self { ctx, at }
    }

    /// A refused charge yields the zero slot, which is harmless: the frame is out of
    /// gas by then, so every access through it is refused too.
    pub(crate) fn map_slot(&mut self, base: U256, key: B256) -> U256 {
        if !self.ctx.charge_keccak(64) {
            return U256::ZERO;
        }
        mapping_slot(base, key)
    }

    /// Solidity hashes a string key's raw bytes ++ base, so the preimage and the
    /// charge are caller-sized.
    pub(crate) fn str_map_slot(&mut self, base: U256, key: &[u8]) -> U256 {
        if !self.ctx.charge_keccak(key.len() + 32) {
            return U256::ZERO;
        }
        let mut buf = Vec::with_capacity(key.len() + 32);
        buf.extend_from_slice(key);
        buf.extend_from_slice(&base.to_be_bytes::<32>());
        U256::from_be_bytes(keccak256(buf).0)
    }

    /// Reads a slot, warming it first as the interpreter does, and charges for the
    /// state it was found in.
    fn read(&mut self, slot: U256) -> Option<U256> {
        if self.ctx.out_of_gas() {
            return None;
        }
        let load = self.ctx.frame.sload(self.at, slot)?;
        let cost = if load.is_cold { COLD_SLOAD_COST } else { WARM_STORAGE_READ_COST };
        if !self.ctx.charge_gas(cost) {
            return None;
        }
        Some(load.data)
    }

    /// For reads whose value decides a branch: get_word's zero is safe only where
    /// the next charge stops the caller anyway, not where zero means "proceed".
    pub(crate) fn get_word_checked(&mut self, slot: U256) -> Option<U256> {
        self.read(slot)
    }

    pub(crate) fn get_word(&mut self, slot: U256) -> U256 {
        self.read(slot).unwrap_or_default()
    }

    /// Mirrors go-bsc's makeGasSStoreFunc, arming in the same order with the same
    /// clause numbers. The write is journalled before the charge is known: a refused
    /// charge ends the frame with every write reverted, so the order is unobservable.
    pub(crate) fn set_word(&mut self, slot: U256, value: U256) -> bool {
        if self.ctx.read_only {
            self.ctx.mark_write_protected();
            return false;
        }
        if !self.ctx.sstore_sentry() {
            self.ctx.mark_out_of_gas();
            return false;
        }
        if self.ctx.out_of_gas() {
            return false;
        }
        let Some(load) = self.ctx.frame.sstore(self.at, slot, value) else { return false };
        let (original, current) = (load.data.original_value, load.data.present_value);
        let cost = if load.is_cold { COLD_SLOAD_COST } else { 0 };
        let refund = &mut self.ctx.frame.gas.refund;

        if current == value {
            // noop (1)
            return self.ctx.charge_gas(cost + WARM_STORAGE_READ_COST);
        }
        if original == current {
            if original.is_zero() {
                // create slot (2.1.1)
                return self.ctx.charge_gas(cost + SSTORE_SET_GAS);
            }
            if value.is_zero() {
                // delete slot (2.1.2b)
                *refund += SSTORE_CLEARS_SCHEDULE_REFUND;
            }
            // write existing slot (2.1.2)
            return self.ctx.charge_gas(cost + (SSTORE_RESET_GAS - COLD_SLOAD_COST));
        }
        // dirty slot (2.2)
        if !original.is_zero() {
            if current.is_zero() {
                // recreate slot (2.2.1.1)
                *refund -= SSTORE_CLEARS_SCHEDULE_REFUND;
            } else if value.is_zero() {
                // delete slot (2.2.1.2)
                *refund += SSTORE_CLEARS_SCHEDULE_REFUND;
            }
        }
        if original == value {
            if original.is_zero() {
                // reset to original inexistent slot (2.2.2.1)
                *refund += (SSTORE_SET_GAS - WARM_STORAGE_READ_COST) as i64;
            } else {
                // reset to original existing slot (2.2.2.2)
                *refund += ((SSTORE_RESET_GAS - COLD_SLOAD_COST) - WARM_STORAGE_READ_COST) as i64;
            }
        }
        // dirty update (2.2)
        self.ctx.charge_gas(cost + WARM_STORAGE_READ_COST)
    }

    // --- fixed uint256 fields ---------------------------------------------------

    pub(crate) fn get_u256(&mut self, offset: u64) -> U256 {
        self.get_word(slot_at(offset))
    }

    pub(crate) fn set_u256(&mut self, offset: u64, v: U256) {
        self.set_word(slot_at(offset), v);
    }

    pub(crate) fn total_supply(&mut self) -> U256 {
        self.get_u256(SLOT_TOTAL_SUPPLY)
    }
    pub(crate) fn set_total_supply(&mut self, v: U256) {
        self.set_u256(SLOT_TOTAL_SUPPLY, v)
    }
    pub(crate) fn supply_cap(&mut self) -> U256 {
        self.get_u256(SLOT_SUPPLY_CAP)
    }
    pub(crate) fn set_supply_cap(&mut self, v: U256) {
        self.set_u256(SLOT_SUPPLY_CAP, v)
    }
    pub(crate) fn admin_count(&mut self) -> U256 {
        self.get_u256(SLOT_ADMIN_COUNT)
    }
    pub(crate) fn set_admin_count(&mut self, v: U256) {
        self.set_u256(SLOT_ADMIN_COUNT, v)
    }

    pub(crate) fn paused_checked(&mut self) -> Option<U256> {
        self.get_word_checked(slot_at(SLOT_PAUSED))
    }
    pub(crate) fn paused(&mut self) -> U256 {
        self.get_u256(SLOT_PAUSED)
    }
    pub(crate) fn set_paused(&mut self, v: U256) {
        self.set_u256(SLOT_PAUSED, v)
    }

    // --- balances / allowances / nonces ----------------------------------------
    //
    // Deriving a mapping slot is a metered keccak, so a read-modify-write derives
    // the slot once and reuses it, as Solidity would.

    pub(crate) fn balance_slot(&mut self, a: Address) -> U256 {
        self.map_slot(slot_at(SLOT_BALANCES), addr_key(a))
    }

    pub(crate) fn allowance_slot(&mut self, owner: Address, spender: Address) -> U256 {
        let inner = self.map_slot(slot_at(SLOT_ALLOWANCES), addr_key(owner));
        self.map_slot(inner, addr_key(spender))
    }

    pub(crate) fn get_u256_at(&mut self, slot: U256) -> U256 {
        self.get_word(slot)
    }

    pub(crate) fn set_u256_at(&mut self, slot: U256, v: U256) {
        self.set_word(slot, v);
    }

    pub(crate) fn balance_of(&mut self, a: Address) -> U256 {
        let slot = self.balance_slot(a);
        self.get_u256_at(slot)
    }

    pub(crate) fn allowance(&mut self, owner: Address, spender: Address) -> U256 {
        let slot = self.allowance_slot(owner, spender);
        self.get_u256_at(slot)
    }

    pub(crate) fn set_allowance(&mut self, owner: Address, spender: Address, v: U256) {
        let slot = self.allowance_slot(owner, spender);
        self.set_u256_at(slot, v)
    }

    pub(crate) fn nonce(&mut self, owner: Address) -> U256 {
        let slot = self.map_slot(slot_at(SLOT_NONCES), addr_key(owner));
        self.get_word(slot)
    }

    pub(crate) fn set_nonce(&mut self, owner: Address, v: U256) {
        let slot = self.map_slot(slot_at(SLOT_NONCES), addr_key(owner));
        self.set_word(slot, v);
    }

    // --- roles ------------------------------------------------------------------

    pub(crate) fn has_role(&mut self, role: B256, a: Address) -> bool {
        let inner = self.map_slot(slot_at(SLOT_ROLES), role);
        let slot = self.map_slot(inner, addr_key(a));
        !self.get_word(slot).is_zero()
    }

    pub(crate) fn set_role(&mut self, role: B256, a: Address, enabled: bool) {
        let inner = self.map_slot(slot_at(SLOT_ROLES), role);
        let slot = self.map_slot(inner, addr_key(a));
        self.set_word(slot, U256::from(enabled as u8));
    }

    pub(crate) fn role_admin(&mut self, role: B256) -> B256 {
        let slot = self.map_slot(slot_at(SLOT_ROLE_ADMINS), role);
        B256::from(self.get_word(slot))
    }

    pub(crate) fn set_role_admin(&mut self, role: B256, admin: B256) {
        let slot = self.map_slot(slot_at(SLOT_ROLE_ADMINS), role);
        self.set_word(slot, u256_of(admin));
    }

    // --- packed policy ids ------------------------------------------------------

    pub(crate) fn get_packed_u64(&mut self, offset: u64, byte_off: usize) -> u64 {
        packed_lane(self.get_word(slot_at(offset)), byte_off)
    }

    pub(crate) fn set_packed_u64(&mut self, offset: u64, byte_off: usize, v: u64) {
        let slot = slot_at(offset);
        let word = self.get_word(slot);
        let lane = U256::from(u64::MAX) << (byte_off * 8);
        let word = (word & !lane) | (U256::from(v) << (byte_off * 8));
        self.set_word(slot, word);
    }

    pub(crate) fn transfer_policies(&mut self) -> (u64, u64, u64) {
        let w = self.get_u256_at(slot_at(SLOT_TRANSFER_POLICIES));
        (
            packed_lane(w, OFF_TRANSFER_SENDER),
            packed_lane(w, OFF_TRANSFER_RECEIVER),
            packed_lane(w, OFF_TRANSFER_EXECUTOR),
        )
    }

    pub(crate) fn seize_policies(&mut self) -> (u64, u64) {
        let w = self.get_u256_at(slot_at(SLOT_SEIZE_POLICIES));
        (packed_lane(w, OFF_SEIZE_HOLDER), packed_lane(w, OFF_SEIZE_RECEIVER))
    }

    pub(crate) fn mint_receiver_policy(&mut self) -> u64 {
        self.get_packed_u64(SLOT_MINT_POLICY, OFF_MINT_RECEIVER)
    }

    // --- strings (Solidity storage encoding) ------------------------------------

    pub(crate) fn get_string(&mut self, offset: u64) -> Option<Vec<u8>> {
        self.get_string_at(slot_at(offset))
    }

    pub(crate) fn set_string(&mut self, offset: u64, s: &[u8]) -> bool {
        self.set_string_at(slot_at(offset), s)
    }

    pub(crate) fn string_data_root(&mut self, slot: U256) -> U256 {
        if !self.ctx.charge_keccak(32) {
            return U256::ZERO;
        }
        U256::from_be_bytes(keccak256(slot.to_be_bytes::<32>()).0)
    }

    pub(crate) fn get_string_at(&mut self, slot: U256) -> Option<Vec<u8>> {
        let word = self.get_word_checked(slot)?;
        let bytes = word.to_be_bytes::<32>();
        if bytes[31] & 1 == 0 {
            let n = (bytes[31] / 2) as usize;
            if n > 31 {
                return Some(Vec::new());
            }
            return Some(bytes[..n].to_vec());
        }
        if word > U256::from(2 * MAX_STRING_LEN + 1) {
            return Some(Vec::new());
        }
        let length = (word.to::<u64>() - 1) / 2;
        if length < 32 {
            return Some(Vec::new());
        }
        let base = self.string_data_root(slot);
        // Grown only behind paid reads, so the allocation is bounded by the gas spent.
        let mut out = Vec::new();
        let mut i = 0u64;
        while i < length {
            let chunk = self.get_word_checked(base.wrapping_add(U256::from(i / 32)))?;
            out.extend_from_slice(&chunk.to_be_bytes::<32>());
            i += 32;
        }
        out.truncate(length as usize);
        Some(out)
    }

    pub(crate) fn set_string_at(&mut self, slot: U256, b: &[u8]) -> bool {
        let old_chunks = self.string_chunks(slot);
        let new_chunks = if b.len() >= 32 { b.len().div_ceil(32) as u64 } else { 0 };

        if b.len() < 32 {
            let mut word = [0u8; 32];
            word[..b.len()].copy_from_slice(b);
            word[31] = (b.len() * 2) as u8;
            if !self.set_word(slot, U256::from_be_bytes(word)) {
                return false;
            }
        } else if !self.set_word(slot, U256::from(b.len() as u64 * 2 + 1)) {
            return false;
        }
        if new_chunks == 0 && old_chunks == 0 {
            return true;
        }
        let base = self.string_data_root(slot);
        for i in 0..new_chunks {
            if self.ctx.out_of_gas() {
                return false;
            }
            let mut chunk = [0u8; 32];
            let start = (i * 32) as usize;
            let end = (start + 32).min(b.len());
            chunk[..end - start].copy_from_slice(&b[start..end]);
            self.set_word(base.wrapping_add(U256::from(i)), U256::from_be_bytes(chunk));
        }
        // old_chunks comes from state, not calldata: without this guard a starved
        // frame would do work proportional to the old length and never pay for it.
        for i in new_chunks..old_chunks {
            if self.ctx.out_of_gas() {
                return false;
            }
            self.set_word(base.wrapping_add(U256::from(i)), U256::ZERO);
        }
        true
    }

    pub(crate) fn string_chunks(&mut self, slot: U256) -> u64 {
        let word = self.get_word(slot);
        if word.to_be_bytes::<32>()[31] & 1 == 0 {
            return 0;
        }
        // Same bounds as get_string_at: a word it reads as empty must not leave
        // chunks for the release loop to walk.
        if word > U256::from(2 * MAX_STRING_LEN + 1) {
            return 0;
        }
        let length = (word.to::<u64>() - 1) / 2;
        if length < 32 {
            return 0;
        }
        length.div_ceil(32)
    }

    pub(crate) fn name(&mut self) -> Option<Vec<u8>> {
        self.get_string(SLOT_NAME)
    }
    pub(crate) fn set_name(&mut self, v: &[u8]) -> bool {
        self.set_string(SLOT_NAME, v)
    }
    pub(crate) fn symbol(&mut self) -> Option<Vec<u8>> {
        self.get_string(SLOT_SYMBOL)
    }
    pub(crate) fn set_symbol(&mut self, v: &[u8]) -> bool {
        self.set_string(SLOT_SYMBOL, v)
    }
    pub(crate) fn contract_uri(&mut self) -> Option<Vec<u8>> {
        self.get_string(SLOT_CONTRACT_URI)
    }
    pub(crate) fn set_contract_uri(&mut self, v: &[u8]) -> bool {
        self.set_string(SLOT_CONTRACT_URI, v)
    }
}

pub(crate) fn packed_lane(word: U256, byte_off: usize) -> u64 {
    (word >> (byte_off * 8)).to::<U256>().wrapping_to::<u64>()
}
