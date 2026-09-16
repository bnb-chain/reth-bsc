//! CAS20, the Compliant Asset Standard (BEP-702): a token family implemented as
//! stateful precompiles. Every routed address resolves here once Jenner is active:
//! the factory, the two registries, and the `0xCA52…` token space, whose members
//! have no fixed address and are matched by prefix. Ported from go-bsc's
//! core/vm/cas20*.go; the storage layout in testdata/cas20_layout.json and the
//! constants in `sigs` are the contract both clients are held to.

pub(crate) mod abi;
pub(crate) mod activation;
pub(crate) mod admin;
pub(crate) mod asset;
pub(crate) mod ctx;
pub(crate) mod errors;
pub(crate) mod factory;
pub(crate) mod memo;
pub(crate) mod metadata;
pub mod observer;
pub(crate) mod permit;
pub(crate) mod policy;
pub(crate) mod sigs;
pub(crate) mod stablecoin;
pub(crate) mod storage;
pub(crate) mod token;

#[cfg(test)]
mod test_host;
#[cfg(test)]
mod tests;

pub use self::observer::{
    CallRecord, CallStats, CallStatus, Cas20Observer, MetricsObserver, NoopObserver,
};
use self::{
    ctx::{had_no_code, Ctx, Frame},
    errors::{complete, finish, finish_metered, rev, Cas20Err, Exit, Outcome, R},
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

/// 0xEF cannot be deployed (EIP-3541), so nothing can forge the marker, and it
/// keeps the account clear of EIP-161 reaping (BEP-702 3.16).
pub const MARKER_CODE: [u8; 1] = [0xEF];

/// type(uint128).max: the supply cap a token is created with.
pub(crate) const NO_SUPPLY_CAP: U256 = U256::from_limbs([u64::MAX, u64::MAX, 0, 0]);

pub fn marker_bytecode() -> Bytecode {
    Bytecode::new_raw(Bytes::from_static(&MARKER_CODE))
}

/// Whether `code_hash` is the account sentinel every initialized CAS20 account carries.
pub fn is_marker_code_hash(code_hash: B256) -> bool {
    code_hash == MARKER_CODE_HASH
}

/// byte[0:2] == 0xCA52 and byte[2:10] all zero.
pub fn is_cas20_address(addr: Address) -> bool {
    addr[0] == MARKER_PREFIX[0]
        && addr[1] == MARKER_PREFIX[1]
        && addr[2..10].iter().all(|&b| b == 0)
}

/// Whether dispatch resolves `addr` to native CAS20 code once Jenner is active:
/// the three singletons and the token space, minus variant ordinals no version
/// defines, which the EVM treats as ordinary accounts.
pub fn is_cas20_routed(addr: Address) -> bool {
    resolve(addr).is_some()
}

pub(crate) fn variant_recognized(variant: u8) -> bool {
    variant_feature(variant).is_some()
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
pub enum Kind {
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

/// The behaviour of the family under a fork. A later fork that changes an entry
/// point adds a variant here and branches on `frame.version`, leaving the earlier
/// behaviour frozen for the blocks that ran under it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Cas20Version {
    /// Jenner: the family as BEP-702 first shipped it.
    V1,
}

impl Cas20Version {
    /// The version in force under `spec`, none before the family exists.
    pub fn from_spec(spec: BscHardfork) -> Option<Self> {
        (spec >= BscHardfork::Jenner).then_some(Self::V1)
    }
}

/// Leaves fork gating to the caller.
fn resolve(addr: Address) -> Option<Kind> {
    match addr {
        FACTORY_ADDRESS => return Some(Kind::Factory),
        POLICY_REGISTRY_ADDRESS => return Some(Kind::Policy),
        ACTIVATION_REGISTRY_ADDRESS => return Some(Kind::Activation),
        _ => {}
    }
    if !is_cas20_address(addr) {
        return None;
    }
    match addr[10] {
        VARIANT_ASSET => Some(Kind::Asset),
        VARIANT_STABLECOIN => Some(Kind::Stablecoin),
        _ => None,
    }
}

/// The dynamic lookup a Jenner EVM installs in its precompile map: CAS20 tokens
/// have no fixed address, so they cannot live in the static tables and are
/// resolved from the address prefix instead. The version is settled once, when the
/// lookup is built for a spec; the five precompiles are shared, stateful (so the
/// engine never caches a result) and side-effect free to look up, since the map
/// also consults the lookup for `contains`.
#[derive(Clone, Debug)]
pub struct Cas20Lookup {
    active: Option<Arc<[DynPrecompile; 5]>>,
}

impl Cas20Lookup {
    /// The production lookup for `spec`: metrics on, precompiles shared process-wide.
    pub fn new(spec: BscHardfork) -> Self {
        static V1: LazyLock<Arc<[DynPrecompile; 5]>> =
            LazyLock::new(|| Arc::new(precompiles(Cas20Version::V1, MetricsObserver)));
        Self {
            active: Cas20Version::from_spec(spec).map(|version| match version {
                Cas20Version::V1 => V1.clone(),
            }),
        }
    }

    /// A lookup for `spec` reporting to `observer`.
    pub fn with_observer<O: Cas20Observer>(spec: BscHardfork, observer: O) -> Self {
        Self {
            active: Cas20Version::from_spec(spec)
                .map(|version| Arc::new(precompiles(version, observer))),
        }
    }

    /// Whether the family is routed at all under this lookup's spec.
    pub fn is_active(&self) -> bool {
        self.active.is_some()
    }
}

impl PrecompileLookup for Cas20Lookup {
    fn lookup(&self, address: &Address) -> Option<DynPrecompile> {
        let entries = self.active.as_ref()?;
        Some(entries[resolve(*address)? as usize].clone())
    }
}

fn precompiles<O: Cas20Observer>(version: Cas20Version, observer: O) -> [DynPrecompile; 5] {
    Kind::ALL.map(|kind| {
        let observer = observer.clone();
        DynPrecompile::new_stateful(kind.id(), move |input| run(version, kind, &observer, input))
    })
}

fn run<O: Cas20Observer>(
    version: Cas20Version,
    kind: Kind,
    observer: &O,
    input: PrecompileInput<'_>,
) -> PrecompileResult {
    let started = O::ENABLED.then(Instant::now);
    let direct_call = input.is_direct_call();
    let (data, gas_limit, reservoir) = (input.data, input.gas, input.reservoir);
    let mut internals = input.internals;
    let mut frame = Frame::new(&mut internals, gas_limit, version);
    let mut ctx = Ctx {
        frame: &mut frame,
        self_addr: input.bytecode_address,
        caller: input.caller,
        read_only: input.is_static,
        direct_call,
        value: input.value,
        admin_renounced: false,
    };
    let exit = execute(kind, &mut ctx, data);
    let (outcome, used, refund) = complete(&mut frame, exit);
    if O::ENABLED {
        // Told after the fact, from data already settled: nothing here can reach
        // gas, state or the result.
        observer.record_call(&CallRecord {
            kind,
            selector: match data.get(..4) {
                Some(sel) => sigs::selector_name(sel.try_into().unwrap()),
                None => "short",
            },
            status: outcome.status(),
            gas_used: used,
            elapsed: started.map(|t| t.elapsed()),
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
fn execute(kind: Kind, ctx: &mut Ctx<'_, '_>, data: &[u8]) -> Exit {
    match kind {
        Kind::Factory => run_singleton(ctx, data, factory::run_factory),
        Kind::Policy => run_singleton(ctx, data, policy::run_policy),
        Kind::Activation => run_singleton(ctx, data, activation::run_activation),
        Kind::Asset => run_token(ctx, data, |tok, input| asset::asset_dispatch(tok, input)),
        Kind::Stablecoin => {
            run_token(ctx, data, |tok, input| stablecoin::stablecoin_dispatch(tok, input))
        }
    }
}

/// The prologue every CAS20 entry point runs.
fn enter_call(ctx: &mut Ctx<'_, '_>, input: &[u8]) -> R<()> {
    if !ctx.direct_call {
        return Err(Cas20Err::DelegateCall);
    }
    if !ctx.value.is_zero() {
        return Err(rev(ERR_NON_PAYABLE, &[]));
    }
    if !ctx.charge_calldata(input) {
        return Err(Cas20Err::OutOfGas);
    }
    Ok(())
}

fn run_singleton(
    ctx: &mut Ctx<'_, '_>,
    input: &[u8],
    handler: fn(&mut Ctx<'_, '_>, &[u8]) -> R<Vec<u8>>,
) -> Exit {
    if let Err(e) = enter_call(ctx, input) {
        return finish(Err(e));
    }
    let res = handler(ctx, input);
    finish_metered(ctx, res)
}

/// Stateless: the token is ctx.self_addr, so one dispatcher serves every address
/// of a variant.
fn run_token<'f, 'a>(
    ctx: &mut Ctx<'f, 'a>,
    input: &[u8],
    dispatch: fn(&mut Token<'_, 'a>, &[u8]) -> R<Vec<u8>>,
) -> Exit {
    if let Err(e) = enter_call(ctx, input) {
        return finish(Err(e));
    }
    if !initialized_metered(ctx, ctx.self_addr) {
        let res = Err(errors::revert());
        return finish_metered(ctx, res);
    }
    let decimals = if ctx.self_addr[10] == VARIANT_STABLECOIN { 6 } else { 0 };
    let res = dispatch(&mut Token::new(ctx.reborrow(), decimals), input);
    finish_metered(ctx, res)
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
