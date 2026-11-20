//! CIRCL 1.6.5 backend through a Go C archive. Includes the Go runtime.

use super::*;
use revm_precompile::PrecompileError;
use std::{cell::RefCell, collections::VecDeque, ffi::c_int, num::NonZeroUsize};

unsafe extern "C" {
    fn bsc_pqc_circl_verify(
        algorithm: u32,
        message: *const u8,
        message_len: usize,
        signature: *const u8,
        signature_len: usize,
        key: *const u8,
        key_len: usize,
    ) -> c_int;
    fn bsc_pqc_circl_key_new(algorithm: u32, key: *const u8, key_len: usize) -> usize;
    fn bsc_pqc_circl_key_free(handle: usize) -> c_int;
    fn bsc_pqc_circl_verify_prepared(
        handle: usize,
        message: *const u8,
        message_len: usize,
        signature: *const u8,
        signature_len: usize,
    ) -> c_int;
}

/// Pinned CIRCL release and the Go compiler used to build the linked archive.
pub fn version() -> &'static str {
    concat!("CIRCL 1.6.5; ", env!("PQC_CIRCL_GO_VERSION"))
}

fn verify(
    algorithm: u32,
    message: &[u8],
    signature: &[u8],
    key: &[u8],
) -> Result<bool, PrecompileError> {
    // SAFETY: Slices remain live for the synchronous call. Go only reads these
    // buffers and retains no pointers. Each invocation has separate mutable state.
    let result = unsafe {
        bsc_pqc_circl_verify(
            algorithm,
            message.as_ptr(),
            message.len(),
            signature.as_ptr(),
            signature.len(),
            key.as_ptr(),
            key.len(),
        )
    };
    verification_result(result)
}

fn verification_result(result: c_int) -> Result<bool, PrecompileError> {
    match result {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err(PrecompileError::Fatal("CIRCL PQ verification backend failed".to_owned())),
    }
}

/// Maximum number of decoded keys retained by each Rust thread in cached mode.
pub const KEY_CACHE_CAPACITY: usize = 16;

struct PreparedKey(NonZeroUsize);

impl PreparedKey {
    fn new(algorithm: u32, key: &[u8]) -> Result<Self, PrecompileError> {
        // SAFETY: Go copies/decodes the live slice and returns an integer handle,
        // never a Go pointer. This owner is solely responsible for deleting it.
        NonZeroUsize::new(unsafe { bsc_pqc_circl_key_new(algorithm, key.as_ptr(), key.len()) })
            .map(Self)
            .ok_or_else(|| PrecompileError::Fatal("CIRCL public key import failed".to_owned()))
    }

    fn verify(&self, message: &[u8], signature: &[u8]) -> Result<bool, PrecompileError> {
        // SAFETY: The handle remains live for the call, and Go neither retains
        // nor writes to the borrowed message/signature buffers.
        verification_result(unsafe {
            bsc_pqc_circl_verify_prepared(
                self.0.get(),
                message.as_ptr(),
                message.len(),
                signature.as_ptr(),
                signature.len(),
            )
        })
    }
}

impl Drop for PreparedKey {
    fn drop(&mut self) {
        // SAFETY: This private, non-Clone owner deletes its handle exactly once,
        // with no verification in flight. Failure indicates an ownership bug.
        if unsafe { bsc_pqc_circl_key_free(self.0.get()) } != 1 {
            std::process::abort();
        }
    }
}

struct CacheEntry {
    algorithm: u32,
    encoded: Box<[u8]>,
    prepared: PreparedKey,
}

#[derive(Default)]
struct KeyCache(VecDeque<CacheEntry>);

impl KeyCache {
    fn verify(
        &mut self,
        algorithm: u32,
        message: &[u8],
        signature: &[u8],
        key: &[u8],
    ) -> Result<bool, PrecompileError> {
        // Full-byte equality avoids hash collisions. The bounded linear scan
        // needs no shared lock or attacker-controlled hash-table allocation.
        if let Some(index) = self
            .0
            .iter()
            .position(|entry| entry.algorithm == algorithm && entry.encoded.as_ref() == key)
        {
            let entry = self.0.remove(index).expect("index from the same cache");
            self.0.push_front(entry);
        } else {
            if self.0.len() == KEY_CACHE_CAPACITY {
                self.0.pop_back(); // Drop deletes the evicted Go handle.
            }
            let prepared = PreparedKey::new(algorithm, key)?;
            self.0.push_front(CacheEntry { algorithm, encoded: key.into(), prepared });
        }
        // Only decoded public keys are cached. Every message/signature is verified.
        self.0.front().expect("key was inserted").prepared.verify(message, signature)
    }
}

thread_local! {
    static KEY_CACHE: RefCell<KeyCache> = RefCell::default();
}

/// Release this thread's decoded keys, for worker cleanup or cold-cache benchmarks.
pub fn clear_thread_key_cache() {
    KEY_CACHE.with_borrow_mut(|cache| cache.0.clear());
}

fn cached_verify(
    algorithm: u32,
    message: &[u8],
    signature: &[u8],
    key: &[u8],
) -> Result<bool, PrecompileError> {
    KEY_CACHE.with_borrow_mut(|cache| cache.verify(algorithm, message, signature, key))
}

/// Construct the CIRCL ML-DSA-44 verifier with per-thread decoded-key caching.
pub fn ml_dsa44_cached(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("ML_DSA_44_VERIFY_EXPERIMENTAL")),
        address,
        ml_dsa44_cached_run,
    )
}

/// Construct the CIRCL SLH verifier with per-thread decoded-key caching.
pub fn slh_dsa_sha2_128s_cached(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("SLH_DSA_SHA2_128S_VERIFY_EXPERIMENTAL")),
        address,
        slh_dsa_sha2_128s_cached_run,
    )
}

/// Same verification and gas rules, retaining up to 16 decoded keys per thread.
pub fn ml_dsa44_cached_run(input: &[u8], gas_limit: u64, reservoir: u64) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        ML_DSA44_GAS,
        ML_DSA44_SIGNATURE_LEN,
        ML_DSA44_PUBLIC_KEY_LEN,
        |message, signature, key| cached_verify(0, message, signature, key),
    )
}

/// Same verification and gas rules, retaining up to 16 decoded keys per thread.
pub fn slh_dsa_sha2_128s_cached_run(
    input: &[u8],
    gas_limit: u64,
    reservoir: u64,
) -> PrecompileResult {
    run(
        input,
        gas_limit,
        reservoir,
        SLH_DSA_SHA2_128S_GAS,
        SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        SLH_DSA_SHA2_128S_PUBLIC_KEY_LEN,
        |message, signature, key| cached_verify(1, message, signature, key),
    )
}

/// Construct the CIRCL ML-DSA-44 verifier at a caller-chosen address.
pub fn ml_dsa44(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("ML_DSA_44_VERIFY_EXPERIMENTAL")),
        address,
        ml_dsa44_run,
    )
}

/// Construct the CIRCL SLH-DSA-SHA2-128s verifier at a caller-chosen address.
pub fn slh_dsa_sha2_128s(address: Address) -> Precompile {
    Precompile::new(
        PrecompileId::Custom(Cow::Borrowed("SLH_DSA_SHA2_128S_VERIFY_EXPERIMENTAL")),
        address,
        slh_dsa_sha2_128s_run,
    )
}

/// CIRCL verification with the shared experimental input, gas, and output rules.
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

/// CIRCL verification with the shared experimental input, gas, and output rules.
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

#[cfg(test)]
mod tests {
    use super::*;

    const ML: &[u8] = include_bytes!("../tests/fixtures/ML-DSA-44-valid.bin");
    const SLH: &[u8] = include_bytes!("../tests/fixtures/SLH-DSA-SHA2-128s-valid.bin");

    fn probe_handle(handle: usize) -> c_int {
        // SAFETY: The Go entry checks the handle then rejects these zero lengths
        // before reading pointers. A deleted handle returns -1 via panic recovery.
        unsafe { bsc_pqc_circl_verify_prepared(handle, std::ptr::null(), 0, std::ptr::null(), 0) }
    }

    #[test]
    fn prepared_keys_own_input_and_release_handles() {
        for (algorithm, fixture, sig_len) in
            [(0, ML, ML_DSA44_SIGNATURE_LEN), (1, SLH, SLH_DSA_SHA2_128S_SIGNATURE_LEN)]
        {
            let (message, tail) = fixture.split_at(MESSAGE_LEN);
            let (signature, key) = tail.split_at(sig_len);
            let mut transient_key = key.to_vec();
            let prepared = PreparedKey::new(algorithm, &transient_key).unwrap();
            transient_key.fill(0);
            drop(transient_key);
            assert!(prepared.verify(message, signature).unwrap());
            let mut changed = message.to_vec();
            changed[0] ^= 1;
            assert!(!prepared.verify(&changed, signature).unwrap());
            assert!(prepared.verify(message, signature).unwrap());
            let handle = prepared.0.get();
            assert_eq!(probe_handle(handle), 0);
            drop(prepared);
            assert_eq!(probe_handle(handle), -1);
        }
    }

    #[test]
    fn cache_is_bounded_and_evicts_least_recently_used_key() {
        for (algorithm, corpus, sig_len, input_len) in [
            (
                0,
                include_bytes!("../tests/fixtures/cache/ML-DSA-44-17-keys.bin").as_slice(),
                ML_DSA44_SIGNATURE_LEN,
                ML.len(),
            ),
            (
                1,
                include_bytes!("../tests/fixtures/cache/SLH-DSA-SHA2-128s-17-keys.bin").as_slice(),
                SLH_DSA_SHA2_128S_SIGNATURE_LEN,
                SLH.len(),
            ),
        ] {
            let inputs: Vec<_> = corpus.chunks_exact(input_len).collect();
            assert_eq!(inputs.len(), KEY_CACHE_CAPACITY + 1);
            let mut cache = KeyCache::default();
            let mut handles = Vec::new();
            for input in inputs.iter().take(KEY_CACHE_CAPACITY) {
                assert!(cache
                    .verify(
                        algorithm,
                        &input[..32],
                        &input[32..32 + sig_len],
                        &input[32 + sig_len..]
                    )
                    .unwrap());
                handles.push(cache.0.front().unwrap().prepared.0.get());
            }
            let first = inputs[0];
            assert!(cache
                .verify(algorithm, &first[..32], &first[32..32 + sig_len], &first[32 + sig_len..])
                .unwrap());
            assert_eq!(cache.0.front().unwrap().prepared.0.get(), handles[0]);
            let next = inputs[KEY_CACHE_CAPACITY];
            assert!(cache
                .verify(algorithm, &next[..32], &next[32..32 + sig_len], &next[32 + sig_len..])
                .unwrap());
            assert_eq!(cache.0.len(), KEY_CACHE_CAPACITY);
            assert_eq!(probe_handle(handles[0]), 0);
            assert_eq!(probe_handle(handles[1]), -1);
            let retained: Vec<_> = cache.0.iter().map(|e| e.prepared.0.get()).collect();
            drop(cache);
            for handle in retained {
                assert_eq!(probe_handle(handle), -1);
            }
        }
    }

    #[test]
    fn thread_exit_releases_its_cached_keys() {
        let handles = std::thread::spawn(|| {
            ml_dsa44_cached_run(ML, ML_DSA44_GAS, 0).unwrap();
            slh_dsa_sha2_128s_cached_run(SLH, SLH_DSA_SHA2_128S_GAS, 0).unwrap();
            KEY_CACHE
                .with_borrow(|cache| cache.0.iter().map(|e| e.prepared.0.get()).collect::<Vec<_>>())
        })
        .join()
        .unwrap();
        assert_eq!(handles.len(), 2);
        for handle in handles {
            assert_eq!(probe_handle(handle), -1);
        }
    }

    #[test]
    fn gas_and_length_failures_do_not_populate_cache() {
        clear_thread_key_cache();
        ml_dsa44_cached_run(ML, ML_DSA44_GAS - 1, 0).unwrap();
        slh_dsa_sha2_128s_cached_run(SLH, SLH_DSA_SHA2_128S_GAS - 1, 0).unwrap();
        ml_dsa44_cached_run(&ML[..ML.len() - 1], ML_DSA44_GAS, 0).unwrap();
        slh_dsa_sha2_128s_cached_run(&SLH[..SLH.len() - 1], SLH_DSA_SHA2_128S_GAS, 0).unwrap();
        KEY_CACHE.with_borrow(|cache| assert!(cache.0.is_empty()));
    }

    #[test]
    fn ffi_rejects_bad_arguments_before_reading_memory() {
        let null = std::ptr::null();
        // SAFETY: The ABI promises to reject these lengths/nulls before reading.
        unsafe {
            assert_eq!(bsc_pqc_circl_verify(0, null, usize::MAX, null, 2420, null, 1312), 0);
            assert_eq!(bsc_pqc_circl_verify(1, null, 32, null, 7856, null, 32), -1);
        }
        assert!(matches!(verify(2, &[], &[], &[]), Err(PrecompileError::Fatal(_))));
    }
}
