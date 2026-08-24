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

It mirrors `make maxperf` (same profile and features) but inside the musl-cross
image, adds a few build-time packages, and persists the crate/git cache under a
repo-local `.cargo-musl/` so subsequent builds are incremental:

```bash
docker run --rm \
  -v "$PWD":/src -w /src \
  -v "$PWD/.cargo-musl/registry":/root/.cargo/registry \
  -v "$PWD/.cargo-musl/git":/root/.cargo/git \
  messense/rust-musl-cross:x86_64-musl bash -euxc '
    apt-get update && apt-get install -y --no-install-recommends \
      clang libclang-dev m4 cmake perl golang pkg-config &&
    RUSTFLAGS="-C target-cpu=native" \
      cargo build --bin reth-bsc --profile maxperf \
        --features jemalloc,asm-keccak --target x86_64-unknown-linux-musl'
```

The added packages cover reth's C stack: `clang`/`libclang-dev` (bindgen for
rocksdb and mdbx — these run on the glibc host so `dlopen` works), `m4` (GMP's
autotools configure), `cmake` (rocksdb, aws-lc), `perl`+`golang` (aws-lc-sys).

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

- **jemalloc**: if it fails under static musl, drop it from the feature list
  (`--features asm-keccak`). musl's own allocator works; you lose a little
  throughput.

- **Rust version**: the deps require a recent stable. If the image's toolchain
  is too old ("package X requires rustc 1.NN"), add `rustup update` before the
  `cargo build` in the recipe.

- **First build is slow** (compiles GMP, rocksdb, aws-lc, blst, mdbx from source
  under musl). The `.cargo-musl/` cache makes later builds fast.

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
