# PQ precompile backends: Apple M5

Measured 2026-11-19 on an Apple M5 MacBook Pro (10 cores: 4 performance,
6 efficiency), 32 GB RAM, macOS 26.6.1, Rust 1.99.0, and Homebrew
OpenSSL 3.6.3. OpenSSL uses its default provider, **not a FIPS-certified build**.
RustCrypto versions are `ml-dsa 0.1.2` and `slh-dsa 0.2.0-rc.5`.

A subsequent [CIRCL decoded-key cache experiment](pqc-circl-cache-apple-m5.md)
measures reused and changing keys. The results on this page retain fresh-key
decoding and should not be compared directly with cached-key timings.

## Three-backend comparison with CIRCL

CIRCL 1.6.5 was compiled with Go 1.26.1 as a C archive and called through the
actual Rust FFI adapter. All three backends were linked into the same binary.
`GOGC=100` and `GOMAXPROCS=10` were set explicitly; Go GC remained enabled.
Builds and correctness tests completed before timing. The machine and Rust/OpenSSL
versions are the same as above, but these are fresh runs, not comparisons against
old measurements. OS scheduling, background activity, and thermals remain uncontrolled.

The full suite used 30 samples, 1 second warmup, and 3 seconds measurement.
Three subsequent valid-signature passes used 100 samples, 2 seconds warmup, and
5 seconds measurement per case, rotating each backend into first position.
Values below are mean microseconds per complete call with estimated 95% confidence
intervals, including key decoding and FFI overhead; lower is faster.

| First backend | Algorithm | RustCrypto | OpenSSL | CIRCL |
| --- | --- | ---: | ---: | ---: |
| rustcrypto | ml-dsa-44 | 52.683 (52.645–52.725) | 49.095 (48.826–49.403) | 47.570 (47.467–47.703) |
| rustcrypto | slh-dsa-sha2-128s | 64.325 (64.202–64.508) | 114.484 (114.362–114.649) | 104.637 (104.440–104.847) |
| openssl | ml-dsa-44 | 52.591 (52.482–52.725) | 49.899 (48.967–51.496) | 47.367 (47.327–47.405) |
| openssl | slh-dsa-sha2-128s | 64.528 (64.369–64.702) | 114.873 (114.699–115.111) | 106.355 (106.187–106.553) |
| circl | ml-dsa-44 | 52.587 (52.389–52.876) | 48.650 (48.528–48.780) | 50.129 (48.063–53.775) |
| circl | slh-dsa-sha2-128s | 65.054 (64.492–65.736) | 118.488 (116.160–122.237) | 104.474 (104.232–104.771) |

CIRCL's ML-DSA means ranged from **47.367–50.129 µs**, versus
**48.650–49.899 µs** for OpenSSL and **52.587–52.683 µs** for RustCrypto.
CIRCL led in the first two passes; in the third, OpenSSL had a lower mean and
CIRCL's confidence interval widened and overlapped both alternatives. Treat
CIRCL and OpenSSL as competitive here, not as a stable CIRCL win.

For SLH-DSA, RustCrypto consistently led at **64.325–65.054 µs**, followed by
CIRCL at **104.474–106.355 µs**, then OpenSSL at **114.484–118.488 µs**.
RustCrypto delivered roughly **1.61–1.65×** CIRCL's throughput in paired runs.
The results support retaining CIRCL as an optional evaluation backend; they do
not justify selecting a production backend solely from this machine's timings.

### Invalid-input comparison

Means in microseconds from the full three-backend run. Short inputs return
from the shared Rust wrapper before entering any cryptographic library.

| Algorithm | Case | RustCrypto | OpenSSL | CIRCL |
| --- | --- | ---: | ---: | ---: |
| ml-dsa-44 | wrong-message | 52.8631 | 49.9483 | 47.4478 |
| ml-dsa-44 | wrong-signature | 52.7758 | 48.6950 | 47.5831 |
| ml-dsa-44 | all-ff | 22.6811 | 4.1680 | 27.5732 |
| ml-dsa-44 | short-input | 0.0041 | 0.0039 | 0.0040 |
| slh-dsa-sha2-128s | wrong-message | 65.9032 | 119.2769 | 106.2759 |
| slh-dsa-sha2-128s | wrong-signature | 63.7124 | 114.0060 | 101.1032 |
| slh-dsa-sha2-128s | all-ff | 65.0041 | 116.8339 | 104.0317 |
| slh-dsa-sha2-128s | short-input | 0.0040 | 0.0040 | 0.0038 |

### Reproduce the CIRCL comparison

```sh
export PKG_CONFIG_PATH=/opt/homebrew/opt/openssl@3/lib/pkgconfig
export GOGC=100 GOMAXPROCS=10
cargo +1.99.0 bench -p bsc-pqc-precompiles --all-features --bench verification --locked -- \
  --save-baseline circl-full
for pqc_order in rustcrypto-first openssl-first circl-first; do
  PQC_BENCH_ORDER="$pqc_order" cargo +1.99.0 bench -p bsc-pqc-precompiles --all-features --bench verification --locked -- \
    '/valid/' --sample-size 100 --warm-up-time 2 --measurement-time 5 --save-baseline "circl-confirm-$pqc_order"
done
```

Only macOS ARM64 was tested. The CIRCL build also accepts native Linux GNU and
macOS x86_64 targets, but cross-compilation and musl are not implemented.
No full node, mixed-client consensus, cold-start, or diverse-key throughput test
was run. The provisional interface still needs BEP-736 alignment.

Validation passed: 8 integration tests for the default backend; 8 integration
plus 1 FFI unit test with CIRCL alone or all backends enabled; all-target Clippy
with all features and warnings denied; Go vet and module checksum verification;
and the reth_bsc library check with both optional backends enabled. Tests cover
512 distinct mutations across both algorithms, noncanonical ML-DSA hints, wrong
contexts, gas/length behavior, and concurrent registered calls. CIRCL-generated
fixtures are independent interoperability evidence for RustCrypto/OpenSSL and
regression evidence for CIRCL itself.

The `circl_comparison` section of the saved JSON contains all 48 case records,
raw samples, confidence intervals, source hashes, and runtime settings. Historical
measurements below are preserved.

## Earlier two-backend valid-signature runs

The earlier two-backend confirmation used **two passes with opposite backend order**, each
with 500 samples, 5 seconds warmup, and a 30-second measurement target per case.
Criterion extended some cases to collect all 500 samples. In total these passes
measured **4,133,250 calls over 279.75 seconds**, excluding warmup and analysis.

Values are Criterion's mean time per complete precompile call in microseconds,
with estimated 95% confidence intervals. The same binary and inputs were used
for both passes; only the backend execution order changed.

| Backend order | Algorithm | RustCrypto mean (95% CI) | OpenSSL mean (95% CI) |
| --- | --- | ---: | ---: |
| RustCrypto first | ML-DSA-44 | 53.865 (53.516–54.244) | 52.925 (51.991–53.940) |
| OpenSSL first | ML-DSA-44 | 61.409 (60.098–62.779) | 51.626 (50.622–52.818) |
| RustCrypto first | SLH-DSA-SHA2-128s | 68.076 (67.587–68.585) | 120.142 (119.173–121.189) |
| OpenSSL first | SLH-DSA-SHA2-128s | 65.098 (64.809–65.412) | 122.881 (121.550–124.283) |

**Interpretation:** RustCrypto's SLH-DSA advantage persists across both orders:
1.76–1.89× throughput based on the sample means. OpenSSL has the lower ML-DSA
mean in both passes, but the advantage ranges from 1.02× to 1.19× and the first
pass's confidence intervals overlap. The earlier 1.08× ML-DSA result should not
be treated as a stable speedup. The variation between passes is larger than
some within-pass confidence intervals; execution order alone cannot be isolated
from scheduling, background activity, or thermal/frequency changes in this test.

These results support keeping both backends available for evaluation. They do
not establish full-node throughput or justify changing the gas schedule.

### Earlier 100-sample confirmation

The earlier confirmation run used 100 samples, 2 seconds warmup, and 5 seconds
measurement per case. Values are Criterion's mean time per complete precompile
call, in microseconds; intervals are the estimated 95% confidence intervals.

| Algorithm | RustCrypto mean (95% CI) | OpenSSL mean (95% CI) | Result on this machine |
| --- | ---: | ---: | --- |
| ML-DSA-44 | 52.602 (52.526–52.692) | 48.644 (48.490–48.898) | OpenSSL 1.08× throughput, about 7.5% less time |
| SLH-DSA-SHA2-128s | 62.397 (62.137–62.701) | 114.736 (114.401–115.267) | RustCrypto 1.84× throughput, about 45.6% less time |

## Invalid inputs

The initial full run used 30 samples, 1 second warmup, and 3 seconds measurement
per case. Times below are means in microseconds.

| Algorithm | Case | RustCrypto | OpenSSL |
| --- | --- | ---: | ---: |
| ML-DSA-44 | Wrong message | 52.587 | 49.768 |
| ML-DSA-44 | Corrupted signature | 61.568 | 50.546 |
| ML-DSA-44 | All bytes `0xff` | 22.369 | 4.158 |
| ML-DSA-44 | Short input | 0.0039 | 0.0039 |
| SLH-DSA-SHA2-128s | Wrong message | 65.928 | 118.633 |
| SLH-DSA-SHA2-128s | Corrupted signature | 61.404 | 112.130 |
| SLH-DSA-SHA2-128s | All bytes `0xff` | 64.542 | 129.452 |
| SLH-DSA-SHA2-128s | Short input | 0.0040 | 0.0038 |

The short-input path returns from the shared Rust wrapper before calling either
crypto implementation. Its nanosecond-scale difference is not a crypto result.
All-`0xff` ML-DSA inputs allow early rejection; their speed does not represent
the cost of arbitrary invalid signatures. Timing varied between cases/runs,
including high outliers; confidence intervals describe these repeated-input
measurements, not the performance distribution across machines or public keys.

## Method and limits

- Same independently generated CIRCL v1.6.5 fixture for each backend; one key and
  valid signature per algorithm. Each case asserts its complete expected result
  before measurement, so backend failure cannot masquerade as a speedup.
- Includes exact-length/gas checks, public-key decoding/import, per-call
  allocation, verification, and ABI output allocation. No cached public keys.
- Excludes fixture preparation and one-time OpenSSL provider initialization.
  OpenSSL keeps its library context and algorithm handles for the process lifetime.
- Single-threaded, hot-cache microbenchmarks, not full node TPS, signing,
  cold-start latency, diverse-key traffic, or mixed-client consensus tests.
- Cargo bench profile: optimization level 3, thin LTO, 16 codegen units. No
  `target-cpu=native` override; OpenSSL's normal runtime acceleration is enabled.
- Node compilation and other test workloads finished before the longer
  confirmation run. OS background activity and CPU scheduling were not controlled.
- Both backends preserve the prototype's provisional interface. BEP-736 input
  ordering, invalid-length behavior, addresses, and activation still need
  integration as explained in [the implementation notes](../pqc-precompiles.md).
- Repeat on Linux x86_64 and deployment hardware before selecting a production
  backend or using these measurements for gas pricing.

## Reproduce

From the repository root, with OpenSSL >= 3.5, development headers, `pkg-config`,
and a C compiler available:

```sh
export PKG_CONFIG_PATH="/opt/homebrew/opt/openssl@3/lib/pkgconfig"

# Full suite: valid, wrong message, corrupted signature, all-ff, short input.
cargo +1.99.0 bench -p bsc-pqc-precompiles --features openssl-backend --bench verification --locked

# Earlier valid-signature comparison.
cargo +1.99.0 bench -p bsc-pqc-precompiles --features openssl-backend --bench verification --locked -- \
  '/valid/' --sample-size 100 --warm-up-time 2 --measurement-time 5

# Long confirmation, two orders. Run sequentially on an otherwise quiet machine.
PQC_BENCH_ORDER=rustcrypto-first cargo +1.99.0 bench -p bsc-pqc-precompiles --features openssl-backend --bench verification --locked -- \
  '/valid/' --sample-size 500 --warm-up-time 5 --measurement-time 30 --save-baseline long-rustcrypto-first
PQC_BENCH_ORDER=openssl-first cargo +1.99.0 bench -p bsc-pqc-precompiles --features openssl-backend --bench verification --locked -- \
  '/valid/' --sample-size 500 --warm-up-time 5 --measurement-time 30 --save-baseline long-openssl-first
```

The feature adds both implementations; it does not select or activate either
backend on a network. The local registry can select `Backend::OpenSsl` or
`Backend::RustCrypto` explicitly.

[Saved estimates, raw samples, environment and source/fixture hashes](pqc-backends-apple-m5.json)
include the initial run, earlier confirmation, and both long passes. The long
passes also retain iteration counts, measured durations, and aggregate time per
call (total time divided by total iterations), which differs from the mean of
per-sample timings shown above. Criterion retains the named baselines under
`target/criterion`. The long runs exercise valid signatures; the invalid-input
table above remains from the initial full run.

## Validation

Both backend configurations passed: 8 tests with OpenSSL enabled and 5 tests
without it. Coverage includes valid CIRCL signatures, wrong contexts, altered
messages/signatures/keys, malformed lengths, gas boundaries, ABI results,
registry conflicts, 512 deterministic mutations, noncanonical ML-DSA hints,
and concurrent registered OpenSSL calls.

Clippy with all targets and OpenSSL enabled passed with warnings denied.
Clang's static analyzer reported no findings in the C adapter. The full
`reth_bsc` library check with `experimental-pqc-openssl` and default features
disabled passed. No existing locked package version was upgraded or removed.
