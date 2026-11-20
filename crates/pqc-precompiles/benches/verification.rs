use bsc_pqc_precompiles::*;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use revm_precompile::{primitives::Bytes, PrecompileFn, PrecompileOutput};
use std::{hint::black_box, time::Duration};

fn verification(c: &mut Criterion) {
    let order = std::env::var("PQC_BENCH_ORDER").unwrap_or_else(|_| "rustcrypto-first".to_owned());
    assert!(
        matches!(order.as_str(), "rustcrypto-first" | "openssl-first" | "circl-first"),
        "invalid PQC_BENCH_ORDER"
    );
    #[cfg(feature = "openssl-backend")]
    openssl::initialize().expect("OpenSSL PQ algorithms unavailable");
    #[cfg(feature = "openssl-backend")]
    eprintln!(
        "Backend: {}; provider=default; target={}-{}; order={order}",
        openssl::version(),
        std::env::consts::ARCH,
        std::env::consts::OS
    );
    #[cfg(feature = "circl-backend")]
    eprintln!(
        "Backend: {}; order={order}; GOGC={:?}; GOMAXPROCS={:?}",
        circl::version(),
        std::env::var("GOGC"),
        std::env::var("GOMAXPROCS")
    );
    for (algorithm, rust, gas, fixture) in [
        (
            "ml-dsa-44",
            ml_dsa44_run as PrecompileFn,
            ML_DSA44_GAS,
            include_bytes!("../tests/fixtures/ML-DSA-44-valid.bin").as_slice(),
        ),
        (
            "slh-dsa-sha2-128s",
            slh_dsa_sha2_128s_run,
            SLH_DSA_SHA2_128S_GAS,
            include_bytes!("../tests/fixtures/SLH-DSA-SHA2-128s-valid.bin").as_slice(),
        ),
    ] {
        let mut wrong_message = fixture.to_vec();
        wrong_message[0] ^= 1;
        let mut wrong_signature = fixture.to_vec();
        wrong_signature[MESSAGE_LEN] ^= 1;
        let mut group = c.benchmark_group(algorithm);
        for (case, input, valid, malformed_length) in [
            ("valid", fixture.to_vec(), true, false),
            ("wrong-message", wrong_message, false, false),
            ("wrong-signature", wrong_signature, false, false),
            ("all-ff", vec![0xff; fixture.len()], false, false),
            ("short-input", fixture[..fixture.len() - 1].to_vec(), false, true),
        ] {
            let expected = if malformed_length {
                PrecompileOutput::revert(gas, Bytes::new(), 0)
            } else {
                let mut output = [0; 32];
                output[31] = u8::from(valid);
                PrecompileOutput::new(gas, Bytes::copy_from_slice(&output), 0)
            };
            let mut backends: Vec<(&str, PrecompileFn)> = vec![
                ("rustcrypto", rust),
                #[cfg(feature = "openssl-backend")]
                (
                    "openssl",
                    if gas == ML_DSA44_GAS {
                        openssl::ml_dsa44_run
                    } else {
                        openssl::slh_dsa_sha2_128s_run
                    },
                ),
                #[cfg(feature = "circl-backend")]
                (
                    "circl",
                    if gas == ML_DSA44_GAS {
                        circl::ml_dsa44_run
                    } else {
                        circl::slh_dsa_sha2_128s_run
                    },
                ),
            ];
            let first = order.strip_suffix("-first").unwrap();
            let position = backends
                .iter()
                .position(|(name, _)| *name == first)
                .expect("PQC_BENCH_ORDER requests a backend that was not compiled");
            backends.rotate_left(position);
            for (backend, run) in backends {
                // Assert correctness outside the timed loop. A fast backend
                // failure must never be reported as a verification speedup.
                assert_eq!(run(&input, gas, 0).unwrap(), expected, "{algorithm}/{case}/{backend}");
                group.bench_with_input(BenchmarkId::new(case, backend), &input, |b, input| {
                    // Includes parsing, key import/decoding, per-call allocation,
                    // verification, and output allocation; no cached public keys.
                    b.iter(|| black_box(run(black_box(input), black_box(gas), 0).unwrap()));
                });
            }
        }
        group.finish();
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(30)
        .warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3));
    targets = verification
}
criterion_main!(benches);
