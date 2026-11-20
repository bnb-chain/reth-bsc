# CIRCL decoded-key cache: Apple M5

Measured 2026-11-19 on the same Apple M5 machine as the [backend comparison](pqc-backends-apple-m5.md). CIRCL 1.6.5, Go 1.26.1, Rust 1.99.0, macOS 26.6.1; `GOGC=100`, `GOMAXPROCS=10`. Only the CIRCL optional feature was enabled for this benchmark.

## What changed

An optional Rust thread-local LRU retains at most 16 decoded Go public keys, indexed by algorithm and complete encoded key bytes. Rust owns opaque `runtime/cgo.Handle` integers and releases them on eviction or normal thread exit. Every message/signature still undergoes verification. The CIRCL library itself is unmodified.

ML-DSA public-key decoding expands the public matrix and computes the key hash. Retaining the decoded object avoids repeating that work on cache hits. SLH public-key decoding is small and does not retain the internal per-verification hash state.

## Findings

ML-DSA improves by **2.06–2.25×** on cache hits in the confirmation runs:
repeated-key calls drop from **50.24–50.76 µs** to **22.37–24.62 µs**, and
an eight-key working set drops from **49.08–51.33 µs** to **22.02–22.79 µs**.
The 17-key miss workload stays around **50–52 µs** with no consistent speedup.

SLH has **no repeatable improvement**: small apparent gains in one order reverse
in the other. Repeated-key calls remain around **104–110 µs**. Key caching does
not remove the internal per-hash prefix work discussed in the earlier comparison.

The non-cryptographic ABI roundtrip measured **0.050 µs (about 50 ns)**. Prepared
key import/release measured **31.606 µs for ML-DSA** and **0.197 µs for SLH** in
the initial run. These are separate measurements rather than an exact additive
breakdown, but they support focusing on ML key preparation rather than FFI entry
cost. The optimized mode is explicit and remains experimental.

## Confirmation results

Two passes used opposite cached/uncached order, each with 100 samples, 2 seconds warmup and a 5-second measurement target per case. Values are mean microseconds per complete precompile call, with estimated 95% confidence intervals; lower is faster.

| Order | Algorithm | Workload | Uncached | Cached | Uncached / cached mean |
| --- | --- | --- | ---: | ---: | ---: |
| uncached-first | ml-dsa-44 | repeated-key | 50.760 (48.900–53.433) | 24.620 (23.579–25.751) | 2.062× |
| uncached-first | ml-dsa-44 | working-set-8 | 51.327 (50.611–52.118) | 22.787 (22.343–23.456) | 2.252× |
| uncached-first | ml-dsa-44 | churn-17 | 52.210 (50.945–53.571) | 49.772 (49.390–50.183) | 1.049× |
| uncached-first | slh-dsa-sha2-128s | repeated-key | 109.641 (108.828–110.455) | 106.757 (105.908–107.616) | 1.027× |
| uncached-first | slh-dsa-sha2-128s | working-set-8 | 117.602 (115.095–120.835) | 111.206 (110.378–112.051) | 1.058× |
| uncached-first | slh-dsa-sha2-128s | churn-17 | 114.222 (113.407–115.041) | 120.797 (117.728–124.593) | 0.946× |
| cached-first | ml-dsa-44 | repeated-key | 50.242 (49.874–50.611) | 22.370 (21.736–23.220) | 2.246× |
| cached-first | ml-dsa-44 | working-set-8 | 49.079 (48.630–49.598) | 22.021 (21.820–22.230) | 2.229× |
| cached-first | ml-dsa-44 | churn-17 | 50.088 (49.706–50.487) | 50.457 (50.081–50.838) | 0.993× |
| cached-first | slh-dsa-sha2-128s | repeated-key | 104.434 (103.714–105.210) | 106.888 (106.074–107.704) | 0.977× |
| cached-first | slh-dsa-sha2-128s | working-set-8 | 110.225 (109.397–111.132) | 112.362 (110.752–114.414) | 0.981× |
| cached-first | slh-dsa-sha2-128s | churn-17 | 125.050 (121.229–129.144) | 116.783 (113.117–121.253) | 1.071× |

## Setup and ABI measurements

The initial full run used 50 samples, 1 second warmup, and 3 seconds measurement. These isolated timings include Rust-to-Go transitions. Import/release includes decoded-key preparation, handle registration and deletion, not just byte parsing. The roundtrip returns an unsupported-algorithm error before cryptography; it measures entry/exit plus dispatch/panic guard, not the complete prepared-key lookup path.

| Operation | Mean µs (95% CI) |
| --- | ---: |
| circl-setup/ffi-roundtrip | 0.050 (0.049–0.052) |
| circl-setup/ml-dsa-44/key-import-and-release | 31.606 (30.647–32.630) |
| circl-setup/slh-dsa-sha2-128s/key-import-and-release | 0.197 (0.190–0.208) |

## Workloads and limits

- Seventeen independently checked valid public-key/message/signature tuples per algorithm, generated deterministically with CIRCL and verified with RustCrypto before timing. No invalid-signature early exits are used in the cache comparison.
- Repeated-key uses one tuple; working-set-8 cycles the first eight; churn-17 cycles all seventeen, exceeding the 16-entry LRU. The cursor persists across Criterion batches so every churn call misses.
- A hit makes one Go call. A miss imports and verifies in two calls, plus an eviction release when full. Cached and uncached modes use the same corpus and perform the same pure-scheme signature check.
- Gas, length checks, reservoir handling and output encoding remain the same. This is key caching, not result caching; cache hits do not change gas.
- Warm single-Rust-thread microbenchmarks with Go GC enabled. OS background activity, scheduling and thermals are uncontrolled; confidence intervals are within-run estimates, not cross-machine guarantees.
- Memory scales with Rust worker count. Sixteen is a bound on live retained handles, not RSS; Go reclaims evicted objects on its GC schedule. Untrusted keys can force misses.
- SLH verification work varies with the message/signature, so timings across different corpus workloads should not be interpreted as a cache effect.
- Previous RustCrypto/OpenSSL measurements decoded fresh keys. This does not establish a speed advantage over equivalently cached versions of those backends.
- No mixed-client consensus, full-node throughput, Linux, x86_64 or cold process-start measurements were performed. The interface remains provisional, as described in [implementation notes](../pqc-precompiles.md).

## Validation

All 13 combined-backend tests passed (5 unit tests and 8 integration tests). New checks cover decoded-key ownership after input destruction, eviction order and capacity, handle release on eviction/drop/thread exit, and gas/length rejection before cache access. Existing mutation, noncanonical hint, wrong-context, registration and parallel-call tests also exercise cached mode. Clippy with warnings denied, Go vet, module checksum verification, and the reth_bsc library check passed.

## Reproduce

```sh
export GOGC=100 GOMAXPROCS=10
cargo +1.99.0 bench -p bsc-pqc-precompiles --features circl-backend --bench circl_overhead --locked -- \
  --save-baseline circl-cache-initial
for pqc_cache_order in uncached-first cached-first; do
  PQC_CACHE_ORDER="$pqc_cache_order" cargo +1.99.0 bench -p bsc-pqc-precompiles --features circl-backend --bench circl_overhead --locked -- \
    'circl-cache/' --sample-size 100 --warm-up-time 2 --measurement-time 5 --save-baseline "circl-cache-confirm-$pqc_cache_order"
done
```

The [saved JSON](pqc-circl-cache-apple-m5.json) contains all 39 records, raw samples, estimates, runtime settings, and source/binary hashes. The general three-backend report preserves the earlier uncached measurements.
