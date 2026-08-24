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

## Why Alpine, not a gnu→musl cross build

Cross-compiling from a glibc host to `x86_64-unknown-linux-musl` (via
`cargo-zigbuild`, `cross`, or a musl cross toolchain) trips on reth's C/C++
dependencies — `gmp-mpfr-sys` (pulled by `revm-precompile`) outright refuses to
cross-compile, and rocksdb / aws-lc / mdbx-bindgen are fragile under cross
toolchains.

Building **inside Alpine** sidesteps all of it: Alpine's host triple *is*
`x86_64-unknown-linux-musl`, so every C dependency sees a **native** build (not
a cross build) and compiles normally. No `force-cross`, no zig.

## Quick start

Requires Docker on the build host (the host's own glibc version is irrelevant —
the build happens inside the container):

```bash
make maxperf-musl
```

Output: `target/maxperf/reth-bsc`, a static musl binary. Verify:

```bash
file target/maxperf/reth-bsc          # ... statically linked
ldd  target/maxperf/reth-bsc          # "not a dynamic executable"
```

Copy that binary to the target host (e.g. Amazon Linux 2) and run it directly —
no runtime dependencies to install.

## What `make maxperf-musl` runs

It mirrors `make maxperf` (same profile and features) but inside `rust:alpine`,
installing the C toolchain reth needs and pointing `CARGO_HOME` at a repo-local
`.cargo-musl/` cache so subsequent builds are incremental:

```bash
docker run --rm \
  -v "$PWD":/src -w /src \
  -v "$PWD/.cargo-musl":/cargo -e CARGO_HOME=/cargo \
  rust:alpine sh -euxc '
    apk add --no-cache build-base musl-dev linux-headers \
      clang clang-dev llvm-dev cmake make m4 perl go gmp-dev mpfr-dev \
      git bash pkgconf &&
    export LIBCLANG_PATH=/usr/lib &&
    RUSTFLAGS="-C target-cpu=native" \
      cargo build --bin reth-bsc --profile maxperf --features jemalloc,asm-keccak'
```

The apk packages cover reth's C stack: `clang`/`llvm` (bindgen for rocksdb and
mdbx), `cmake` (rocksdb, aws-lc), `perl`+`go` (aws-lc-sys), `m4` +
`gmp-dev`/`mpfr-dev` (`gmp-mpfr-sys` builds GMP from source via autotools),
`build-base` (g++ for rocksdb/blst).

## Caveats

- **Fully static is required to run on old glibc.** Do *not* pass
  `-C target-feature=-crt-static` — that yields a *dynamic* musl binary needing
  musl's loader (`/lib/ld-musl-x86_64.so.1`), which glibc hosts don't have. The
  musl target defaults to static; leave it that way.

- **`target-cpu=native`** tunes for the build host's CPU. If you build and run
  on different CPU classes, replace it with e.g. `-C target-cpu=x86-64-v3` to
  avoid illegal-instruction crashes (`FEATURES`/`RUSTFLAGS` are baked into the
  target, so edit the Makefile target or run the docker command by hand).

- **jemalloc**: if it fails to build under fully-static musl, drop it from the
  feature list (`--features asm-keccak`). musl's own allocator works; you lose a
  little throughput.

- **First build is slow** (15–40 min): it compiles GMP, rocksdb, aws-lc, blst,
  and mdbx from source under musl. The `.cargo-musl/` cache makes later builds
  fast.

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
