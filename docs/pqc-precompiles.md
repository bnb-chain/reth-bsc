# Experimental PQ verification precompiles

**Benchmarking purposes only.** This prototype compares verification backends and
adapter overhead. It is not a production implementation or a network activation
proposal; its provisional interface is not yet BEP-736 compatible.

This prototype implements ML-DSA-44 and SLH-DSA-SHA2-128s verification using
RustCrypto, OpenSSL, or Cloudflare CIRCL through REVM's precompile API. It is based on
`origin/develop` at `573e04efa`. The original interface was developed before a Go BSC prototype was located.

## Scope and activation

The `bsc-pqc-precompiles` workspace crate can be tested independently. Enabling
the node's `experimental-pqc` Cargo feature re-exports it as
`reth_bsc::evm::precompiles::pqc`. The feature does **not** register precompiles
on any network, modify any existing hardfork, or add a CLI activation flag.

For a local harness, clone a REVM precompile registry and call
`pqc::register(&mut registry, ml_address, slh_address)`. Registration rejects
duplicate or occupied registry addresses atomically. The caller must also check
for existing contracts at those addresses in the local chain state. Addresses
`0x1000` and `0x1001` in tests are arbitrary test addresses, not allocations.

## Provisional interface

The comparison currently preserves the original experimental interface for all
backends. It is **not yet BEP-736 compatible**. The subsequently discovered
[BEP-736 draft](https://github.com/bnb-chain/BEPs/pull/736/files) specifies
`public_key || message || signature`, addresses `0x101`/`0x102`, and a false
result for invalid lengths instead of a revert. Align all backends with the
agreed draft before any network integration; do not use the test addresses as
production allocations.

Input is the exact concatenation `message[32] || signature || public_key`.
There are no ABI offsets, dynamic-length headers, or ignored trailing bytes.

| Parameter | ML-DSA-44 | SLH-DSA-SHA2-128s |
| --- | ---: | ---: |
| Message bytes | 32 | 32 |
| Signature bytes | 2,420 | 7,856 |
| Public-key bytes | 1,312 | 32 |
| Total input bytes | 3,764 | 7,920 |
| Experimental verification gas | 10,000 | 60,000 |

The verifiers use the finalized FIPS **pure** signature scheme with an empty
context. A 32-byte `userOpHash` is treated as the message. This does not use
HashML-DSA, HashSLH-DSA, raw internal verification, or a caller-supplied context.
Wallets must sign with the same convention. Account contracts remain responsible
for binding the message to the chain, account, nonce, and requested operation.

Behavior:

- Insufficient gas halts with `OutOfGas`, before input parsing or cryptography.
- An incorrect total input length reverts with empty output and the fixed gas cost.
- A correctly sized but invalid signature or encoding succeeds with ABI `false`.
- A valid signature succeeds with ABI `true`.
- Both boolean results are exactly 32 bytes, with the last byte set to 0 or 1.
- The REVM state-gas reservoir is preserved on every path.

Gas prices come from the exploratory report, not measurements of this Rust
implementation or an agreed production schedule. The interface and activation
rules require agreement across clients before deployment.

## OpenSSL backend

The optional `openssl-backend` crate feature builds a small C adapter to
OpenSSL >= 3.5 using `cc` and `pkg-config`. It links only `libcrypto` and adds no
replacement cryptographic implementation. The default build still needs no
OpenSSL. Both backends remain available together when the feature is enabled.

```sh
# macOS Homebrew; on Linux install OpenSSL >= 3.5 development files and pkg-config.
export PKG_CONFIG_PATH="/opt/homebrew/opt/openssl@3/lib/pkgconfig"
cargo +1.99.0 test -p bsc-pqc-precompiles --features openssl-backend --locked
cargo +1.99.0 clippy -p bsc-pqc-precompiles --all-targets --features openssl-backend --locked -- -D warnings
cargo +1.99.0 check -p reth_bsc --lib --no-default-features --features experimental-pqc-openssl --locked
```

Select the backend explicitly in a local harness:

```rust,ignore
pqc::register_with_backend(&mut registry, ml_address, slh_address, pqc::Backend::OpenSsl)?;
// Or pqc::Backend::RustCrypto. Existing pqc::register selects RustCrypto.
```

Registration checks availability of both algorithms before changing the registry.
The C adapter maintains one process-lifetime library context, explicitly loads
the default provider, and caches immutable algorithm handles. It does not load
configuration into that context. Each call imports its raw public key, allocates
a separate EVP verification context, performs pure-message verification with an
empty context, and frees the per-call objects. Concurrent calls share no mutable
key or verification context. Backend initialization/allocation/API failures are
fatal errors, not invalid-signature results and not a fallback to another backend.

This uses the **default provider**, not a certified FIPS module. The wrapper has
not been independently audited. An OpenSSL FIPS validation does not certify this
adapter or the node. Production builds need a pinned and maintained libcrypto
distribution, reproducible provider selection, and cross-client testing.

## CIRCL backend

The optional crate feature `circl-backend` builds `native/circl` as a Go
`c-archive` and links it into Rust. The node exposes the corresponding
`experimental-pqc-circl` feature. Select `Backend::Circl` with
`register_with_backend`; the default remains RustCrypto and no network registry
is activated.

Dependencies are pinned by `go.mod` and `go.sum`: CIRCL 1.6.5, x/crypto 0.54.0,
and x/sys 0.47.0. Go >= 1.25 and a C compiler are required only when enabling
this backend. The build uses `-mod=readonly`, disables workspace/toolchain auto
selection, and places its Go build cache under Cargo's output directory. Set
`GO` to select a Go compiler. Native macOS and Linux GNU builds on arm64/x86_64
are accepted; only macOS arm64 has been exercised here. Cross-compilation,
Windows, and musl are explicitly rejected rather than linking a host archive
into a target binary.

```sh
cargo +1.99.0 test -p bsc-pqc-precompiles --features circl-backend --locked
# All three backends, with OpenSSL configured as above:
cargo +1.99.0 test -p bsc-pqc-precompiles --all-features --locked
cargo +1.99.0 bench -p bsc-pqc-precompiles --all-features --bench verification --locked
cargo +1.99.0 check -p reth_bsc --lib --no-default-features --features experimental-pqc-circl --locked
```

Go modules must already be cached for offline builds. Use `GOPROXY=off` (or
`CARGO_NET_OFFLINE=true`, which the build script translates for Go) to prohibit
module downloads. Cargo's `--offline` flag alone does not configure an external
Go process. Ordinary builds may fetch the pinned modules from the configured
Go module proxy.

The exported C ABI borrows read-only Rust byte buffers for one synchronous call;
it does not copy the entire input, retain pointers, or return Go pointers. It
checks algorithm/length/null arguments before constructing bounded Go slices.
The uncached entry point decodes keys on each call; verification uses per-call state and pure signatures
with an empty context. Recoverable Go panics become a fatal backend error,
never a false verification result or an automatic fallback. Runtime-fatal
failures, such as out-of-memory, are not recoverable through this boundary.

Unlike the OpenSSL adapter, this embeds a Go runtime, scheduler, and garbage
collector in the node process. The benchmark includes Rust-to-Go transitions,
key parsing, and normal allocation/GC costs, after runtime startup. No runtime
settings are changed by the adapter. `GOGC` and `GOMAXPROCS` affect performance
and are printed by the benchmark; report them when comparing runs.

### Optional decoded-key cache

`Backend::CirclCached` selects a Rust-managed LRU cache with at most **16 live
Go key handles per Rust thread**, shared between the two algorithms. Select it
explicitly; `Backend::Circl` remains the uncached comparison path:

```rust,ignore
pqc::register_with_backend(&mut registry, ml_address, slh_address, pqc::Backend::CirclCached)?;
```

The standalone entry points are `circl::ml_dsa44_cached_run` and
`circl::slh_dsa_sha2_128s_cached_run`. The cache compares the algorithm and complete
encoded public key bytes; it caches neither signatures nor verification results.
Every call verifies its own message and signature, and gas/length checks happen
before cache access. Gas and the provisional input/output behavior are unchanged.

Go owns each decoded key through `runtime/cgo.Handle`; only an integer handle
crosses the ABI. The adapter copies/decodes raw keys into Go memory on import.
Rust owns a separate encoded-key copy for lookup and deletes each Go handle on
eviction, explicit `circl::clear_thread_key_cache()`, or normal Rust thread exit.
Decoded keys are immutable during verification. There is no global Rust cache
lock, and handles are never evicted while a call uses them. Handle-release failure
indicates a lifecycle bug and aborts instead of silently leaking/corrupting state.

A cache hit uses one Rust-to-Go call. A miss imports the key and verifies it in
two calls, with a third release call when evicting a key. No cryptographic code
inside CIRCL was changed. The optimization retains CIRCL's ML-DSA expanded
public matrix and key hash; it does not introduce SLH hash-prefix caching.

The bound applies to live retained handles, not process RSS: memory scales with
Rust worker count, and Go's GC reclaims retired objects on its own schedule.
Untrusted keys can churn the cache and eliminate the hit-rate benefit. For that
reason the benchmark includes an 8-key working set and a 17-key cycle which
misses on every call. Cache state must not be used to discount consensus gas.

```sh
export GOGC=100 GOMAXPROCS=10
cargo +1.99.0 bench -p bsc-pqc-precompiles --features circl-backend --bench circl_overhead --locked
```

The benchmark exercises the actual thread-local adapter and compares cached
and uncached calls using valid signatures for every key. It also measures a
non-cryptographic Go ABI roundtrip and prepared-key import/release. See the
[CIRCL cache experiment](benchmarks/pqc-circl-cache-apple-m5.md) for saved results,
confidence intervals, and reproduction commands.

### Review and adoption evidence

CIRCL has documented external review and real deployment, but these facts do
not establish a comprehensive independent audit of our exact two verifiers:

- [zkSecurity's published review findings](https://www.linkedin.com/posts/zksecurity_ai-meets-cryptography-1-what-ai-found-in-activity-7480539042330157056-JUX3)
  describe seven confirmed, fixed bugs discovered through AI-assisted analysis
  and expert validation. The listed findings concern threshold RSA, BLS, HPKE,
  DLEQ, and attribute-based encryption, not an audit conclusion for ML-DSA or
  SLH-DSA. No public report establishing coverage of both selected implementations
  was located during this evaluation.
- [Cloudflare's origin-authentication deployment](https://blog.cloudflare.com/post-quantum-authentication-to-origins/)
  reports using CIRCL to parse and validate ML-DSA certificates in its Go control
  plane. The article also plans replacement with Go's standard library, so this
  is deployment evidence rather than a claim that every current TLS connection
  executes CIRCL.
- [Proton's Go crypto dependency manifest](https://github.com/ProtonMail/go-crypto/blob/main/go.mod)
  lists CIRCL. This establishes adoption of the library, not use of these PQ
  signature packages. Broad production deployment of CIRCL's SLH-DSA-SHA2-128s
  implementation was not established.
- [BSC's closed, unmerged PQ migration prototype](https://github.com/bnb-chain/bsc/pull/3660/files)
  imports CIRCL 1.6.3 for ML-DSA-44. Our backend uses 1.6.5, matching the existing
  fixtures. This is not a claim of BSC production use or exact version parity.

The [CIRCL maintainers](https://github.com/cloudflare/circl#security-disclaimer)
explicitly describe some contents as experimental. This Rust/Go adapter has not
been independently audited either.

## Backend benchmark

```sh
PKG_CONFIG_PATH=/opt/homebrew/opt/openssl@3/lib/pkgconfig \
  cargo +1.99.0 bench -p bsc-pqc-precompiles --features openssl-backend --bench verification --locked
```

Criterion compares valid signatures, wrong messages, corrupted signatures,
all-`0xff` inputs, and short inputs for each algorithm. Every case checks the
expected full precompile result before timing. Timed work includes input checks,
key decoding/import, per-call allocation, verification, and ABI output allocation;
it excludes fixture construction, Go runtime startup, and one-time OpenSSL provider initialization.
Public keys are not cached. All backends use the same CIRCL fixture and gas limit.

Default settings are 30 samples, 1 second warmup, and 3 seconds measurement per
case, in the optimized Cargo bench profile. Criterion writes estimates and raw
samples under `target/criterion`. The binary prints its linked OpenSSL version.
Set `PQC_BENCH_ORDER=openssl-first` or `circl-first` to rotate the selected
backend to the front (the default is `rustcrypto-first`). The selected backend
must be compiled in; the benchmark also works with RustCrypto alone. Criterion CLI options can extend the sample count and
measurement duration; the benchmark report includes the two-pass long-run commands.
These are single-threaded, repeated-input microbenchmarks with warm caches, not
node throughput, diverse-key traffic, signing performance, or cold-start latency.
Short-input results measure the common Rust wrapper rather than cryptography.
Use a quiet machine and compare ARM64 and x86_64 before changing gas prices.

The [Apple M5 benchmark report](benchmarks/pqc-backends-apple-m5.md) contains
measured results, confidence intervals, limitations, and saved raw samples.

## Dependencies

- `ml-dsa = 0.1.2` (minimum Rust 1.87)
- `slh-dsa = 0.2.0-rc.5` (minimum Rust 1.85; release candidate)
- `revm-precompile = 43.0.3`, matching the node's resolved REVM precompile version

The earlier `slh-dsa 0.1.0` cannot resolve alongside this workspace: it requires
`signature 2.3.0-pre.*`, while existing ECDSA dependencies use `signature 2.2`.
The selected SLH release uses the `signature 3` family and resolves alongside
the existing dependencies without upgrading them. Default crypto features are
disabled; the verifier does not need OS randomness or secret keys.

Both RustCrypto implementations document that they have not been independently
audited. The release candidate and its transitive dependencies need review before
production use. Sources:
[ML-DSA](https://docs.rs/ml-dsa/0.1.2/ml_dsa/),
[SLH-DSA](https://docs.rs/slh-dsa/0.2.0-rc.5/slh_dsa/).

## Validation

Run from the repository root:

```sh
cargo test -p bsc-pqc-precompiles --locked
cargo clippy -p bsc-pqc-precompiles --all-targets --locked -- -D warnings
cargo +1.99.0 check -p reth_bsc --lib --no-default-features --features experimental-pqc --locked
```

The node's existing dependencies require Rust 1.95 or newer; the standalone PQ
crate also builds with the locally installed Rust 1.94.0.

The original RustCrypto-only configuration passed five integration tests, Clippy,
and the full `reth_bsc` library check on macOS ARM64 with Rust 1.99.0.
The OpenSSL configuration runs the same behavior tests on both backends, plus
512 deterministic mutations, noncanonical ML-DSA hint encodings (including
duplicate indices), and parallel verification through a registered OpenSSL
backend. These checks now also cover CIRCL, including all three registered backends
under concurrent Rust-thread calls and direct FFI argument guards. The node binary,
mixed-client networks, and ERC-4337 contract flows
have not been exercised yet.

The Rust tests consume fixtures generated independently by Cloudflare CIRCL
v1.6.5, including signatures with an incompatible context. Tests cover both
algorithms, modified messages/signatures/keys, invalid encodings, exact input
lengths, gas boundaries, output encoding, reservoir preservation, and explicit
registration with atomic conflict handling. They are interoperability checks for RustCrypto/OpenSSL and regression checks
for CIRCL itself, not Go BSC consensus parity tests.

To regenerate the fixtures:

```sh
cd crates/pqc-precompiles/tests/fixtures/generate
go run .
```

The generator uses public deterministic test seeds (`0x42` repeated to the
scheme's seed length), a 32-byte `0x37` message, and either an empty context or
`reth-bsc-test-context`. It verifies every signature with CIRCL before writing
the complete precompile input. Go is needed for regeneration and for builds enabling the CIRCL backend.

## Remaining deployment work

Agree on addresses, input/output and failure semantics, signing context, and a
future activation rule with the BSC client team. Then wire registration through
the node's hardfork/tracing path, run mixed-client execution and ERC-4337 wallet
tests, and benchmark valid/invalid verification on ARM64 and x86_64 to review gas
pricing and denial-of-service limits. Current network behavior is unchanged.
