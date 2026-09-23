//! CAS20 stateful precompiles (BEP-702), enabled at Jenner.
//! Routes the factory, registries and `0xCA52…` token addresses by prefix.
//! Ported from go-bsc's core/vm/cas20*.go; `sigs` and the fixture pin selectors and storage layout.

#![warn(dead_code)]

mod abi;
pub(crate) mod access_list;
mod activation;
mod admin;
mod asset;
mod ctx;
mod errors;
mod factory;
pub(crate) mod info;
mod memo;
mod metadata;
mod observer;
mod permit;
mod policy;
mod sigs;
mod stablecoin;
mod storage;
mod token;

#[cfg(test)]
mod test_host;
#[cfg(test)]
mod tests;

pub use observer::enable_metrics;

use self::{
    ctx::{had_no_code, Ctx, Frame},
    errors::{complete, rev, Cas20Err, Outcome, R},
    observer::{CallRecord, CallStats, CallStatus, Cas20Observer, NodeObserver},
    sigs::{ERR_NON_PAYABLE, FEATURE_ASSET, FEATURE_STABLECOIN, MARKER_CODE_HASH},
    token::Token,
};
use crate::hardforks::bsc::BscHardfork;
use alloy_evm::precompiles::{DynPrecompile, PrecompileInput, PrecompileLookup};
use alloy_primitives::{address, Address, Bytes, B256, U256};
use revm::{
    bytecode::Bytecode,
    precompile::{
        PrecompileError, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult,
    },
};
use std::{
    borrow::Cow,
    sync::{Arc, LazyLock},
    time::Instant,
};

/// Opens every CAS20 token address.
pub(crate) const MARKER_PREFIX: [u8; 2] = [0xca, 0x52];

pub(crate) const VARIANT_ASSET: u8 = 0x00;
pub(crate) const VARIANT_STABLECOIN: u8 = 0x01;
pub(crate) const VARIANT_MAX: u8 = VARIANT_STABLECOIN;

pub const FACTORY_ADDRESS: Address = address!("CA5F000000000000000000000000000000000000");
pub const ACTIVATION_REGISTRY_ADDRESS: Address =
    address!("7020000000000000000000000000000000000001");
pub const POLICY_REGISTRY_ADDRESS: Address = address!("7020000000000000000000000000000000000002");

/// Undeployable under EIP-3541; nonempty code prevents EIP-161 account clearing.
pub const MARKER_CODE: [u8; 1] = [0xEF];

/// type(uint128).max: the supply cap a token is created with.
pub(crate) const NO_SUPPLY_CAP: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

pub fn marker_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from_static(&MARKER_CODE))
}

/// byte[0:2] == 0xCA52 and byte[2:10] all zero.
pub fn is_cas20_address(addr: Address) -> bool {
    addr[0] == MARKER_PREFIX[0]
        && addr[1] == MARKER_PREFIX[1]
        && addr[2..10].iter().all(|&b| b == 0)
}

/// One entry per ordinal, so routing and feature gating cannot disagree.
pub(crate) fn variant_feature(variant: u8) -> Option<B256> {
    match variant {
        VARIANT_ASSET => Some(FEATURE_ASSET),
        VARIANT_STABLECOIN => Some(FEATURE_STABLECOIN),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Factory,
    Policy,
    Activation,
    Asset,
    Stablecoin,
}

impl Kind {
    const ALL: [Kind; 5] =
        [Kind::Factory, Kind::Policy, Kind::Activation, Kind::Asset, Kind::Stablecoin];

    /// The name go-bsc reports for the precompile.
    pub const fn name(self) -> &'static str {
        match self {
            Kind::Factory => "CAS20Factory",
            Kind::Policy => "CAS20PolicyRegistry",
            Kind::Activation => "CAS20ActivationRegistry",
            Kind::Asset => "CAS20Asset",
            Kind::Stablecoin => "CAS20Stablecoin",
        }
    }

    fn id(self) -> PrecompileId {
        PrecompileId::Custom(Cow::Borrowed(self.name()))
    }
}

/// Leaves fork gating to the caller.
fn resolve(addr: Address) -> Option<Kind> {
    match [addr[0], addr[1]] {
        [0xca, 0x52] if is_cas20_address(addr) => match addr[10] {
            VARIANT_ASSET => Some(Kind::Asset),
            VARIANT_STABLECOIN => Some(Kind::Stablecoin),
            _ => None,
        },
        [0xca, 0x5f] if addr == FACTORY_ADDRESS => Some(Kind::Factory),
        [0x70, 0x20] => match addr {
            POLICY_REGISTRY_ADDRESS => Some(Kind::Policy),
            ACTIVATION_REGISTRY_ADDRESS => Some(Kind::Activation),
            _ => None,
        },
        _ => None,
    }
}

pub(crate) fn is_cas20_precompile(address: Address) -> bool {
    resolve(address).is_some()
}

/// Prefix lookup enabled at Jenner. Shared stateful precompiles disable result caching.
#[derive(Clone, Debug)]
pub struct Cas20Lookup {
    active: Option<Arc<[DynPrecompile; 5]>>,
    disabled: std::collections::BTreeSet<Address>,
}

impl Cas20Lookup {
    /// The production lookup for `spec`, with precompiles shared process-wide.
    pub fn new(spec: BscHardfork) -> Self {
        static PRECOMPILES: LazyLock<Arc<[DynPrecompile; 5]>> =
            LazyLock::new(|| Arc::new(precompiles(NodeObserver)));
        Self {
            active: (spec >= BscHardfork::Jenner).then(|| PRECOMPILES.clone()),
            disabled: Default::default(),
        }
    }

    /// Disables native routing only for code overrides in this RPC execution.
    pub fn with_disabled(mut self, disabled: std::collections::BTreeSet<Address>) -> Self {
        self.disabled = disabled;
        self
    }

    /// Select observation explicitly in benchmarks, independently of node startup.
    #[cfg(any(test, feature = "bench-test"))]
    pub fn with_metrics(spec: BscHardfork, enabled: bool) -> Self {
        Self {
            active: (spec >= BscHardfork::Jenner)
                .then(|| Arc::new(precompiles(enabled.then(observer::MetricsObserver::register)))),
            disabled: Default::default(),
        }
    }

    /// A lookup for `spec` reporting to `observer`.
    #[cfg(test)]
    fn with_observer<O: Cas20Observer>(spec: BscHardfork, observer: O) -> Self {
        Self {
            active: (spec >= BscHardfork::Jenner).then(|| Arc::new(precompiles(observer))),
            disabled: Default::default(),
        }
    }
}

impl PrecompileLookup for Cas20Lookup {
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        let entries = self.active.as_ref()?;
        let kind = resolve(*address)?;
        if self.disabled.contains(address) {
            return None;
        }
        Some(entries[kind as usize].clone())
    }
}

fn precompiles<O: Cas20Observer>(observer: O) -> [DynPrecompile; 5] {
    Kind::ALL.map(|kind| {
        let observer = observer.clone();
        DynPrecompile::new_stateful(kind.id(), move |input| run(kind, &observer, input))
    })
}

fn run<O: Cas20Observer>(kind: Kind, observer: &O, input: PrecompileInput<'_>) -> PrecompileResult {
    let started = observer.enabled().then(Instant::now);
    let direct_call = input.is_direct_call();
    let (data, gas_limit, reservoir) = (input.data, input.gas, input.reservoir);
    let mut internals = input.internals;
    let mut frame = Frame::new(&mut internals, gas_limit);
    let ctx = Ctx {
        frame: &mut frame,
        self_addr: input.bytecode_address,
        caller: input.caller,
        read_only: input.is_static,
        direct_call,
        value: input.value,
        admin_renounced: false,
    };
    let result = execute(kind, ctx, data);
    let (outcome, used, refund) = complete(&mut frame, result);
    if let Some(started) = started {
        observer.record_call(&CallRecord {
            kind,
            selector: sigs::selector_index(data),
            status: outcome.status(),
            gas_used: used,
            elapsed: started.elapsed(),
            stats: frame.stats,
        });
    }
    match outcome {
        Outcome::Return(bytes) => {
            let mut out = PrecompileOutput::new(used, bytes.into(), reservoir);
            out.gas_refunded = refund;
            Ok(out)
        }
        Outcome::Revert(bytes) => Ok(PrecompileOutput::revert(used, bytes.into(), reservoir)),
        Outcome::OutOfGas => Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir)),
        Outcome::Fatal(msg) => Err(PrecompileError::Fatal(msg)),
    }
}

/// One CAS20 call, from the entry prologue to the exit shape, over any host state.
fn execute(kind: Kind, mut ctx: Ctx<'_, '_>, data: &[u8]) -> R<Vec<u8>> {
    if !ctx.direct_call {
        return Err(Cas20Err::DelegateCall);
    }
    if !ctx.value.is_zero() {
        return Err(rev(ERR_NON_PAYABLE, &[]));
    }
    if !ctx.charge_calldata(data) {
        return Err(Cas20Err::OutOfGas);
    }
    match kind {
        Kind::Factory => factory::run_factory(&mut ctx, data),
        Kind::Policy => policy::run_policy(&mut ctx, data),
        Kind::Activation => activation::run_activation(&mut ctx, data),
        Kind::Asset | Kind::Stablecoin => {
            let address = ctx.self_addr;
            if !initialized_metered(&mut ctx, address) {
                return Err(errors::revert());
            }
            let decimals = if kind == Kind::Stablecoin { 6 } else { 0 };
            let mut token = Token::new(ctx, decimals);
            if kind == Kind::Asset {
                asset::asset_dispatch(&mut token, data)
            } else {
                stablecoin::stablecoin_dispatch(&mut token, data)
            }
        }
    }
}

/// Exact-hash, not non-empty: foreign code is not a token.
pub(crate) fn initialized_metered(ctx: &mut Ctx<'_, '_>, addr: Address) -> bool {
    match ctx.charge_account_access(addr) {
        Some(hash) => hash == MARKER_CODE_HASH,
        None => false,
    }
}

/// Any code counts, not just the sentinel: overwriting foreign code would destroy it.
pub(crate) fn address_occupied(ctx: &mut Ctx<'_, '_>, addr: Address) -> bool {
    match ctx.charge_account_access(addr) {
        // True is the safe answer when the frame can no longer pay to find out.
        None => true,
        Some(hash) => !had_no_code(hash),
    }
}
