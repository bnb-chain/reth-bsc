//! ActivationRegistry: the per-feature governance switch. It gates token creation
//! and PolicyRegistry writes only — deactivation never reaches an existing token,
//! so it cannot freeze balances. Ported from core/vm/cas20_activation.go.

use super::{
    abi::*,
    ctx::Ctx,
    errors::*,
    sigs::*,
    storage::{addr_key, offset_slot, Store},
    ACTIVATION_REGISTRY_ADDRESS,
};
use alloy_primitives::{address, Address, B256, U256};

pub(crate) const NAMESPACE: &str = "bsc.activation_registry";

/// mapping(bytes32 feature => bool)
pub(crate) const SLOT_FEATURES: u64 = 0;
/// address, zero means no admin exists
pub(crate) const SLOT_ADMIN: u64 = 1;

/// The only key this registry takes.
const PARAM_ADMIN: &[u8] = b"admin";

/// The governance contract whose parameter changes appoint the admin.
pub(crate) const GOV_HUB_ADDRESS: Address = address!("0000000000000000000000000000000000001007");

pub(crate) fn act_slot(offset: u64) -> U256 {
    offset_slot(ROOT_ACTIVATION, offset)
}

/// A gas-metered view over the registry's storage.
pub(crate) struct ActivationReg<'r, 'f, 'a> {
    s: Store<'r, 'f, 'a>,
}

impl<'r, 'f, 'a> ActivationReg<'r, 'f, 'a> {
    pub(crate) fn new(ctx: &'r mut Ctx<'f, 'a>) -> Self {
        Self { s: Store::new(ctx, ACTIVATION_REGISTRY_ADDRESS) }
    }

    pub(crate) fn is_activated(&mut self, feature: B256) -> bool {
        let slot = self.s.map_slot(act_slot(SLOT_FEATURES), feature);
        !self.s.get_word(slot).is_zero()
    }

    fn set_activated(&mut self, feature: B256, on: bool) {
        // Cleared, not written false, so the refund matches a Solidity `delete`.
        let slot = self.s.map_slot(act_slot(SLOT_FEATURES), feature);
        self.s.set_word(slot, U256::from(on as u8));
    }

    fn admin(&mut self) -> Address {
        Address::from_word(B256::from(self.s.get_word(act_slot(SLOT_ADMIN))))
    }

    fn set_admin(&mut self, a: Address) {
        self.s.set_word(act_slot(SLOT_ADMIN), U256::from_be_bytes(addr_key(a).0));
    }

    fn require_admin(&mut self) -> R<()> {
        let caller = self.s.ctx.caller;
        let a = self.admin();
        if a.is_zero() || caller != a {
            return Err(rev(ERR_UNAUTHORIZED_ADDR, &[addr_key(caller)]));
        }
        Ok(())
    }
}

fn require_gov(ctx: &Ctx<'_, '_>) -> R<()> {
    if ctx.caller != GOV_HUB_ADDRESS {
        return Err(rev(ERR_UNAUTHORIZED_ADDR, &[addr_key(ctx.caller)]));
    }
    Ok(())
}

pub(crate) fn run_activation(ctx: &mut Ctx<'_, '_>, input: &[u8]) -> R<Vec<u8>> {
    if input.len() < 4 {
        return Err(revert());
    }
    let sel: Selector = input[..4].try_into().unwrap();
    let args = &input[4..];
    let mut reg = ActivationReg::new(ctx);

    match sel {
        // reads (permitted in read-only frames)
        SEL_IS_ACTIVATED => {
            let feature = read_word(args, 0)?;
            Ok(enc_bool(reg.is_activated(feature)))
        }
        SEL_CHECK_ACTIVATED => {
            let feature = read_word(args, 0)?;
            if !reg.is_activated(feature) {
                return Err(rev(ERR_FEATURE_NOT_ACTIVATED, &[feature]));
            }
            Ok(Vec::new())
        }
        SEL_ACTIVATION_ADMIN => Ok(enc_word(addr_key(reg.admin()))),

        // writes: governance appoints the admin, the admin works the switch
        SEL_UPDATE_PARAM => update_param(&mut reg, args).map(|_| Vec::new()),
        SEL_ACTIVATE => set_feature(&mut reg, args, true).map(|_| Vec::new()),
        SEL_DEACTIVATE => set_feature(&mut reg, args, false).map(|_| Vec::new()),
        _ => Err(revert()),
    }
}

/// The governance contract entry point, in the shape every BSC system contract
/// uses. Governance appoints the admin here and the admin works the switch, so a
/// feature opens without a voting period while authority stays with governance.
fn update_param(reg: &mut ActivationReg<'_, '_, '_>, args: &[u8]) -> R<()> {
    if reg.s.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    // Decoded before the authorization check, as Solidity's external decoder does.
    let key = read_string_arg(args, 0)?;
    let value = read_bytes_arg(args, 1)?;
    require_gov(reg.s.ctx)?;

    if key != PARAM_ADMIN {
        return Err(rev_string_bytes(ERR_UNKNOWN_PARAM, key, value));
    }
    if value.len() != 20 {
        return Err(rev_string_bytes(ERR_INVALID_VALUE, key, value));
    }
    let next = Address::from_slice(value);
    if next.is_zero() {
        return Err(rev_string_bytes(ERR_INVALID_VALUE, key, value));
    }
    let previous = reg.admin();
    reg.set_admin(next);
    let caller = reg.s.ctx.caller;
    if !reg.s.ctx.add_log(
        vec![TOPIC_ADMIN_CHANGED, addr_key(previous), addr_key(next), addr_key(caller)],
        Vec::new(),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    // Logged alongside the registry's own event, as every system contract does.
    if !reg
        .s
        .ctx
        .add_log(vec![TOPIC_PARAM_CHANGE], encode_tuple(&[abi_string(key), abi_bytes(value)]))
    {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn set_feature(reg: &mut ActivationReg<'_, '_, '_>, args: &[u8], on: bool) -> R<()> {
    if reg.s.ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    let feature = read_word(args, 0)?;
    reg.require_admin()?;
    let active = reg.is_activated(feature);
    if on && active {
        return Err(rev(ERR_ALREADY_ACTIVATED, &[feature]));
    }
    if !on && !active {
        return Err(rev(ERR_FEATURE_NOT_ACTIVATED, &[feature]));
    }
    reg.set_activated(feature, on);
    let topic = if on { TOPIC_FEATURE_ACTIVATED } else { TOPIC_FEATURE_DEACTIVATED };
    let caller = reg.s.ctx.caller;
    if !reg.s.ctx.add_log(vec![topic, feature, addr_key(caller)], Vec::new()) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

pub(crate) fn ensure_feature_activated(ctx: &mut Ctx<'_, '_>, feature: B256) -> R<()> {
    if !ActivationReg::new(ctx).is_activated(feature) {
        return Err(rev(ERR_FEATURE_NOT_ACTIVATED, &[feature]));
    }
    Ok(())
}
