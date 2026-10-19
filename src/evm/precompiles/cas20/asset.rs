//! The Asset variant: decimals, the ERC-8056 UI multiplier with its schedule,
//! announcements, extra metadata and batch minting. Its storage lives in its own
//! ERC-7201 namespace, disjoint from the core layout. Ported from core/vm/cas20_asset.go.

use super::{
    abi::*,
    errors::*,
    sigs::*,
    storage::{addr_key, offset_slot},
    token::{Token, PAUSE_MINT},
};
use alloy_primitives::{Address, B256, U256};

#[cfg(test)]
pub(crate) const NAMESPACE: &str = "bsc.cas20.asset";

pub(crate) const SLOT_DECIMALS: u64 = 0;
pub(crate) const SLOT_MULTIPLIER: u64 = 1;
/// mapping(string id => bool used)
pub(crate) const SLOT_ANNOUNCEMENTS: u64 = 2;
/// mapping(string key => string value)
pub(crate) const SLOT_EXTRA_META: u64 = 3;
/// packed: multiplier (u128) | effectiveAt (u64)
pub(crate) const SLOT_PENDING: u64 = 4;

/// LSB-first, as Solidity packs {uint128 multiplier; uint64 effectiveAt}.
const PENDING_WHEN_BITS: usize = 128;

/// 1.0x
pub(crate) const WAD: U256 = U256::from_limbs([1_000_000_000_000_000_000, 0, 0, 0]);
const U64_MAX: U256 = U256::from_limbs([u64::MAX, 0, 0, 0]);
pub(crate) const U128_MASK: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

/// All four ERC-8056 interfaces are implemented, so all four are advertised.
const INTERFACE_IDS: [[u8; 4]; 5] = [
    [0x01, 0xff, 0xc9, 0xa7], // IERC165
    [0xa6, 0x0b, 0xf1, 0x3d], // IScaledUIAmount
    [0x4b, 0xd2, 0x76, 0x48], // IScaledUIAmountNewUIMultiplier
    [0xd8, 0x90, 0xfd, 0x71], // IScaledUIAmountBalances
    [0x57, 0x85, 0x4f, 0xc3], // IScaledUIAmountConversion
];

pub(crate) fn asset_slot(offset: u64) -> U256 {
    offset_slot(ROOT_ASSET, offset)
}

impl Token<'_, '_> {
    pub(crate) fn asset_decimals(&mut self) -> u8 {
        self.s().get_word(asset_slot(SLOT_DECIMALS)).wrapping_to::<u64>() as u8
    }
    fn set_asset_decimals(&mut self, d: u8) {
        self.s().set_word(asset_slot(SLOT_DECIMALS), U256::from(d));
    }
    pub(crate) fn multiplier(&mut self) -> U256 {
        self.s().get_word(asset_slot(SLOT_MULTIPLIER))
    }
    fn set_multiplier(&mut self, m: U256) {
        self.s().set_word(asset_slot(SLOT_MULTIPLIER), m);
    }

    // --- ERC-8056 scheduled multiplier -------------------------------------------
    //
    // The effective multiplier flips from the stored value to the scheduled one at a
    // timestamp, with no transaction and no event at the flip.

    pub(crate) fn pending(&mut self) -> (U256, u64) {
        let w = self.s().get_word(asset_slot(SLOT_PENDING));
        (w & U128_MASK, (w >> PENDING_WHEN_BITS).wrapping_to::<u64>())
    }

    fn set_pending(&mut self, mul: U256, effective_at: u64) {
        let packed = (mul & U128_MASK) | (U256::from(effective_at) << PENDING_WHEN_BITS);
        self.s().set_word(asset_slot(SLOT_PENDING), packed);
    }

    fn clear_pending(&mut self) {
        self.s().set_word(asset_slot(SLOT_PENDING), U256::ZERO);
    }

    /// Reads cannot write, so a matured schedule is folded into the stored
    /// multiplier by the next write that reuses the slot; otherwise that write would
    /// revalue the token.
    fn settle_matured(&mut self, now: u64) {
        let (mul, at) = self.pending();
        if at != 0 && now >= at {
            self.set_multiplier(mul);
        }
    }

    pub(crate) fn effective_multiplier(&mut self, now: u64) -> U256 {
        let (mul, at) = self.pending();
        if at != 0 && now >= at {
            return mul;
        }
        self.multiplier()
    }

    fn announcement_slot(&mut self, id: &[u8]) -> U256 {
        self.s().str_map_slot(asset_slot(SLOT_ANNOUNCEMENTS), id)
    }

    fn announcement_used(&mut self, id: &[u8]) -> Option<bool> {
        let slot = self.announcement_slot(id);
        self.s().get_word_checked(slot).map(|w| !w.is_zero())
    }

    fn mark_announcement(&mut self, id: &[u8]) -> bool {
        let slot = self.announcement_slot(id);
        self.s().set_word(slot, U256::from(1))
    }

    fn extra_meta_slot(&mut self, key: &[u8]) -> U256 {
        self.s().str_map_slot(asset_slot(SLOT_EXTRA_META), key)
    }

    fn extra_metadata(&mut self, key: &[u8]) -> Option<Vec<u8>> {
        let slot = self.extra_meta_slot(key);
        self.s().get_string_at(slot)
    }

    fn set_extra_metadata(&mut self, key: &[u8], value: &[u8]) -> bool {
        let slot = self.extra_meta_slot(key);
        self.s().set_string_at(slot, value)
    }
}

pub(crate) fn init_asset_extension(tok: &mut Token<'_, '_>, decimals: u8) {
    tok.set_asset_decimals(decimals);
    tok.set_multiplier(WAD);
}

pub(crate) fn apply_multiplier(raw: U256, mul: U256) -> R<U256> {
    let (p, overflow) = raw.overflowing_mul(mul);
    if overflow {
        return Err(rev_panic(0x11));
    }
    Ok(p / WAD)
}

/// uint256 division by zero yields zero; the setters reject a zero multiplier anyway.
pub(crate) fn remove_multiplier(scaled: U256, mul: U256) -> R<U256> {
    let (p, overflow) = scaled.overflowing_mul(WAD);
    if overflow {
        return Err(rev_panic(0x11));
    }
    Ok(p.checked_div(mul).unwrap_or_default())
}

pub(crate) fn asset_dispatch(tok: &mut Token<'_, '_>, input: &[u8]) -> R<Vec<u8>> {
    if let Some(r) = dispatch_asset(tok, input) {
        return r;
    }
    tok.dispatch(input)
}

fn dispatch_asset(tok: &mut Token<'_, '_>, input: &[u8]) -> Option<R<Vec<u8>>> {
    if input.len() < 4 {
        return None;
    }
    let sel: Selector = input[..4].try_into().unwrap();
    let args = &input[4..];
    let now = tok.ctx.block_time();

    Some(match sel {
        SEL_DECIMALS => Ok(enc_u256(U256::from(tok.asset_decimals()))),
        SEL_MULTIPLIER | SEL_UI_MULTIPLIER => Ok(enc_u256(tok.effective_multiplier(now))),
        SEL_WAD_PRECISION => Ok(enc_u256(WAD)),
        SEL_MAX_UI_MULTIPLIER => Ok(enc_u256(U128_MASK)),
        SEL_OPERATOR_ROLE => Ok(enc_word(ROLE_OPERATOR)),
        SEL_NEW_UI_MULTIPLIER => {
            // With no live schedule it answers as uiMultiplier does (ERC-8056).
            let (mut mul, at) = tok.pending();
            if at <= now {
                mul = tok.effective_multiplier(now);
            }
            Ok(enc_u256(mul))
        }
        SEL_EFFECTIVE_AT => {
            let (_, at) = tok.pending();
            Ok(enc_u256(U256::from(at)))
        }
        SEL_TOTAL_SUPPLY_UI => (|| {
            let supply = tok.s().total_supply();
            let mul = tok.effective_multiplier(now);
            Ok(enc_u256(apply_multiplier(supply, mul)?))
        })(),
        SEL_SUPPORTS_INTERFACE => (|| {
            let id = read_word(args, 0)?;
            if id.0[4..].iter().any(|&b| b != 0) {
                return Err(revert());
            }
            let want: [u8; 4] = id.0[..4].try_into().unwrap();
            Ok(enc_bool(INTERFACE_IDS.contains(&want)))
        })(),
        SEL_CANCEL_UI_MULTIPLIER => cancel_ui_multiplier(tok).map(|_| Vec::new()),
        SEL_UPDATE_UI_MULTIPLIER => (|| {
            let mul = read_u256(args, 0)?;
            let at = read_u256(args, 1)?;
            update_ui_multiplier(tok, mul, at).map(|_| Vec::new())
        })(),
        SEL_SCALED_BALANCE_OF | SEL_BALANCE_OF_UI => (|| {
            let a = read_address(args, 0)?;
            let bal = tok.s().balance_of(a);
            let mul = tok.effective_multiplier(now);
            Ok(enc_u256(apply_multiplier(bal, mul)?))
        })(),
        SEL_TO_SCALED_BALANCE | SEL_TO_UI_AMOUNT => (|| {
            let raw = read_u256(args, 0)?;
            let mul = tok.effective_multiplier(now);
            Ok(enc_u256(apply_multiplier(raw, mul)?))
        })(),
        SEL_TO_RAW_BALANCE | SEL_FROM_UI_AMOUNT => (|| {
            let scaled = read_u256(args, 0)?;
            let mul = tok.effective_multiplier(now);
            Ok(enc_u256(remove_multiplier(scaled, mul)?))
        })(),
        SEL_UPDATE_MULTIPLIER => (|| {
            let m = read_u256(args, 0)?;
            update_multiplier(tok, m).map(|_| Vec::new())
        })(),
        SEL_BATCH_MINT => batch_mint(tok, args).map(|_| Vec::new()),
        SEL_IS_ANNOUNCEMENT_ID_USED => (|| {
            let id = read_string_arg(args, 0)?;
            let used = tok.announcement_used(id).ok_or(Cas20Err::OutOfGas)?;
            Ok(enc_bool(used))
        })(),
        SEL_ANNOUNCE => announce(tok, args).map(|_| Vec::new()),
        SEL_EXTRA_METADATA => (|| {
            let key = read_string_arg(args, 0)?;
            let v = tok.extra_metadata(key).ok_or(Cas20Err::OutOfGas)?;
            Ok(enc_string(&v))
        })(),
        SEL_UPDATE_EXTRA_METADATA => (|| {
            let key = read_string_arg(args, 0)?;
            let value = read_string_arg(args, 1)?;
            update_extra_metadata(tok, key, value).map(|_| Vec::new())
        })(),
        _ => return None,
    })
}

fn update_extra_metadata(tok: &mut Token<'_, '_>, key: &[u8], value: &[u8]) -> R<()> {
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(ROLE_METADATA)?;
    if key.is_empty() {
        return Err(rev(ERR_INVALID_METADATA_KEY, &[]));
    }
    if !tok.set_extra_metadata(key, value) {
        return Err(Cas20Err::OutOfGas);
    }
    if !tok.ctx.add_log(
        vec![TOPIC_EXTRA_METADATA_UPDATED],
        encode_tuple(&[abi_string(key), abi_string(value)]),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn announce(tok: &mut Token<'_, '_>, args: &[u8]) -> R<()> {
    let calls = read_bytes_array(args, 0)?;
    let id = read_string_arg(args, 1)?;
    let description = read_string_arg(args, 2)?;
    let uri = read_string_arg(args, 3)?;
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    if tok.in_announce {
        return Err(rev(ERR_ANNOUNCEMENT_IN_PROGRESS, &[]));
    }
    tok.ensure_role(ROLE_OPERATOR)?;
    let used = tok.announcement_used(id).ok_or(Cas20Err::OutOfGas)?;
    if used {
        return Err(rev_bytes(ERR_ANNOUNCEMENT_ID_ALREADY_USED, id));
    }
    if !tok.mark_announcement(id) {
        return Err(Cas20Err::OutOfGas);
    }
    let caller = tok.ctx.caller;
    if !tok.ctx.add_log(
        vec![TOPIC_ANNOUNCEMENT, addr_key(caller)],
        encode_tuple(&[abi_string(id), abi_string(description), abi_string(uri)]),
    ) {
        return Err(Cas20Err::OutOfGas);
    }

    // The flag travels with the internal calls only: the reference passes the token
    // by value, so the caller's copy never sees it set.
    tok.in_announce = true;
    for c in calls {
        if !tok.ctx.charge_internal_dispatch(c) {
            return Err(Cas20Err::OutOfGas);
        }
        if c.len() < 4 {
            return Err(rev_bytes(ERR_INTERNAL_CALL_MALFORMED, c));
        }
        if asset_dispatch(tok, c).is_err() {
            return Err(rev_bytes(ERR_INTERNAL_CALL_FAILED, c));
        }
    }
    tok.in_announce = false;
    if !tok.ctx.add_log(vec![TOPIC_END_ANNOUNCEMENT], encode_tuple(&[abi_string(id)])) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

/// Check order: role, value, both timestamp bounds, then a live schedule.
fn update_ui_multiplier(tok: &mut Token<'_, '_>, new_mul: U256, at: U256) -> R<()> {
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(ROLE_OPERATOR)?;
    if new_mul.is_zero() || new_mul > U128_MASK {
        return Err(rev(ERR_INVALID_MULTIPLIER, &[]));
    }
    let now = tok.ctx.block_time();
    if at <= U256::from(now) {
        return Err(rev(ERR_EFFECTIVE_AT_IN_PAST, &[w_u256(at)]));
    }
    if at > U64_MAX {
        return Err(rev(ERR_EFFECTIVE_AT_TOO_FAR, &[w_u256(at)]));
    }
    // Only a live schedule blocks a new one; a matured record is stale state, not a commitment.
    let (_, existing) = tok.pending();
    if existing > now {
        return Err(rev(ERR_UI_MUL_EXISTS, &[w_u64(existing)]));
    }
    tok.settle_matured(now);
    let previous = tok.multiplier();
    tok.set_pending(new_mul, at.to::<u64>());
    let mut data = previous.to_be_bytes::<32>().to_vec();
    data.extend_from_slice(&new_mul.to_be_bytes::<32>());
    data.extend_from_slice(&at.to_be_bytes::<32>());
    if !tok.ctx.add_log(vec![TOPIC_UI_MULTIPLIER_UPDATED], data) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

/// A matured schedule is already in force, so there is nothing to withdraw.
fn cancel_ui_multiplier(tok: &mut Token<'_, '_>) -> R<()> {
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(ROLE_OPERATOR)?;
    let (mul, at) = tok.pending();
    if at <= tok.ctx.block_time() {
        return Err(rev(ERR_UI_MUL_MISSING, &[]));
    }
    tok.clear_pending();
    let mut data = mul.to_be_bytes::<32>().to_vec();
    data.extend_from_slice(w_u64(at).as_slice());
    if !tok.ctx.add_log(vec![TOPIC_UI_MULTIPLIER_UPDATE_CANCELLED], data) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn update_multiplier(tok: &mut Token<'_, '_>, new_mul: U256) -> R<()> {
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    tok.ensure_role(ROLE_OPERATOR)?;
    if new_mul.is_zero() || new_mul > U128_MASK {
        return Err(rev(ERR_INVALID_MULTIPLIER, &[]));
    }
    // The instant setter is the failsafe and overrides any schedule: a live one is
    // withdrawn loudly, a matured one is stale state and goes quietly.
    let now = tok.ctx.block_time();
    let previous = tok.effective_multiplier(now);
    let (pending_mul, at) = tok.pending();
    if at != 0 {
        tok.clear_pending();
        if at > now {
            let mut data = pending_mul.to_be_bytes::<32>().to_vec();
            data.extend_from_slice(w_u64(at).as_slice());
            if !tok.ctx.add_log(vec![TOPIC_UI_MULTIPLIER_UPDATE_CANCELLED], data) {
                return Err(Cas20Err::OutOfGas);
            }
        }
    }
    tok.set_multiplier(new_mul);
    if !tok.ctx.add_log(vec![TOPIC_MULTIPLIER_UPDATED], new_mul.to_be_bytes::<32>().to_vec()) {
        return Err(Cas20Err::OutOfGas);
    }
    // Emitted by both setters so one stream carries every change.
    let mut data = previous.to_be_bytes::<32>().to_vec();
    data.extend_from_slice(&new_mul.to_be_bytes::<32>());
    data.extend_from_slice(&U256::from(now).to_be_bytes::<32>());
    if !tok.ctx.add_log(vec![TOPIC_UI_MULTIPLIER_UPDATED], data) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn batch_mint(tok: &mut Token<'_, '_>, args: &[u8]) -> R<()> {
    let recipients = read_word_array(args, 0)?;
    let amounts = read_word_array(args, 1)?;
    if tok.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    if tok.is_paused(PAUSE_MINT) {
        return Err(rev(ERR_CONTRACT_PAUSED, &[w_u8(PAUSE_MINT)]));
    }
    tok.ensure_role(ROLE_MINT)?;
    if recipients.len() != amounts.len() {
        return Err(rev(
            ERR_LENGTH_MISMATCH,
            &[w_u64(recipients.len() as u64), w_u64(amounts.len() as u64)],
        ));
    }
    if recipients.is_empty() {
        return Err(rev(ERR_EMPTY_BATCH, &[]));
    }
    for (r, a) in recipients.iter().zip(&amounts) {
        // charge_gas only marks the frame, so without this an exhausted batch would
        // still run to completion before being discarded.
        if tok.ctx.out_of_gas() {
            return Err(Cas20Err::OutOfGas);
        }
        let to: Address = address_from_word(*r).ok_or_else(revert)?;
        tok.mint_core(to, U256::from_be_bytes(a.0))?;
    }
    Ok(())
}

#[allow(dead_code)]
fn _topics_used(_: B256) {}
