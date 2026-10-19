//! The factory: address derivation and token creation with its bootstrap bundle.
//! Ported from core/vm/cas20_factory.go.

use super::{
    abi::*,
    activation::ensure_feature_activated,
    address_occupied,
    asset::{asset_dispatch, init_asset_extension},
    ctx::Ctx,
    errors::*,
    initialized_metered, is_cas20_address, marker_bytecode,
    sigs::*,
    stablecoin::stablecoin_dispatch,
    storage::addr_key,
    token::Token,
    variant_feature, FACTORY_ADDRESS, MARKER_CODE, MARKER_PREFIX, NO_SUPPLY_CAP, VARIANT_ASSET,
    VARIANT_MAX, VARIANT_STABLECOIN,
};
use alloy_primitives::{keccak256, Address, B256, U256};

const PARAMS_VERSION: u8 = 1;

// BEP-702 4.10.
const MIN_DECIMALS: u8 = 6;
const MAX_DECIMALS: u8 = 18;

/// 0xCA52 ++ 8×0x00 ++ variant ++ keccak256(abi.encode(creator, salt))[:9].
pub(crate) fn derive_address(variant: u8, creator: Address, salt: B256) -> Address {
    let mut pre = [0u8; 64];
    pre[..32].copy_from_slice(addr_key(creator).as_slice());
    pre[32..].copy_from_slice(salt.as_slice());
    let h = keccak256(pre);
    let mut a = [0u8; 20];
    a[0] = MARKER_PREFIX[0];
    a[1] = MARKER_PREFIX[1];
    a[10] = variant;
    a[11..20].copy_from_slice(&h[..9]);
    Address::from(a)
}

pub(crate) fn run_factory(ctx: &mut Ctx<'_, '_>, input: &[u8]) -> R<Vec<u8>> {
    if input.len() < 4 {
        return Err(revert());
    }
    let sel: Selector = input[..4].try_into().unwrap();
    let args = &input[4..];

    match sel {
        SEL_GET_CAS20_ADDRESS => {
            let variant = read_word(args, 0)?;
            let sender = read_address(args, 1)?;
            let salt = read_word(args, 2)?;
            // Decoded as createCAS20 decodes it, so prediction and creation agree.
            if !is_enum_word(variant, VARIANT_MAX) {
                return Err(revert());
            }
            if !ctx.charge_keccak(64) {
                return Err(Cas20Err::OutOfGas);
            }
            Ok(enc_word(addr_key(derive_address(variant.0[31], sender, salt))))
        }
        SEL_IS_CAS20 => {
            let a = read_address(args, 0)?;
            Ok(enc_bool(is_cas20_address(a)))
        }
        SEL_VARIANT_OF => {
            let a = read_address(args, 0)?;
            // The return type is an enum, so an unrecognized variant reverts rather
            // than handing the caller a value its decoder rejects.
            if !is_cas20_address(a) || variant_feature(a[10]).is_none() {
                return Err(rev(ERR_INVALID_VARIANT, &[]));
            }
            Ok(enc_word(w_u8(a[10])))
        }
        SEL_IS_CAS20_INITIALIZED => {
            let a = read_address(args, 0)?;
            Ok(enc_bool(is_cas20_address(a) && initialized_metered(ctx, a)))
        }
        SEL_CREATE_CAS20 => create_cas20(ctx, args),
        _ => Err(revert()),
    }
}

fn create_cas20(ctx: &mut Ctx<'_, '_>, args: &[u8]) -> R<Vec<u8>> {
    if ctx.read_only {
        return Err(Cas20Err::WriteProtection);
    }
    let variant_word = read_word(args, 0)?;
    let salt = read_word(args, 1)?;
    let params = read_bytes_arg(args, 2)?;
    let init_calls = read_bytes_array(args, 3)?;

    // Variant and feature gate before the params blob (BEP-702 3.4): a closed
    // feature is reported as such whatever the payload.
    if !is_enum_word(variant_word, VARIANT_MAX) {
        return Err(revert());
    }
    let variant = variant_word.0[31];
    let feature = variant_feature(variant).ok_or_else(|| rev(ERR_INVALID_VARIANT, &[]))?;
    ensure_feature_activated(ctx, feature)?;
    let create = decode_create_params(variant, params)?;
    if variant == VARIANT_STABLECOIN {
        validate_currency(&create.currency)?;
    }
    let creator = ctx.caller;
    if !ctx.charge_keccak(64) {
        return Err(Cas20Err::OutOfGas);
    }
    let addr = derive_address(variant, creator, salt);

    if address_occupied(ctx, addr) {
        return Err(rev(ERR_TOKEN_ALREADY_EXISTS, &[addr_key(addr)]));
    }
    if !ctx.charge_code_write(addr, &MARKER_CODE) {
        return Err(Cas20Err::OutOfGas);
    }
    ctx.frame.set_code(addr, marker_bytecode());

    let decimals = create.decimals;
    {
        let mut tok = Token::bootstrap(ctx.spawn_bootstrap(addr, FACTORY_ADDRESS), decimals);

        if !tok.s().set_name(&create.name) || !tok.s().set_symbol(&create.symbol) {
            return Err(Cas20Err::OutOfGas);
        }
        tok.s().set_supply_cap(NO_SUPPLY_CAP);
        if variant == VARIANT_ASSET {
            init_asset_extension(&mut tok, create.decimals);
        } else if !tok.set_currency(&create.currency) {
            return Err(Cas20Err::OutOfGas);
        }
    }
    // The factory announces the token before the admin grant and initCalls. This
    // charge must also precede them: moving just the log changes low-gas outcomes.
    if !ctx.add_log(
        vec![TOPIC_CAS20_CREATED, addr_key(addr), w_u8(variant)],
        encode_created_data(&create),
    ) {
        return Err(Cas20Err::OutOfGas);
    }
    {
        let mut tok = Token::bootstrap(ctx.spawn_bootstrap(addr, FACTORY_ADDRESS), decimals);
        let initial_admin = create.initial_admin;
        if !initial_admin.is_zero() {
            tok.s().set_role(ROLE_DEFAULT_ADMIN, initial_admin, true);
            tok.s().set_admin_count(U256::from(1));
            if !tok.ctx.add_log(
                vec![
                    TOPIC_ROLE_GRANTED,
                    ROLE_DEFAULT_ADMIN,
                    addr_key(initial_admin),
                    addr_key(FACTORY_ADDRESS),
                ],
                Vec::new(),
            ) {
                return Err(Cas20Err::OutOfGas);
            }
        }

        // The variant's full dispatcher, not the shared half: an Asset token has to
        // be able to set its multiplier at creation.
        for (i, call) in init_calls.iter().enumerate() {
            if !tok.ctx.charge_internal_dispatch(call) {
                return Err(Cas20Err::OutOfGas);
            }
            if call.len() < 4 {
                return Err(rev_bytes(ERR_INTERNAL_CALL_MALFORMED, call));
            }
            let res = if variant == VARIANT_ASSET {
                asset_dispatch(&mut tok, call)
            } else {
                stablecoin_dispatch(&mut tok, call)
            };
            match res {
                Ok(_) => {}
                Err(Cas20Err::Revert(_)) => {
                    return Err(rev(ERR_INIT_CALL_FAILED, &[w_u64(i as u64)]));
                }
                Err(other) => return Err(other),
            }
        }
    }
    if ctx.out_of_gas() {
        return Err(Cas20Err::OutOfGas);
    }
    // Recorded last: a creation whose bootstrap fails is undone, including its logs.
    ctx.frame.stats.created = Some(variant);
    Ok(enc_word(addr_key(addr)))
}

struct CreateParams {
    variant: u8,
    name: Vec<u8>,
    symbol: Vec<u8>,
    initial_admin: Address,
    decimals: u8,
    /// Stablecoin only
    currency: Vec<u8>,
}

fn decode_create_params(variant: u8, params: &[u8]) -> R<CreateParams> {
    // The single-struct offset word; see abi_encode_struct.
    let off = word_u64(params, 0).ok_or_else(revert)?;
    if off > params.len() as u64 {
        return Err(revert());
    }
    let body = &params[off as usize..];

    let version = read_strict_uint8(body, 0)?;
    if version != PARAMS_VERSION {
        return Err(rev(ERR_UNSUPPORTED_VERSION, &[w_u8(version), w_u8(variant)]));
    }
    let name = read_string_arg(body, 1)?.to_vec();
    let symbol = read_string_arg(body, 2)?.to_vec();
    let initial_admin = read_address(body, 3)?;

    if variant == VARIANT_ASSET {
        let decimals = read_strict_uint8(body, 4)?;
        if !(MIN_DECIMALS..=MAX_DECIMALS).contains(&decimals) {
            return Err(rev(ERR_INVALID_DECIMALS, &[w_u8(decimals)]));
        }
        return Ok(CreateParams {
            variant,
            name,
            symbol,
            initial_admin,
            decimals,
            currency: Vec::new(),
        });
    }

    // Stablecoin decimals are fixed and not carried on the wire.
    let currency = read_string_arg(body, 4)?.to_vec();
    Ok(CreateParams { variant, name, symbol, initial_admin, decimals: 6, currency })
}

fn validate_currency(code: &[u8]) -> R<()> {
    if code.is_empty() {
        return Err(rev_bytes(ERR_MISSING_REQUIRED_FIELD, b"currency"));
    }
    if !code.iter().all(|b| b.is_ascii_uppercase()) {
        return Err(rev_bytes(ERR_INVALID_CURRENCY, code));
    }
    Ok(())
}

fn encode_created_data(c: &CreateParams) -> Vec<u8> {
    let variant_params = if c.variant == VARIANT_STABLECOIN {
        abi_encode_struct(&[abi_word(w_u8(PARAMS_VERSION)), abi_string(&c.currency)])
    } else {
        Vec::new()
    };
    encode_tuple(&[
        abi_string(&c.name),
        abi_string(&c.symbol),
        abi_word(w_u8(c.decimals)),
        abi_bytes(&variant_params),
    ])
}
