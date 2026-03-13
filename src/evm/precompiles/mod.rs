#![allow(unused)]

use crate::hardforks::bsc::BscHardfork;
use alloy_primitives::{hex, Address as AlloyAddress, B256};
use cfg_if::cfg_if;
use once_cell::race::OnceBox;
use revm::{
    context::Cfg,
    handler::EthPrecompiles,
    precompile::{
        bls12_381, kzg_point_evaluation, modexp, secp256r1, u64_to_address, Precompile, PrecompileError, PrecompileFn,
        PrecompileId, PrecompileOutput, PrecompileResult, Precompiles,
    },
    primitives::{hardfork::SpecId, Address as RevmAddress},
};
use std::{boxed::Box, cell::RefCell, collections::HashMap};

mod bls;
mod cometbft;
mod double_sign;
mod error;
mod iavl;
mod tendermint;
mod tm_secp256k1;

// --- Precompile tracing -----------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PrecompileTraceEntry {
    id: PrecompileId,
    original: PrecompileFn,
}

#[derive(Clone, Debug, Default)]
struct PrecompileTraceMap {
    inner: HashMap<RevmAddress, PrecompileTraceEntry>,
}

impl PrecompileTraceMap {
    fn insert(&mut self, address: RevmAddress, entry: PrecompileTraceEntry) {
        self.inner.insert(address, entry);
    }

    fn get(&self, address: &RevmAddress) -> Option<PrecompileTraceEntry> {
        self.inner.get(address).cloned()
    }
}

#[derive(Clone, Debug)]
struct TracedPrecompiles {
    precompiles: Precompiles,
    trace_map: PrecompileTraceMap,
}

impl TracedPrecompiles {
    #[inline]
    fn precompiles(&self) -> &Precompiles {
        &self.precompiles
    }

    #[inline]
    fn trace_map(&self) -> &PrecompileTraceMap {
        &self.trace_map
    }
}

/// Per-transaction context used by precompile tracing.
#[derive(Clone, Debug)]
pub(crate) struct PrecompileTraceContext {
    pub(crate) block_number: u64,
    pub(crate) spec: BscHardfork,
    trace_map: &'static PrecompileTraceMap,
    pub(crate) is_system_tx: bool,
    pub(crate) tx_hash: Option<B256>,
    pub(crate) tx_to: Option<AlloyAddress>,
    pub(crate) tx_selector: Option<String>,
    pub(crate) tx_input_len: usize,
}

impl PrecompileTraceContext {
    /// Construct a new trace context from the block/spec and tx envelope details.
    pub(crate) fn new(
        block_number: u64,
        spec: BscHardfork,
        is_system_tx: bool,
        tx_hash: Option<B256>,
        tx_to: Option<AlloyAddress>,
        tx_input: &[u8],
    ) -> Self {
        let tx_selector = if tx_input.len() >= 4 { Some(hex::encode(&tx_input[..4])) } else { None };

        Self::from_parts(block_number, spec, is_system_tx, tx_hash, tx_to, tx_selector, tx_input.len())
    }

    /// Construct a new trace context from derived tx parts (avoids cloning tx input).
    pub(crate) fn from_parts(
        block_number: u64,
        spec: BscHardfork,
        is_system_tx: bool,
        tx_hash: Option<B256>,
        tx_to: Option<AlloyAddress>,
        tx_selector: Option<String>,
        tx_input_len: usize,
    ) -> Self {
        Self {
            block_number,
            spec,
            trace_map: trace_map_for_spec(spec),
            is_system_tx,
            tx_hash,
            tx_to,
            tx_selector,
            tx_input_len,
        }
    }

    /// Merge missing "outer" fields (like tx hash) from the parent context.
    fn merge_with_parent(mut self, parent: Option<&PrecompileTraceContext>) -> Self {
        if let Some(parent) = parent {
            if self.tx_hash.is_none() {
                self.tx_hash = parent.tx_hash;
            }
            if self.tx_to.is_none() {
                self.tx_to = parent.tx_to;
            }
            if self.tx_selector.is_none() {
                self.tx_selector = parent.tx_selector.clone();
            }
            if self.tx_input_len == 0 {
                self.tx_input_len = parent.tx_input_len;
            }
            if !self.is_system_tx {
                self.is_system_tx = parent.is_system_tx;
            }
        }
        self
    }
}

thread_local! {
    static PRECOMPILE_TRACE_STACK: RefCell<Vec<PrecompileTraceContext>> = const { RefCell::new(Vec::new()) };
}

/// Push a new precompile trace context for the current thread.
pub(crate) fn push_precompile_trace_context(ctx: PrecompileTraceContext) {
    PRECOMPILE_TRACE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        let merged = ctx.merge_with_parent(stack.last());
        stack.push(merged);
    });
}

/// Pop the most recent precompile trace context for the current thread.
pub(crate) fn pop_precompile_trace_context() {
    PRECOMPILE_TRACE_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        if stack.pop().is_none() {
            tracing::warn!(
                target: "bsc::precompile_gas",
                "Attempted to pop a precompile trace context but the stack was empty"
            );
        }
    });
}

fn current_precompile_trace_context() -> Option<PrecompileTraceContext> {
    PRECOMPILE_TRACE_STACK.with(|stack| stack.borrow().last().cloned())
}

#[inline]
fn should_trace_precompiles(ctx: &PrecompileTraceContext) -> bool {
    ctx.block_number <= 2
}

fn log_precompile_call(
    ctx: &PrecompileTraceContext,
    address: RevmAddress,
    id: PrecompileId,
    gas_limit: u64,
    input_len: usize,
    result: &PrecompileResult,
) {
    let spec_id = SpecId::from(ctx.spec);

    match result {
        Ok(output) => {
            tracing::warn!(
                target: "bsc::precompile_gas",
                block_number = ctx.block_number,
                spec = ?ctx.spec,
                spec_id = ?spec_id,
                is_system_tx = ctx.is_system_tx,
                tx_hash = ?ctx.tx_hash,
                tx_to = ?ctx.tx_to,
                tx_selector = ctx.tx_selector.as_deref().unwrap_or(""),
                tx_input_len = ctx.tx_input_len,
                precompile_address = ?address,
                precompile_id = ?id,
                precompile_gas_limit = gas_limit,
                precompile_input_len = input_len,
                precompile_gas_used = output.gas_used,
                precompile_gas_refunded = output.gas_refunded,
                precompile_reverted = output.reverted,
                precompile_output_len = output.bytes.len(),
                "Precompile executed"
            );
        }
        Err(err) => {
            tracing::warn!(
                target: "bsc::precompile_gas",
                block_number = ctx.block_number,
                spec = ?ctx.spec,
                spec_id = ?spec_id,
                is_system_tx = ctx.is_system_tx,
                tx_hash = ?ctx.tx_hash,
                tx_to = ?ctx.tx_to,
                tx_selector = ctx.tx_selector.as_deref().unwrap_or(""),
                tx_input_len = ctx.tx_input_len,
                precompile_address = ?address,
                precompile_id = ?id,
                precompile_gas_limit = gas_limit,
                precompile_input_len = input_len,
                error = ?err,
                "Precompile failed"
            );
        }
    }
}

fn traced_precompile_call(address: RevmAddress, input: &[u8], gas_limit: u64) -> PrecompileResult {
    let ctx = current_precompile_trace_context();

    let entry = if let Some(ctx) = &ctx {
        ctx.trace_map.get(&address)
    } else {
        trace_map_for_spec(BscHardfork::default()).get(&address)
    };

    let Some(entry) = entry else {
        return Err(PrecompileError::other("missing original precompile function"));
    };

    let result = (entry.original)(input, gas_limit);

    if let Some(ctx) = &ctx {
        if should_trace_precompiles(ctx) {
            log_precompile_call(ctx, address, entry.id, gas_limit, input.len(), &result);
        }
    }

    result
}

// Wrapper functions must be plain function pointers, so we define one per address.
const PRECOMPILE_ADDR_1: RevmAddress = u64_to_address(1);
const PRECOMPILE_ADDR_2: RevmAddress = u64_to_address(2);
const PRECOMPILE_ADDR_3: RevmAddress = u64_to_address(3);
const PRECOMPILE_ADDR_4: RevmAddress = u64_to_address(4);
const PRECOMPILE_ADDR_5: RevmAddress = u64_to_address(5);
const PRECOMPILE_ADDR_6: RevmAddress = u64_to_address(6);
const PRECOMPILE_ADDR_7: RevmAddress = u64_to_address(7);
const PRECOMPILE_ADDR_8: RevmAddress = u64_to_address(8);
const PRECOMPILE_ADDR_9: RevmAddress = u64_to_address(9);
const PRECOMPILE_ADDR_10: RevmAddress = u64_to_address(10);

const PRECOMPILE_ADDR_100: RevmAddress = u64_to_address(100);
const PRECOMPILE_ADDR_101: RevmAddress = u64_to_address(101);
const PRECOMPILE_ADDR_102: RevmAddress = u64_to_address(102);
const PRECOMPILE_ADDR_103: RevmAddress = u64_to_address(103);
const PRECOMPILE_ADDR_104: RevmAddress = u64_to_address(104);
const PRECOMPILE_ADDR_105: RevmAddress = u64_to_address(105);

macro_rules! traced_wrapper {
    ($name:ident, $addr:ident) => {
        fn $name(input: &[u8], gas_limit: u64) -> PrecompileResult {
            traced_precompile_call($addr, input, gas_limit)
        }
    };
}

traced_wrapper!(traced_precompile_1, PRECOMPILE_ADDR_1);
traced_wrapper!(traced_precompile_2, PRECOMPILE_ADDR_2);
traced_wrapper!(traced_precompile_3, PRECOMPILE_ADDR_3);
traced_wrapper!(traced_precompile_4, PRECOMPILE_ADDR_4);
traced_wrapper!(traced_precompile_5, PRECOMPILE_ADDR_5);
traced_wrapper!(traced_precompile_6, PRECOMPILE_ADDR_6);
traced_wrapper!(traced_precompile_7, PRECOMPILE_ADDR_7);
traced_wrapper!(traced_precompile_8, PRECOMPILE_ADDR_8);
traced_wrapper!(traced_precompile_9, PRECOMPILE_ADDR_9);
traced_wrapper!(traced_precompile_10, PRECOMPILE_ADDR_10);

traced_wrapper!(traced_precompile_100, PRECOMPILE_ADDR_100);
traced_wrapper!(traced_precompile_101, PRECOMPILE_ADDR_101);
traced_wrapper!(traced_precompile_102, PRECOMPILE_ADDR_102);
traced_wrapper!(traced_precompile_103, PRECOMPILE_ADDR_103);
traced_wrapper!(traced_precompile_104, PRECOMPILE_ADDR_104);
traced_wrapper!(traced_precompile_105, PRECOMPILE_ADDR_105);

fn traced_wrapper_for_address(address: RevmAddress) -> Option<PrecompileFn> {
    match address {
        a if a == PRECOMPILE_ADDR_1 => Some(traced_precompile_1),
        a if a == PRECOMPILE_ADDR_2 => Some(traced_precompile_2),
        a if a == PRECOMPILE_ADDR_3 => Some(traced_precompile_3),
        a if a == PRECOMPILE_ADDR_4 => Some(traced_precompile_4),
        a if a == PRECOMPILE_ADDR_5 => Some(traced_precompile_5),
        a if a == PRECOMPILE_ADDR_6 => Some(traced_precompile_6),
        a if a == PRECOMPILE_ADDR_7 => Some(traced_precompile_7),
        a if a == PRECOMPILE_ADDR_8 => Some(traced_precompile_8),
        a if a == PRECOMPILE_ADDR_9 => Some(traced_precompile_9),
        a if a == PRECOMPILE_ADDR_10 => Some(traced_precompile_10),
        a if a == PRECOMPILE_ADDR_100 => Some(traced_precompile_100),
        a if a == PRECOMPILE_ADDR_101 => Some(traced_precompile_101),
        a if a == PRECOMPILE_ADDR_102 => Some(traced_precompile_102),
        a if a == PRECOMPILE_ADDR_103 => Some(traced_precompile_103),
        a if a == PRECOMPILE_ADDR_104 => Some(traced_precompile_104),
        a if a == PRECOMPILE_ADDR_105 => Some(traced_precompile_105),
        _ => None,
    }
}

fn install_precompile_tracing(precompiles: &mut Precompiles) -> PrecompileTraceMap {
    let mut trace_map = PrecompileTraceMap::default();
    let addresses: Vec<_> = precompiles.addresses_set().iter().copied().collect();

    for address in addresses {
        let Some(precompile) = precompiles.get(&address).cloned() else {
            continue;
        };

        let (id, address, original) = precompile.into();
        trace_map.insert(address, PrecompileTraceEntry { id: id.clone(), original });

        if let Some(wrapper) = traced_wrapper_for_address(address) {
            precompiles.extend([Precompile::new(id, address, wrapper)]);
        }
    }

    trace_map
}

fn build_traced_precompiles(mut precompiles: Precompiles) -> TracedPrecompiles {
    let trace_map = install_precompile_tracing(&mut precompiles);
    TracedPrecompiles { precompiles, trace_map }
}

// --- Unwrapped precompile builders --------------------------------------------------------------

fn build_istanbul_precompiles() -> Precompiles {
    let mut precompiles = Precompiles::istanbul().clone();
    precompiles.extend([tendermint::TENDERMINT_HEADER_VALIDATION, iavl::IAVL_PROOF_VALIDATION]);
    precompiles
}

fn build_nano_precompiles() -> Precompiles {
    let mut precompiles = build_istanbul_precompiles();
    precompiles.extend([tendermint::TENDERMINT_HEADER_VALIDATION_NANO, iavl::IAVL_PROOF_VALIDATION_NANO]);
    precompiles
}

fn build_moran_precompiles() -> Precompiles {
    let mut precompiles = build_istanbul_precompiles();
    precompiles.extend([tendermint::TENDERMINT_HEADER_VALIDATION, iavl::IAVL_PROOF_VALIDATION_MORAN]);
    precompiles
}

fn build_planck_precompiles() -> Precompiles {
    let mut precompiles = build_istanbul_precompiles();
    precompiles.extend([tendermint::TENDERMINT_HEADER_VALIDATION, iavl::IAVL_PROOF_VALIDATION_PLANCK]);
    precompiles
}

fn build_luban_precompiles() -> Precompiles {
    let mut precompiles = build_planck_precompiles();
    precompiles.extend([bls::BLS_SIGNATURE_VALIDATION, cometbft::COMETBFT_LIGHT_BLOCK_VALIDATION_BEFORE_HERTZ]);
    precompiles
}

fn build_plato_precompiles() -> Precompiles {
    let mut precompiles = build_luban_precompiles();
    precompiles.extend([iavl::IAVL_PROOF_VALIDATION_PLATO]);
    precompiles
}

fn build_hertz_precompiles() -> Precompiles {
    let mut precompiles = build_plato_precompiles();
    precompiles.extend([cometbft::COMETBFT_LIGHT_BLOCK_VALIDATION, modexp::BERLIN]);
    precompiles
}

fn build_feynman_precompiles() -> Precompiles {
    let mut precompiles = build_hertz_precompiles();
    precompiles.extend([double_sign::DOUBLE_SIGN_EVIDENCE_VALIDATION, tm_secp256k1::TM_SECP256K1_SIGNATURE_RECOVER]);
    precompiles
}

fn build_cancun_precompiles() -> Precompiles {
    let mut precompiles = build_feynman_precompiles();
    precompiles.extend([kzg_point_evaluation::POINT_EVALUATION]);
    precompiles
}

fn build_haber_precompiles() -> Precompiles {
    let mut precompiles = build_cancun_precompiles();
    precompiles.extend([secp256r1::P256VERIFY]);
    precompiles
}

fn build_pascal_precompiles() -> Precompiles {
    let mut precompiles = build_haber_precompiles();
    precompiles.extend(bls12_381::precompiles());
    precompiles
}

fn build_mendel_precompiles() -> Precompiles {
    let mut precompiles = build_pascal_precompiles();
    // EIP-7823 (MODEXP bounds) + EIP-7883 (MODEXP gas cost increase)
    precompiles.extend([modexp::OSAKA]);
    // EIP-7951/BEP-659: P256VERIFY gas cost increase (3450 -> 6900)
    precompiles.extend([secp256r1::P256VERIFY_OSAKA]);
    precompiles
}

// --- Traced precompile singletons ---------------------------------------------------------------

fn istanbul_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_istanbul_precompiles())))
}

fn nano_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_nano_precompiles())))
}

fn moran_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_moran_precompiles())))
}

fn planck_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_planck_precompiles())))
}

fn luban_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_luban_precompiles())))
}

fn plato_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_plato_precompiles())))
}

fn hertz_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_hertz_precompiles())))
}

fn feynman_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_feynman_precompiles())))
}

fn cancun_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_cancun_precompiles())))
}

fn haber_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_haber_precompiles())))
}

fn pascal_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_pascal_precompiles())))
}

fn mendel_traced() -> &'static TracedPrecompiles {
    static INSTANCE: OnceBox<TracedPrecompiles> = OnceBox::new();
    INSTANCE.get_or_init(|| Box::new(build_traced_precompiles(build_mendel_precompiles())))
}

fn traced_precompiles_for_spec(spec: BscHardfork) -> &'static TracedPrecompiles {
    // Osaka uses updated precompiles (EIP-7823/7883 MODEXP, EIP-7951 P256VERIFY)
    if spec >= BscHardfork::Mendel {
        mendel_traced()
    } else if spec >= BscHardfork::Pascal {
        pascal_traced()
    } else if spec >= BscHardfork::Haber {
        haber_traced()
    } else if spec >= BscHardfork::Cancun {
        cancun_traced()
    } else if spec >= BscHardfork::Feynman {
        feynman_traced()
    } else if spec >= BscHardfork::Hertz {
        hertz_traced()
    } else if spec >= BscHardfork::Plato {
        plato_traced()
    } else if spec >= BscHardfork::Luban {
        luban_traced()
    } else if spec >= BscHardfork::Planck {
        planck_traced()
    } else if spec >= BscHardfork::Moran {
        moran_traced()
    } else if spec >= BscHardfork::Nano {
        nano_traced()
    } else {
        istanbul_traced()
    }
}

fn trace_map_for_spec(spec: BscHardfork) -> &'static PrecompileTraceMap {
    traced_precompiles_for_spec(spec).trace_map()
}

// --- Public precompile providers ----------------------------------------------------------------

/// Returns precompiles for Istanbul spec.
pub fn istanbul() -> &'static Precompiles {
    istanbul_traced().precompiles()
}

/// Returns precompiles for Nano spec.
pub fn nano() -> &'static Precompiles {
    nano_traced().precompiles()
}

/// Returns precompiles for Moran sepc.
pub fn moran() -> &'static Precompiles {
    moran_traced().precompiles()
}

/// Returns precompiles for Planck sepc.
pub fn planck() -> &'static Precompiles {
    planck_traced().precompiles()
}

/// Returns precompiles for Luban sepc.
pub fn luban() -> &'static Precompiles {
    luban_traced().precompiles()
}

/// Returns precompiles for Plato sepc.
pub fn plato() -> &'static Precompiles {
    plato_traced().precompiles()
}

/// Returns precompiles for Hertz sepc.
pub fn hertz() -> &'static Precompiles {
    hertz_traced().precompiles()
}

/// Returns precompiles for Feynman sepc.
pub fn feynman() -> &'static Precompiles {
    feynman_traced().precompiles()
}

/// Returns precompiles for Cancun spec.
pub fn cancun() -> &'static Precompiles {
    cancun_traced().precompiles()
}

/// Returns precompiles for Haber spec.
pub fn haber() -> &'static Precompiles {
    haber_traced().precompiles()
}

/// Returns precompiles for Pascal spec.
pub fn pascal() -> &'static Precompiles {
    pascal_traced().precompiles()
}

/// Returns precompiles for Mendel spec.
/// Includes EIP-7823/7883 (MODEXP) and EIP-7951 (P256VERIFY gas increase).
pub fn mendel() -> &'static Precompiles {
    mendel_traced().precompiles()
}

// BSC precompile provider
#[derive(Debug, Clone)]
pub struct BscPrecompiles {
    /// Inner precompile provider is same as Ethereums.
    inner: EthPrecompiles,
}

impl BscPrecompiles {
    /// Create a new precompile provider with the given bsc spec.
    #[inline]
    pub fn new(spec: BscHardfork) -> Self {
        let precompiles = traced_precompiles_for_spec(spec).precompiles();
        Self { inner: EthPrecompiles { precompiles, spec: spec.into() } }
    }

    #[inline]
    pub fn precompiles(&self) -> &'static Precompiles {
        self.inner.precompiles
    }
}

impl Default for BscPrecompiles {
    fn default() -> Self {
        Self::new(BscHardfork::default())
    }
}
