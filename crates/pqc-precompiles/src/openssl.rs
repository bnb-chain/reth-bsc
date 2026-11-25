//! OpenSSL default-provider backend. This is not a FIPS-certified build.

use super::*;
use revm_precompile::PrecompileError;
use std::{
    ffi::{c_char, c_int, c_uint, c_void, CStr},
    ptr::NonNull,
    sync::OnceLock,
};

unsafe extern "C" {
    fn bsc_pqc_openssl_init() -> *mut c_void;
    fn bsc_pqc_openssl_verify(
        context: *const c_void,
        algorithm: c_uint,
        message: *const u8,
        message_len: usize,
        signature: *const u8,
        signature_len: usize,
        key: *const u8,
        key_len: usize,
    ) -> c_int;
    fn bsc_pqc_openssl_version() -> *const c_char;
}

struct Context(NonNull<c_void>);
// SAFETY: The C context is immutable after initialization and OpenSSL's library
// context/provider/fetched algorithms support concurrent use. Mutable EVP key
// and verification contexts are allocated separately for every call.
unsafe impl Send for Context {}
// SAFETY: Same invariant as Send; the build requires OPENSSL_THREADS.
unsafe impl Sync for Context {}

// The one shared provider context deliberately lives for the process lifetime.
// It cannot be freed while another verification thread is using it.
static CONTEXT: OnceLock<Result<Context, &'static str>> = OnceLock::new();

fn context() -> Result<&'static Context, &'static str> {
    CONTEXT
        .get_or_init(|| {
            // SAFETY: No arguments; C returns an owned, fully initialized context
            // or null after cleaning up all partial allocations.
            NonNull::new(unsafe { bsc_pqc_openssl_init() })
                .map(Context)
                .ok_or("OpenSSL default provider must support ML-DSA-44 and SLH-DSA-SHA2-128s")
        })
        .as_ref()
        .map_err(|error| *error)
}

/// Check provider/algorithm availability before installing a local registry.
pub fn initialize() -> Result<(), &'static str> {
    context().map(|_| ())
}

/// Runtime libcrypto version, useful for reproducible benchmark reports.
pub fn version() -> &'static str {
    // SAFETY: OpenSSL_version returns a non-null, static NUL-terminated string.
    unsafe { CStr::from_ptr(bsc_pqc_openssl_version()) }.to_str().unwrap_or("unknown OpenSSL")
}

fn verify(
    algorithm: c_uint,
    message: &[u8],
    signature: &[u8],
    key: &[u8],
) -> Result<bool, PrecompileError> {
    let context = context().map_err(|error| PrecompileError::Fatal(error.to_owned()))?;
    // SAFETY: Context has process lifetime. All byte slices are valid for their
    // supplied lengths, remain alive during the call, and are not retained by C.
    let result = unsafe {
        bsc_pqc_openssl_verify(
            context.0.as_ptr(),
            algorithm,
            message.as_ptr(),
            message.len(),
            signature.as_ptr(),
            signature.len(),
            key.as_ptr(),
            key.len(),
        )
    };
    match result {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err(PrecompileError::Fatal("OpenSSL PQ verification backend failed".to_owned())),
    }
}

/// Construct the OpenSSL ML-DSA-44 verifier at a caller-chosen address.
pub fn ml_dsa44(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("ML_DSA_44_VERIFY_EXPERIMENTAL")),
        address,
        ml_dsa44_run,
    )
}

/// Construct the OpenSSL SLH-DSA-SHA2-128s verifier at a caller-chosen address.
pub fn slh_dsa_sha2_128s(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("SLH_DSA_SHA2_128S_VERIFY_EXPERIMENTAL")),
        address,
        slh_dsa_sha2_128s_run,
    )
}

/// OpenSSL verification with the same input, gas, and output rules as RustCrypto.
pub fn ml_dsa44_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        ML_DSA44_GAS,
        ML_DSA44_SIGNATURE_LEN,
        ML_DSA44_PUBLIC_KEY_LEN,
        |message, signature, key| verify(0, message, signature, key),
    )
}

/// OpenSSL verification with the same input, gas, and output rules as RustCrypto.
pub fn slh_dsa_sha2_128s_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        SLH_DSA_SHA2_128S_GAS,
        SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        SLH_DSA_SHA2_128S_PUBLIC_KEY_LEN,
        |message, signature, key| verify(1, message, signature, key),
    )
}
