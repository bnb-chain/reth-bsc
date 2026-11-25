use bsc_pqc_precompiles::*;
use revm_precompile::{
    primitives::Bytes, u64_to_address, PrecompileFn, PrecompileHalt, PrecompileOutput, Precompiles,
};

const ML_VALID: &[u8] = include_bytes!("fixtures/ML-DSA-44-valid.bin");
const ML_CONTEXT: &[u8] = include_bytes!("fixtures/ML-DSA-44-context.bin");
const SLH_VALID: &[u8] = include_bytes!("fixtures/SLH-DSA-SHA2-128s-valid.bin");
const SLH_CONTEXT: &[u8] = include_bytes!("fixtures/SLH-DSA-SHA2-128s-context.bin");
const RESERVOIR: u64 = 73;

fn boolean(cost: u64, valid: bool) -> PrecompileOutput {
    let mut word = [0; 32];
    word[31] = u8::from(valid);
    PrecompileOutput::new(cost, Bytes::copy_from_slice(&word), RESERVOIR)
}

type VerificationCase = (PrecompileFn, u64, &'static [u8], &'static [u8], usize);

fn cases() -> Vec<VerificationCase> {
    vec![
        (ml_dsa44_run, ML_DSA44_GAS, ML_VALID, ML_CONTEXT, ML_DSA44_SIGNATURE_LEN),
        (
            slh_dsa_sha2_128s_run,
            SLH_DSA_SHA2_128S_GAS,
            SLH_VALID,
            SLH_CONTEXT,
            SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        ),
        #[cfg(feature = "openssl-backend")]
        (openssl::ml_dsa44_run, ML_DSA44_GAS, ML_VALID, ML_CONTEXT, ML_DSA44_SIGNATURE_LEN),
        #[cfg(feature = "openssl-backend")]
        (
            openssl::slh_dsa_sha2_128s_run,
            SLH_DSA_SHA2_128S_GAS,
            SLH_VALID,
            SLH_CONTEXT,
            SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        ),
        #[cfg(feature = "circl-backend")]
        (circl::ml_dsa44_run, ML_DSA44_GAS, ML_VALID, ML_CONTEXT, ML_DSA44_SIGNATURE_LEN),
        #[cfg(feature = "circl-backend")]
        (circl::ml_dsa44_cached_run, ML_DSA44_GAS, ML_VALID, ML_CONTEXT, ML_DSA44_SIGNATURE_LEN),
        #[cfg(feature = "circl-backend")]
        (
            circl::slh_dsa_sha2_128s_cached_run,
            SLH_DSA_SHA2_128S_GAS,
            SLH_VALID,
            SLH_CONTEXT,
            SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        ),
        #[cfg(feature = "circl-backend")]
        (
            circl::slh_dsa_sha2_128s_run,
            SLH_DSA_SHA2_128S_GAS,
            SLH_VALID,
            SLH_CONTEXT,
            SLH_DSA_SHA2_128S_SIGNATURE_LEN,
        ),
    ]
}

#[test]
fn registries_and_parallel_verification() {
    let ml = u64_to_address(0x1000);
    let slh = u64_to_address(0x1001);
    let registry = Precompiles::berlin().clone();
    for backend in [
        Backend::RustCrypto,
        #[cfg(feature = "openssl-backend")]
        Backend::OpenSsl,
        #[cfg(feature = "circl-backend")]
        Backend::Circl,
        #[cfg(feature = "circl-backend")]
        Backend::CirclCached,
    ] {
        let mut registry = registry.clone();
        register_with_backend(&mut registry, ml, slh, backend).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let registry = &registry;
                scope.spawn(move || {
                    for _ in 0..8 {
                        for (address, input, cost) in
                            [(ml, ML_VALID, ML_DSA44_GAS), (slh, SLH_VALID, SLH_DSA_SHA2_128S_GAS)]
                        {
                            assert_eq!(
                                registry
                                    .get(&address)
                                    .unwrap()
                                    .execute(input, cost, RESERVOIR)
                                    .unwrap(),
                                boolean(cost, true)
                            );
                        }
                    }
                });
            }
        });
        let original = registry.addresses_set().clone();
        assert!(register_with_backend(&mut registry, ml, slh, backend).is_err());
        assert_eq!(registry.addresses_set(), &original);
    }
}

#[test]
fn backends_agree_on_deterministic_mutations() {
    for (native, cost, valid, _, _) in cases() {
        let rust = if cost == ML_DSA44_GAS { ml_dsa44_run } else { slh_dsa_sha2_128s_run };
        // Spread mutations across message, signature, and key. Deterministic so
        // every failing example is reproducible, with no RNG/test dependency.
        for index in 0..256 {
            let mut input = valid.to_vec();
            let offset = index * (input.len() - 1) / 255;
            input[offset] ^= 1 << (index % 8);
            let reference = rust(&input, cost, RESERVOIR).unwrap();
            assert_eq!(reference, boolean(cost, false), "mutation {index}");
            assert_eq!(native(&input, cost, RESERVOIR).unwrap(), reference, "mutation {index}");
        }
    }
}

#[test]
fn all_backends_reject_noncanonical_ml_dsa_hints() {
    // FIPS 204 HintBitPack: ML-DSA-44 ends with 80 indices and 4 cumulative
    // counts. Duplicate indices were the subject of GHSA-5x2r-hc65-25f9.
    let hints = MESSAGE_LEN + ML_DSA44_SIGNATURE_LEN - 84;
    let counts = hints + 80;
    assert!(ML_VALID[counts] >= 2);
    let used = usize::from(ML_VALID[counts + 3]);
    assert!(used < 80);
    let mut duplicate = ML_VALID.to_vec();
    duplicate[hints + 1] = duplicate[hints];
    let mut padding = ML_VALID.to_vec();
    padding[hints + used] = 1;
    let mut decreasing_counts = ML_VALID.to_vec();
    decreasing_counts[counts + 1] = decreasing_counts[counts] - 1;
    let mut oversized_count = ML_VALID.to_vec();
    oversized_count[counts + 3] = 81;
    for input in [duplicate, padding, decreasing_counts, oversized_count] {
        for (run, _, _, _, _) in cases().into_iter().filter(|case| case.1 == ML_DSA44_GAS) {
            assert_eq!(run(&input, ML_DSA44_GAS, RESERVOIR).unwrap(), boolean(ML_DSA44_GAS, false));
        }
    }
}

#[test]
fn accepts_independent_go_signatures_at_exact_gas() {
    for (run, cost, valid, _, _) in cases() {
        for gas in [cost, cost + 1, u64::MAX] {
            assert_eq!(run(valid, gas, RESERVOIR).unwrap(), boolean(cost, true));
        }
    }
}

#[test]
fn rejects_changed_messages_signatures_keys_and_contexts() {
    for (run, cost, valid, context, signature_len) in cases() {
        assert_eq!(run(context, cost, RESERVOIR).unwrap(), boolean(cost, false));
        for offset in [0, 31, 32, 32 + signature_len - 1, 32 + signature_len, valid.len() - 1] {
            let mut altered = valid.to_vec();
            altered[offset] ^= 1;
            assert_eq!(run(&altered, cost, RESERVOIR).unwrap(), boolean(cost, false));
        }
        for byte in [0, 0xff] {
            assert_eq!(
                run(&vec![byte; valid.len()], cost, RESERVOIR).unwrap(),
                boolean(cost, false)
            );
        }
    }
}

#[test]
fn exact_length_and_gas_precedence() {
    for (run, cost, valid, _, _) in cases() {
        for length in [0, 31, 32, valid.len() - 1, valid.len() + 1, valid.len() * 2] {
            let input = vec![0; length];
            assert_eq!(
                run(&input, cost, RESERVOIR).unwrap(),
                PrecompileOutput::revert(cost, Bytes::new(), RESERVOIR)
            );
            assert_eq!(
                run(&input, cost - 1, RESERVOIR).unwrap(),
                PrecompileOutput::halt(PrecompileHalt::OutOfGas, RESERVOIR)
            );
        }
        assert_eq!(
            run(valid, cost - 1, RESERVOIR).unwrap(),
            PrecompileOutput::halt(PrecompileHalt::OutOfGas, RESERVOIR)
        );
    }
}

#[test]
fn explicit_registration_executes_both_verifiers_and_preserves_existing_entries() {
    let mut registry = Precompiles::berlin().clone();
    let original = registry.clone();
    let ml = u64_to_address(0x1000);
    let slh = u64_to_address(0x1001);
    register(&mut registry, ml, slh).unwrap();
    assert_eq!(registry.len(), original.len() + 2);
    for address in original.addresses() {
        assert_eq!(registry.get(address).unwrap().id(), original.get(address).unwrap().id());
    }
    assert_eq!(
        registry.get(&ml).unwrap().execute(ML_VALID, ML_DSA44_GAS, RESERVOIR).unwrap(),
        boolean(ML_DSA44_GAS, true)
    );
    assert_eq!(
        registry.get(&slh).unwrap().execute(SLH_VALID, SLH_DSA_SHA2_128S_GAS, RESERVOIR).unwrap(),
        boolean(SLH_DSA_SHA2_128S_GAS, true)
    );
    assert!(!Precompiles::berlin().contains(&ml));
}

#[test]
fn conflicting_registration_is_atomic() {
    let mut registry = Precompiles::berlin().clone();
    let original = registry.addresses_set().clone();
    let free = u64_to_address(0x1000);
    let occupied = u64_to_address(1);
    for (ml, slh) in [(free, free), (occupied, free), (free, occupied)] {
        assert!(register(&mut registry, ml, slh).is_err());
        assert_eq!(registry.addresses_set(), &original);
    }
}
