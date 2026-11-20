use bsc_pqc_precompiles::{circl, *};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use revm_precompile::{primitives::Bytes, PrecompileFn, PrecompileOutput};
use std::{ffi::c_int, hint::black_box, ptr, time::Duration};

// Direct ABI calls are used only to isolate transition and key preparation costs.
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
}

fn benchmark(c: &mut Criterion) {
    let order = std::env::var("PQC_CACHE_ORDER").unwrap_or_else(|_| "uncached-first".into());
    assert!(matches!(order.as_str(), "uncached-first" | "cached-first"));
    eprintln!(
        "{}; order={order}; GOGC={:?}; GOMAXPROCS={:?}",
        circl::version(),
        std::env::var("GOGC"),
        std::env::var("GOMAXPROCS")
    );
    let mut setup = c.benchmark_group("circl-setup");
    setup.bench_function("ffi-roundtrip", |b| {
        // SAFETY: An unsupported algorithm returns -1 before inspecting pointers.
        // Includes the actual Go entry/exit, panic guard, and argument dispatch.
        let call = || unsafe {
            bsc_pqc_circl_verify(
                black_box(u32::MAX),
                ptr::null(),
                0,
                ptr::null(),
                0,
                ptr::null(),
                0,
            )
        };
        assert_eq!(call(), -1);
        b.iter(|| black_box(call()));
    });
    setup.finish();

    for (algorithm, id, uncached, cached, gas, signature_len, corpus, reference) in [
        (
            "ml-dsa-44",
            0,
            circl::ml_dsa44_run as PrecompileFn,
            circl::ml_dsa44_cached_run as PrecompileFn,
            ML_DSA44_GAS,
            ML_DSA44_SIGNATURE_LEN,
            include_bytes!("../tests/fixtures/cache/ML-DSA-44-17-keys.bin").as_slice(),
            ml_dsa44_run as PrecompileFn,
        ),
        (
            "slh-dsa-sha2-128s",
            1,
            circl::slh_dsa_sha2_128s_run,
            circl::slh_dsa_sha2_128s_cached_run,
            SLH_DSA_SHA2_128S_GAS,
            SLH_DSA_SHA2_128S_SIGNATURE_LEN,
            include_bytes!("../tests/fixtures/cache/SLH-DSA-SHA2-128s-17-keys.bin").as_slice(),
            slh_dsa_sha2_128s_run,
        ),
    ] {
        let input_len = MESSAGE_LEN
            + signature_len
            + if id == 0 { ML_DSA44_PUBLIC_KEY_LEN } else { SLH_DSA_SHA2_128S_PUBLIC_KEY_LEN };
        assert_eq!(corpus.len(), 17 * input_len);
        let inputs: Vec<_> = corpus.chunks_exact(input_len).collect();
        let mut word = [0; 32];
        word[31] = 1;
        let expected = PrecompileOutput::new(gas, Bytes::copy_from_slice(&word), 0);
        for input in &inputs {
            // A separate implementation validates every distinct valid key/signature.
            assert_eq!(reference(input, gas, 0).unwrap(), expected);
            assert_eq!(uncached(input, gas, 0).unwrap(), expected);
            assert_eq!(cached(input, gas, 0).unwrap(), expected);
        }
        circl::clear_thread_key_cache();
        let key = &inputs[0][MESSAGE_LEN + signature_len..];
        let mut setup = c.benchmark_group("circl-setup");
        setup.bench_function(BenchmarkId::new(algorithm, "key-import-and-release"), |b| {
            b.iter(|| {
                // SAFETY: Go copies this live key; the returned owned handle is
                // released once, synchronously, without outstanding verification.
                unsafe {
                    let handle = bsc_pqc_circl_key_new(id, black_box(key.as_ptr()), key.len());
                    assert_ne!(handle, 0);
                    assert_eq!(black_box(bsc_pqc_circl_key_free(handle)), 1);
                }
            });
        });
        setup.finish();

        let mut group = c.benchmark_group(format!("circl-cache/{algorithm}"));
        for (workload, count) in [("repeated-key", 1), ("working-set-8", 8), ("churn-17", 17)] {
            let mut modes = [("uncached", uncached), ("cached", cached)];
            if order == "cached-first" {
                modes.reverse();
            }
            for (mode, run) in modes {
                circl::clear_thread_key_cache();
                for input in inputs.iter().take(count) {
                    assert_eq!(run(input, gas, 0).unwrap(), expected);
                }
                // Keep the cursor across Criterion batches: churn-17 misses on
                // every call, even when warmup or sample batch boundaries change.
                let mut cursor = 0;
                group.bench_function(BenchmarkId::new(workload, mode), |b| {
                    b.iter(|| {
                        let input = inputs[cursor];
                        cursor = (cursor + 1) % count;
                        black_box(run(black_box(input), black_box(gas), 0).unwrap())
                    });
                });
            }
        }
        group.finish();
        circl::clear_thread_key_cache();
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default().sample_size(50)
        .warm_up_time(Duration::from_secs(1)).measurement_time(Duration::from_secs(3));
    targets = benchmark
}
criterion_main!(benches);
