//! Names, symbol, contractURI and the ERC-5267 domain view. Metadata writes are
//! not pause-gated (BEP-702 3.6). Ported from core/vm/cas20_metadata.go.

use super::{
    abi::*,
    errors::*,
    permit::EIP712_VERSION,
    sigs::*,
    storage::addr_key,
    token::{Token, PAUSE_SEIZE},
};
use alloy_primitives::{B256, U256};

impl Token<'_, '_> {
    pub(crate) fn dispatch_metadata(&mut self, sel: Selector, args: &[u8]) -> Option<R<Vec<u8>>> {
        Some(match sel {
            SEL_CONTRACT_URI => match self.s().contract_uri() {
                Some(v) => Ok(enc_string(&v)),
                None => Err(Cas20Err::OutOfGas),
            },
            SEL_SUPPLY_CAP => {
                let v = self.s().supply_cap();
                Ok(enc_u256(v))
            }
            SEL_PAUSED_FEATURES => Ok(encode_tuple(&[abi_word_array(&self.paused_features())])),
            SEL_EIP712_DOMAIN => self.eip712_domain().ok_or(Cas20Err::OutOfGas),

            SEL_UPDATE_NAME => (|| {
                let v = read_string_arg(args, 0)?;
                self.update_name(v).map(|_| Vec::new())
            })(),
            SEL_UPDATE_SYMBOL => (|| {
                let v = read_string_arg(args, 0)?;
                self.update_symbol(v).map(|_| Vec::new())
            })(),
            SEL_UPDATE_CONTRACT_URI => (|| {
                let v = read_string_arg(args, 0)?;
                self.update_contract_uri(v).map(|_| Vec::new())
            })(),
            _ => return None,
        })
    }

    fn ensure_metadata_write(&mut self) -> R<()> {
        if self.ctx.read_only {
            return Err(Cas20Err::WriteProtection);
        }
        self.ensure_role(ROLE_METADATA)
    }

    fn update_name(&mut self, v: &[u8]) -> R<()> {
        self.ensure_metadata_write()?;
        if !self.s().set_name(v) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = self.ctx.caller;
        if !self.ctx.add_log(vec![TOPIC_NAME_UPDATED, addr_key(caller)], enc_string(v)) {
            return Err(Cas20Err::OutOfGas);
        }
        if !self.ctx.add_log(vec![TOPIC_EIP712_DOMAIN_CHANGED], Vec::new()) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    fn update_symbol(&mut self, v: &[u8]) -> R<()> {
        self.ensure_metadata_write()?;
        if !self.s().set_symbol(v) {
            return Err(Cas20Err::OutOfGas);
        }
        let caller = self.ctx.caller;
        if !self.ctx.add_log(vec![TOPIC_SYMBOL_UPDATED, addr_key(caller)], enc_string(v)) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    fn update_contract_uri(&mut self, v: &[u8]) -> R<()> {
        self.ensure_metadata_write()?;
        if !self.s().set_contract_uri(v) {
            return Err(Cas20Err::OutOfGas);
        }
        if !self.ctx.add_log(vec![TOPIC_CONTRACT_URI_UPDATED], Vec::new()) {
            return Err(Cas20Err::OutOfGas);
        }
        Ok(())
    }

    pub(crate) fn paused_features(&mut self) -> Vec<B256> {
        let p = self.s().paused();
        (0..=PAUSE_SEIZE).filter(|&f| p.bit(f as usize)).map(w_u8).collect()
    }

    /// ERC-5267. fields 0x0f names the four members domainSeparator hashes.
    fn eip712_domain(&mut self) -> Option<Vec<u8>> {
        let name = self.s().name()?;
        let mut fields = B256::ZERO;
        fields.0[0] = 0x0f;
        Some(encode_tuple(&[
            abi_word(fields),
            abi_string(&name),
            abi_string(EIP712_VERSION),
            abi_word(w_u256(U256::from(self.ctx.chain_id()))),
            abi_word(addr_key(self.ctx.self_addr)),
            abi_word(B256::ZERO),
            abi_word_array(&[]),
        ]))
    }
}
