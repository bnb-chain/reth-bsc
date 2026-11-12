# Building a static musl binary (for old-glibc hosts)

reth v2.5.1 (and its deps `tracing-logfmt` / `tracing-samply`, plus std's
thread-id path) reference the libc symbol `gettid`, which only became a
linkable symbol in **glibc 2.30** (2019). On hosts with an older glibc the
build fails at link time:

```
undefined reference to `gettid'
error: could not compile `reth_bsc`
```

and even a binary built elsewhere against a newer glibc will not *run* there.
This affects **Amazon Linux 2** (glibc 2.26), older CentOS/RHEL 7, etc.

The fix is a **fully static musl build**: musl bundles its own libc (which has
`gettid`), so the binary has no glibc dependency and runs on any Linux,
including glibc 2.26.

## Why a glibc host cross-compiling to musl (and not native Alpine)

Two things pull in opposite directions:

- reth's C/C++ deps (rocksdb, blst, `gmp-mpfr-sys`, mdbx) build most reliably
  with a real toolchain, which pushed toward building natively inside Alpine.
- **But** several `-sys` crates (mdbx, rocksdb) run `bindgen`, which `dlopen`s
  `libclang` at build time. In a native-musl (Alpine) build the *build scripts
  themselves* are static musl binaries, and **musl static binaries cannot
  `dlopen`** — bindgen dies with "Dynamic loading not supported".

The way out is to build on a **glibc host, cross-compiling to musl**:

- Build scripts / proc-macros compile for the glibc host, so bindgen's `dlopen`
  works.
- Only the final `reth-bsc` binary is linked static-musl, so it runs on old
  glibc.

The `messense/rust-musl-cross:x86_64-musl` image is purpose-built for this: a
Debian (glibc) host with a musl cross toolchain (musl-gcc/g++), cmake, etc.
already wired up, defaulting to the `x86_64-unknown-linux-musl` target.

## Quick start

Requires Docker on the build host (the host's own glibc version is irrelevant —
the build happens inside the container):

```bash
make maxperf-musl
```

Output: `target/x86_64-unknown-linux-musl/maxperf/reth-bsc`, a static musl
binary. Verify:

```bash
file target/x86_64-unknown-linux-musl/maxperf/reth-bsc   # ... statically linked
ldd  target/x86_64-unknown-linux-musl/maxperf/reth-bsc   # "not a dynamic executable"
```

Copy that binary to the target host (e.g. Amazon Linux 2) and run it directly —
no runtime dependencies to install.

## What `make maxperf-musl` runs

It uses the same profile and CPU optimizations as `make maxperf`, with prefixed
jemalloc for Rust allocations. It runs inside the musl-cross image, adds a
few build-time packages, and persists the crate/git cache under a repo-local
`.cargo-musl/` so subsequent builds are incremental:

```bash
docker run --rm \
  -v "$PWD":/src -w /src \
  -v "$PWD/.cargo-musl/registry":/root/.cargo/registry \
  -v "$PWD/.cargo-musl/git":/root/.cargo/git \
  messense/rust-musl-cross:x86_64-musl bash -euxc '
    apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev m4 cmake perl golang pkg-config &&
    export PERL_USE_UNSAFE_INC=1 &&
    RUSTFLAGS="-C target-cpu=native" \
      cargo build --bin reth-bsc --profile maxperf \
        --no-default-features --features asm-keccak,jemalloc-prefixed --target x86_64-unknown-linux-musl'
```

The added packages cover reth's C stack: `clang`/`libclang-dev` (bindgen for
rocksdb and mdbx — these run on the glibc host so `dlopen` works), `m4` (GMP's
autotools configure), `cmake` (rocksdb, aws-lc), `perl`+`golang` (aws-lc-sys).

`PERL_USE_UNSAFE_INC=1` supports the cryptogams Perl scripts that `sha3-asm`
(the `asm-keccak` feature) runs.

## Allocator selection

The static musl build uses `jemalloc-prefixed` for Rust allocations, including
Rust dependencies. Native C/C++ allocations retain their existing allocator.
Enabling the regular `jemalloc` feature through
Reth also enables allocator overrides and RocksDB's unprefixed jemalloc
symbols. Those can collide with musl's `libc.a` during linking, producing
`multiple definition` errors for `calloc`, `free`, `realloc`, `aligned_alloc`,
and `posix_memalign`.

Removing `jemalloc` from `--features` alone does not disable it: both this
package and Reth enable it by default. `Cargo.toml` therefore disables Reth's
implicit defaults and explicitly preserves its other default features. This
package's `jemalloc` feature still enables the allocator for normal builds;
`maxperf-musl` uses `--no-default-features --features asm-keccak,jemalloc-prefixed`
to select only the prefixed Rust allocator. Keep default features disabled:
Cargo features are additive, so `jemalloc-prefixed` cannot undo allocator
overrides enabled by `jemalloc`. For a system-allocator comparison, omit
`jemalloc-prefixed` while preserving the other build flags.

You can inspect the resolved features inside the build container with:

```bash
cargo tree -p reth_bsc --target x86_64-unknown-linux-musl \
  --no-default-features --features asm-keccak,jemalloc-prefixed -e features -i tikv-jemalloc-sys
```

The graph must not enable `unprefixed_malloc_on_supported_platforms` or
`override_allocator_on_supported_platforms`. The `tikv-jemalloc-sys` crate
provides prefixed symbols for the Rust global allocator without overriding libc.
The binary's `--version` output reports `jemalloc-prefixed` when selected.

**Do not override `CARGO_HOME`.** The image ships `/root/.cargo/config.toml`
that configures the musl cross compiler as the linker for the target. If you
point `CARGO_HOME` elsewhere, that config is lost, rustc links with the host
`cc` (glibc gcc), and the final link fails with a flood of undefined glibc
symbols (`__libc_single_threaded`, `fopen64`, `__wmemcpy_chk`, …) from the
host's `libstdc++.a` being pulled into a musl binary. That's why the cache is
mounted at `/root/.cargo/{registry,git}` (subdirs) rather than replacing the
whole `CARGO_HOME`.

## The `gmp-mpfr-sys` force-cross dependency

`gmp-mpfr-sys` (pulled by `revm-precompile`) refuses to build when the host and
target triples differ, which is exactly what a glibc→musl cross is. Its
`force-cross` feature allows it. `Cargo.toml` enables that feature **only** for
the musl target:

```toml
[target.x86_64-unknown-linux-musl.dependencies]
gmp-mpfr-sys = { version = "1.7", features = ["force-cross"] }
```

Normal and CI (glibc) builds don't match that target, so they're unaffected.

## Caveats

- **Fully static is required to run on old glibc.** The musl target defaults to
  static; do not pass `-C target-feature=-crt-static` (that yields a *dynamic*
  musl binary needing musl's loader, which glibc hosts don't have).

- **`target-cpu=native`** tunes for the build host's CPU. If you build and run
  on different CPU classes, edit the Makefile target (or the raw command) to use
  e.g. `-C target-cpu=x86-64-v3` to avoid illegal-instruction crashes.

- **jemalloc**: `maxperf-musl` selects `jemalloc-prefixed` as described above.
  Normal `make maxperf` builds retain the regular `jemalloc` feature. Enabling
  that regular feature for musl can reintroduce the duplicate-symbol link failure.

- **Rust version**: the deps require a recent stable. If the image's toolchain
  is too old ("package X requires rustc 1.NN"), add `rustup update` before the
  `cargo build` in the recipe.

- **First build is slow** (compiles GMP, rocksdb, aws-lc, blst, mdbx from source
  under musl). The `.cargo-musl/` cache makes later builds fast.

- **If you move the crate cache** (change `CARGO_HOME`/registry path) with an
  existing `target/`, `sha3-asm` can fail with `can't locate x86_64-xlate.pl`.
  Its OUT_DIR holds a symlink into the old registry path that's now dangling.
  Delete the stale dir and rebuild:
  `rm -rf target/*/maxperf/build/sha3-asm-*`

## Disk / Docker setup on Amazon Linux 2

AL2's Docker is in the extras repo. The third-party `docker-ce-stable` repo
often 404s on AL2 and aborts the transaction, so disable it during install:

```bash
amazon-linux-extras enable docker
yum install -y docker --disablerepo=docker-ce-stable
systemctl enable --now docker || service docker start
```

The build is disk-hungry (crate registry + fat-LTO target ≈ 20–40 GB). If the
root filesystem is small, move **both** Docker's storage and the build output
onto a larger volume:

```bash
# Docker storage -> big volume
systemctl stop docker
mkdir -p /server/docker
printf '{ "data-root": "/server/docker" }\n' > /etc/docker/daemon.json
systemctl start docker
docker info | grep "Docker Root Dir"        # -> /server/docker
```

Keep the repo (and thus its `target/` and `.cargo-musl/`) on the large volume
too, or add extra `-v` mounts to redirect them.
