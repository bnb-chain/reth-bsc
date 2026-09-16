//! A token's whole configuration read from one state, the way a sequence of
//! eth_calls at one block would read it: the `eth_getCAS20TokenInfo` result, off
//! the consensus path. Ported from go-bsc's core/vm/cas20_info.go.

use super::{
    asset::apply_multiplier,
    ctx::{Cas20State, Ctx, Frame},
    is_cas20_address,
    permit::domain_separator,
    sigs::MARKER_CODE_HASH,
    stablecoin::{stablecoin_slot, SLOT_CURRENCY},
    storage::{slot_at, SLOT_CONTRACT_URI, SLOT_NAME, SLOT_SYMBOL},
    token::{Token, PAUSE_SEIZE},
    Cas20Version, VARIANT_ASSET, VARIANT_STABLECOIN,
};
use alloy_primitives::{Address, B256, KECCAK256_EMPTY, U256, U64};
use reth_provider::StateProvider;
use revm::{
    bytecode::Bytecode,
    interpreter::{SStoreResult, StateLoad},
};
use serde::{Deserialize, Serialize};

/// Bounds what one unmetered read will walk. No single transaction can store a
/// string this long, so it is only ever met on state that did not come from the
/// precompile.
pub const RPC_MAX_STRING_LEN: u64 = 256 << 10;

/// Why a token's configuration could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InfoError {
    /// An address outside the CAS20 space, or one no createCAS20 has initialized.
    #[error("not an initialized CAS20 token")]
    NotToken,
    #[error("CAS20 string exceeds the RPC read bound")]
    StringTooLong,
    #[error("state read failed: {0}")]
    State(String),
}

/// The eth_getCAS20TokenInfo result. Variant-specific fields are omitted for the
/// other variant, and `pending_multiplier` is omitted unless a schedule is live
/// at the block; `multiplier` is the effective value, a matured schedule folded in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub address: Address,
    pub variant: &'static str,
    pub name: String,
    pub symbol: String,
    pub decimals: U64,
    #[serde(rename = "contractURI")]
    pub contract_uri: String,
    pub total_supply: U256,
    pub supply_cap: U256,
    pub paused_features: Vec<U64>,
    pub policies: PolicyBindings,
    pub domain_separator: B256,

    // Asset only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiplier: Option<U256>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_multiplier: Option<PendingMultiplier>,
    #[serde(rename = "totalSupplyUI", skip_serializing_if = "Option::is_none")]
    pub total_supply_ui: Option<U256>,

    // Stablecoin only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyBindings {
    pub transfer_sender: U64,
    pub transfer_receiver: U64,
    pub transfer_executor: U64,
    pub mint_receiver: U64,
    pub seize_holder: U64,
    pub seize_receiver: U64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingMultiplier {
    pub value: U256,
    pub effective_at: U64,
}

/// Reads `addr`'s configuration from `state` as of a block with the given time.
/// Unmetered in effect (the budget is unbounded): for RPC, never for a precompile
/// frame.
pub fn token_info_at(
    state: &mut dyn Cas20State,
    chain_id: u64,
    addr: Address,
    block_time: u64,
) -> Result<TokenInfo, InfoError> {
    let variant = match addr[10] {
        VARIANT_ASSET if is_cas20_address(addr) => "asset",
        VARIANT_STABLECOIN if is_cas20_address(addr) => "stablecoin",
        _ => return Err(InfoError::NotToken),
    };
    let code = state.code_hash(addr).map_err(InfoError::State)?;
    if code.data != MARKER_CODE_HASH {
        return Err(InfoError::NotToken);
    }

    let mut frame = Frame::new(state, u64::MAX, Cas20Version::V1);
    let info = {
        let ctx = Ctx {
            frame: &mut frame,
            self_addr: addr,
            caller: Address::ZERO,
            read_only: true,
            direct_call: true,
            value: U256::ZERO,
            admin_renounced: false,
        };
        let decimals = if addr[10] == VARIANT_STABLECOIN { 6 } else { 0 };
        let mut tok = Token::new(ctx, decimals);
        read(&mut tok, variant, chain_id, addr, block_time)
    };
    if let Some(msg) = frame.fatal.take() {
        return Err(InfoError::State(msg));
    }
    info
}

fn read(
    tok: &mut Token<'_, '_>,
    variant: &'static str,
    chain_id: u64,
    addr: Address,
    block_time: u64,
) -> Result<TokenInfo, InfoError> {
    let mut strings = vec![slot_at(SLOT_NAME), slot_at(SLOT_SYMBOL), slot_at(SLOT_CONTRACT_URI)];
    if addr[10] == VARIANT_STABLECOIN {
        strings.push(stablecoin_slot(SLOT_CURRENCY));
    }
    for slot in strings {
        if tok.s().string_chunks(slot) > RPC_MAX_STRING_LEN / 32 {
            return Err(InfoError::StringTooLong);
        }
    }
    let text = |b: Option<Vec<u8>>| String::from_utf8_lossy(&b.unwrap_or_default()).into_owned();
    let name = tok.s().name().unwrap_or_default();
    let paused = tok.s().paused();
    let (sender, receiver, executor) = tok.s().transfer_policies();
    let (holder, seize_to) = tok.s().seize_policies();
    let mint_receiver = tok.s().mint_receiver_policy();
    let total_supply = tok.s().total_supply();
    let mut info = TokenInfo {
        address: addr,
        variant,
        name: String::from_utf8_lossy(&name).into_owned(),
        symbol: text(tok.s().symbol()),
        decimals: U64::ZERO,
        contract_uri: text(tok.s().contract_uri()),
        total_supply,
        supply_cap: tok.s().supply_cap(),
        paused_features: (0..=PAUSE_SEIZE)
            .filter(|&f| paused.bit(f as usize))
            .map(U64::from)
            .collect(),
        policies: PolicyBindings {
            transfer_sender: U64::from(sender),
            transfer_receiver: U64::from(receiver),
            transfer_executor: U64::from(executor),
            mint_receiver: U64::from(mint_receiver),
            seize_holder: U64::from(holder),
            seize_receiver: U64::from(seize_to),
        },
        domain_separator: domain_separator(&name, chain_id, addr),
        multiplier: None,
        pending_multiplier: None,
        total_supply_ui: None,
        currency: None,
    };
    if addr[10] == VARIANT_ASSET {
        info.decimals = U64::from(tok.asset_decimals());
        let mul = tok.effective_multiplier(block_time);
        info.multiplier = Some(mul);
        let (pending, at) = tok.pending();
        if at > block_time {
            info.pending_multiplier =
                Some(PendingMultiplier { value: pending, effective_at: U64::from(at) });
        }
        // Both factors are bounded by type(uint128).max, so this cannot overflow.
        info.total_supply_ui = Some(apply_multiplier(total_supply, mul).unwrap_or_default());
    } else {
        info.decimals = U64::from(6);
        info.currency = Some(text(tok.currency()));
    }
    Ok(info)
}

/// A read-only host over a state provider, for reads outside any frame.
pub struct ProviderHost<'a> {
    pub state: &'a dyn StateProvider,
    pub time: u64,
    pub chain_id: u64,
}

impl Cas20State for ProviderHost<'_> {
    fn sload(&mut self, address: Address, key: U256) -> Result<StateLoad<U256>, String> {
        let v = self.state.storage(address, B256::from(key)).map_err(|e| e.to_string())?;
        Ok(StateLoad::new(v.unwrap_or_default(), false))
    }

    fn sstore(&mut self, _: Address, _: U256, _: U256) -> Result<StateLoad<SStoreResult>, String> {
        Err("write through a read-only host".into())
    }

    fn code_hash(&mut self, address: Address) -> Result<StateLoad<B256>, String> {
        let acc = self.state.basic_account(&address).map_err(|e| e.to_string())?;
        Ok(StateLoad::new(acc.and_then(|a| a.bytecode_hash).unwrap_or(KECCAK256_EMPTY), false))
    }

    fn set_code(&mut self, _: Address, _: Bytecode) -> Result<(), String> {
        Err("write through a read-only host".into())
    }

    fn log(&mut self, _: alloy_primitives::Log) {}

    fn block_timestamp(&self) -> u64 {
        self.time
    }

    fn chain_id(&self) -> u64 {
        self.chain_id
    }
}
