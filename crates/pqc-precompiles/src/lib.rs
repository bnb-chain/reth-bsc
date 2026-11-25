//! Experimental ML-DSA-44 and SLH-DSA-SHA2-128s verification precompiles.
//!
//! Wire format: `message[32] || signature || public_key`, with exact lengths.
//! The message is verified using the FIPS pure scheme with an empty context; a
//! 32-byte application digest is still a message, not the FIPS prehash variant.
//! Success returns a 32-byte ABI boolean. Invalid signatures/encodings return
//! false. Incorrect input lengths revert. Gas is checked before parsing.
//!
//! Addresses, encoding, and gas prices are experimental. No BSC hardfork registry
//! installs these precompiles. See `docs/pqc-precompiles.md` in the repository.

use ml_dsa::{MlDsa44, Verifier};
use revm_precompile::{
    primitives::{Address, Bytes},
    Precompile, PrecompileHalt, PrecompileId, PrecompileOutput, PrecompileResult, Precompiles,
};
use std::borrow::Cow;

/// Optional OpenSSL 3.5+ implementation of the same experimental interface.
#[cfg(feature = "openssl-backend")]
pub mod openssl;

/// Optional CIRCL implementation compiled with Go's C archive interface.
#[cfg(feature = "circl-backend")]
pub mod circl;

/// Explicit backend choice for a local registry. No choice activates a network.
#[derive(Clone, Copy, Debug)]
pub enum Backend {
    /// Pure Rust implementation (the original default).
    RustCrypto,
    /// Native OpenSSL via a small C FFI adapter.
    #[cfg(feature = "openssl-backend")]
    OpenSsl,
    /// Cloudflare CIRCL via a Go C archive, including the Go runtime.
    #[cfg(feature = "circl-backend")]
    Circl,
    /// CIRCL with a bounded, per-thread Rust cache of decoded public keys.
    #[cfg(feature = "circl-backend")]
    CirclCached,
}

/// Size of the application message (typically a user-operation hash).
pub const MESSAGE_LEN: usize = 32;
/// FIPS 204 ML-DSA-44 public-key size.
pub const ML_DSA44_PUBLIC_KEY_LEN: usize = 1312;
/// FIPS 204 ML-DSA-44 signature size.
pub const ML_DSA44_SIGNATURE_LEN: usize = 2420;
/// FIPS 205 SLH-DSA-SHA2-128s public-key size.
pub const SLH_DSA_SHA2_128S_PUBLIC_KEY_LEN: usize = 32;
/// FIPS 205 SLH-DSA-SHA2-128s signature size.
pub const SLH_DSA_SHA2_128S_SIGNATURE_LEN: usize = 7856;
/// Experimental verification price from the exploratory report.
pub const ML_DSA44_GAS: u64 = 10_000;
/// Experimental verification price from the exploratory report.
pub const SLH_DSA_SHA2_128S_GAS: u64 = 60_000;

/// Construct the experimental ML-DSA-44 precompile at a caller-chosen address.
pub fn ml_dsa44(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("ML_DSA_44_VERIFY_EXPERIMENTAL")),
        address,
        ml_dsa44_run,
    )
}

/// Construct the experimental SLH-DSA-SHA2-128s precompile at a caller-chosen address.
pub fn slh_dsa_sha2_128s(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("SLH_DSA_SHA2_128S_VERIFY_EXPERIMENTAL")),
        address,
        slh_dsa_sha2_128s_run,
    )
}

/// Add both precompiles to an explicitly selected local registry.
///
/// Rejects duplicate/occupied addresses before making any changes. Callers must
/// also ensure these addresses do not contain contracts in their local state.
pub fn register(
    registry: &mut Precompiles,
    ml_address: Address,
    slh_address: Address,
) -> Result<(), &'static str> {
    register_with_backend(registry, ml_address, slh_address, Backend::RustCrypto)
}

/// Register both verifiers using an explicit backend, checking availability first.
pub fn register_with_backend(
    registry: &mut Precompiles,
    ml_address: Address,
    slh_address: Address,
    backend: Backend,
) -> Result<(), &'static str> {
    if ml_address == slh_address {
        return Err("PQ precompile addresses must be distinct");
    }
    if registry.get(&ml_address).is_some() || registry.get(&slh_address).is_some() {
        return Err("PQ precompile address is already registered");
    }
    let precompiles = match backend {
        Backend::RustCrypto => [ml_dsa44(ml_address), slh_dsa_sha2_128s(slh_address)],
        #[cfg(feature = "openssl-backend")]
        Backend::OpenSsl => {
            openssl::initialize()?;
            [openssl::ml_dsa44(ml_address), openssl::slh_dsa_sha2_128s(slh_address)]
        }
        #[cfg(feature = "circl-backend")]
        Backend::Circl => [circl::ml_dsa44(ml_address), circl::slh_dsa_sha2_128s(slh_address)],
        #[cfg(feature = "circl-backend")]
        Backend::CirclCached => {
            [circl::ml_dsa44_cached(ml_address), circl::slh_dsa_sha2_128s_cached(slh_address)]
        }
    };
    registry.extend(precompiles);
    Ok(())
}

type VerifyFn = fn(&[u8], &[u8], &[u8]) -> Result<bool, revm_precompile::PrecompileError>;

fn run(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
    cost: u64,
    signature_len: usize,
    key_len: usize,
    verify: VerifyFn,
) -> PrecompileResult {
    if gas_limit < cost {
        return Ok(PrecompileOutput::halt(PrecompileHalt::OutOfGas, reservoir));
    }
    if input.len() != MESSAGE_LEN + signature_len + key_len {
        return Ok(PrecompileOutput::revert(cost, Bytes::new(), reservoir));
    }
    let (message, remaining) = input.split_at(MESSAGE_LEN);
    let (signature, key) = remaining.split_at(signature_len);
    let mut result = [0; 32];
    result[31] = u8::from(verify(message, signature, key)?);
    Ok(PrecompileOutput::new(cost, Bytes::copy_from_slice(&result), reservoir))
}

/// Verify an ML-DSA-44 input using the provisional wire format and gas rules.
pub fn ml_dsa44_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        ML_DSA44_GAS,
        ML_DSA44_SIGNATURE_LEN,
        ML_DSA44_PUBLIC_KEY_LEN,
        |message, signature, key| {
            let Ok(encoded_key) = key.try_into() else { return Ok(false) };
            let key = ml_dsa::VerifyingKey::<MlDsa44>::decode(encoded_key);
            let Ok(signature) = ml_dsa::Signature::<MlDsa44>::try_from(signature) else {
                return Ok(false);
            };
            Ok(key.verify(message, &signature).is_ok())
        },
    )
}

/// Verify an SLH-DSA-SHA2-128s input using the provisional wire format and gas rules.
pub fn slh_dsa_sha2_128s_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        SLH_DSA_SHA2_128S_GAS,
        SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        SLH_DSA_SHA2_128S_PUBLIC_KEY_LEN,
        |message, signature, key| {
            let Ok(key) = slh_dsa::VerifyingKey::<slh_dsa::Sha2_128s>::try_from(key) else {
                return Ok(false);
            };
            let Ok(signature) = slh_dsa::Signature::<slh_dsa::Sha2_128s>::try_from(signature)
            else {
                return Ok(false);
            };
            Ok(key.try_verify_with_context(message, &[], &signature).is_ok())
        },
    )
}
