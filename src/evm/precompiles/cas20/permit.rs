//! EIP-2612 permit and the memo entry points. The EIP-712 domain is derived from
//! the live token name, so updateName invalidates outstanding permits; there is no
//! cached separator to roll. Ported from core/vm/cas20_permit.go.

use super::{abi::*, ctx::ECRECOVER_GAS, errors::*, sigs::*, storage::addr_key, token::Token};
use alloy_primitives::{keccak256, Address, B256, B512, U256};

pub(crate) const EIP712_VERSION: &[u8] = b"1";

impl Token<'_, '_> {
    pub(crate) fn dispatch_permit_memo(
        &mut self,
        sel: Selector,
        args: &[u8],
    ) -> Option<R<Vec<u8>>> {
        Some(match sel {
            SEL_DOMAIN_SEPARATOR => self.domain_separator().map(enc_word).ok_or(Cas20Err::OutOfGas),
            SEL_NONCES => (|| {
                let owner = read_address(args, 0)?;
                Ok(enc_u256(self.s().nonce(owner)))
            })(),
            SEL_PERMIT => self.decode_permit(args),

            SEL_TRANSFER_WITH_MEMO => (|| {
                let (to, amount, memo) = read_to_amount_memo(args)?;
                let ret = self.transfer(self.ctx.caller, to, amount)?;
                if !self.emit_memo(memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(ret)
            })(),
            SEL_TRANSFER_FROM_WITH_MEMO => (|| {
                let from = read_address(args, 0)?;
                let to = read_address(args, 1)?;
                let amount = read_u256(args, 2)?;
                let memo = read_word(args, 3)?;
                let ret = self.transfer_from(self.ctx.caller, from, to, amount)?;
                if !self.emit_memo(memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(ret)
            })(),
            SEL_MINT_WITH_MEMO => (|| {
                let (to, amount, memo) = read_to_amount_memo(args)?;
                self.mint(to, amount)?;
                if !self.emit_memo(memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(Vec::new())
            })(),
            SEL_BURN_WITH_MEMO => (|| {
                let amount = read_u256(args, 0)?;
                let memo = read_word(args, 1)?;
                self.burn(self.ctx.caller, amount)?;
                if !self.emit_memo(memo) {
                    return Err(Cas20Err::OutOfGas);
                }
                Ok(Vec::new())
            })(),
            _ => return None,
        })
    }

    pub(crate) fn emit_memo(&mut self, memo: B256) -> bool {
        let caller = self.ctx.caller;
        self.ctx.add_log(vec![TOPIC_MEMO, addr_key(caller), memo], Vec::new())
    }

    // --- EIP-2612 permit --------------------------------------------------------

    fn domain_separator(&mut self) -> Option<B256> {
        let name = self.s().name()?;
        if !self.ctx.charge_keccak(name.len())
            || !self.ctx.charge_keccak(EIP712_VERSION.len())
            || !self.ctx.charge_keccak(160)
        {
            return None;
        }
        Some(domain_separator(&name, self.ctx.chain_id(), self.ctx.self_addr))
    }

    fn decode_permit(&mut self, args: &[u8]) -> R<Vec<u8>> {
        let owner = read_address(args, 0)?;
        let spender = read_address(args, 1)?;
        let value = read_u256(args, 2)?;
        let deadline = read_u256(args, 3)?;
        let v = read_strict_uint8(args, 4)?;
        let r = read_word(args, 5)?;
        let s = read_word(args, 6)?;
        self.permit(owner, spender, value, deadline, v, r, s)
    }

    #[allow(clippy::too_many_arguments)]
    fn permit(
        &mut self,
        owner: Address,
        spender: Address,
        value: U256,
        deadline: U256,
        v: u8,
        r: B256,
        s: B256,
    ) -> R<Vec<u8>> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        if owner.is_zero() {
            return Err(rev(ERR_INVALID_APPROVER, &[addr_key(owner)]));
        }
        if deadline < U256::from(self.ctx.block_time()) {
            return Err(rev(ERR_EXPIRED_SIGNATURE, &[w_u256(deadline)]));
        }
        let nonce = self.s().nonce(owner);

        let mut struct_hash = Vec::with_capacity(192);
        struct_hash.extend_from_slice(PERMIT_TYPEHASH.as_slice());
        struct_hash.extend_from_slice(addr_key(owner).as_slice());
        struct_hash.extend_from_slice(addr_key(spender).as_slice());
        struct_hash.extend_from_slice(&value.to_be_bytes::<32>());
        struct_hash.extend_from_slice(&nonce.to_be_bytes::<32>());
        struct_hash.extend_from_slice(&deadline.to_be_bytes::<32>());

        let dom = self.domain_separator().ok_or(Cas20Err::OutOfGas)?;
        // Charged whether or not the signature turns out to be valid, as ECRECOVER would be.
        if !self.ctx.charge_keccak(struct_hash.len())
            || !self.ctx.charge_keccak(66)
            || !self.ctx.charge_gas(ECRECOVER_GAS)
        {
            return Err(Cas20Err::OutOfGas);
        }
        let mut pre = Vec::with_capacity(66);
        pre.extend_from_slice(&[0x19, 0x01]);
        pre.extend_from_slice(dom.as_slice());
        pre.extend_from_slice(keccak256(&struct_hash).as_slice());
        let digest = keccak256(pre);

        let signer = ecrecover_address(digest, v, r, s);
        if signer != Some(owner) {
            return Err(rev(
                ERR_INVALID_SIGNER,
                &[addr_key(signer.unwrap_or_default()), addr_key(owner)],
            ));
        }

        // After the signature, so a bad signature naming the zero spender is InvalidSigner.
        if spender.is_zero() {
            return Err(rev(ERR_INVALID_SPENDER, &[addr_key(spender)]));
        }
        self.s().set_nonce(owner, nonce.wrapping_add(U256::from(1)));
        self.s().set_allowance(owner, spender, value);
        if !self.emit(TOPIC_APPROVAL, owner, spender, value) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(Vec::new())
    }
}

pub(crate) fn read_to_amount_memo(args: &[u8]) -> R<(Address, U256, B256)> {
    Ok((read_address(args, 0)?, read_u256(args, 1)?, read_word(args, 2)?))
}

pub(crate) fn domain_separator(name: &[u8], chain_id: u64, this: Address) -> B256 {
    let mut enc = Vec::with_capacity(160);
    enc.extend_from_slice(DOMAIN_TYPEHASH.as_slice());
    enc.extend_from_slice(keccak256(name).as_slice());
    enc.extend_from_slice(keccak256(EIP712_VERSION).as_slice());
    enc.extend_from_slice(&U256::from(chain_id).to_be_bytes::<32>());
    enc.extend_from_slice(addr_key(this).as_slice());
    keccak256(enc)
}

const SECP256K1_N: U256 = U256::from_be_bytes(alloy_primitives::hex!(
    "fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364141"
));
const SECP256K1_HALF_N: U256 = U256::from_be_bytes(alloy_primitives::hex!(
    "7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0"
));

/// EIP-2 low-s and v ∈ {27,28}; ERC-1271 contract signatures are not supported.
pub(crate) fn ecrecover_address(hash: B256, v: u8, r: B256, s: B256) -> Option<Address> {
    if v != 27 && v != 28 {
        return None;
    }
    let (rv, sv) = (U256::from_be_bytes(r.0), U256::from_be_bytes(s.0));
    if rv.is_zero()
        || sv.is_zero()
        || sv > SECP256K1_HALF_N
        || rv >= SECP256K1_N
        || sv >= SECP256K1_N
    {
        return None;
    }
    let mut sig = [0u8; 64];
    sig[..32].copy_from_slice(r.as_slice());
    sig[32..].copy_from_slice(s.as_slice());
    let pubkey_hash =
        revm::precompile::secp256k1::ecrecover(&B512::from(sig), v - 27, &hash).ok()?;
    Some(Address::from_slice(&pubkey_hash[12..]))
}
