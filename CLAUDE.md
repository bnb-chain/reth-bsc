# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

reth-bsc is a BSC (BNB Smart Chain) compatible Ethereum client. It is **not a fork of reth** but an extension using reth's `NodeBuilder` API and modular architecture. All `reth-*` crate dependencies come from `https://github.com/bnb-chain/reth.git` (not `paradigmxyz/reth`).

Rust toolchain is pinned to **1.91.0** via `rust-toolchain.toml`.

## Build Commands

```bash
# Debug build
cargo build

# Release build (recommended)
make build    # cargo build --bin reth-bsc --features "jemalloc,asm-keccak" --profile "release"

# Maximum performance build
make maxperf  # lto=fat, 1 codegen unit, -C target-cpu=native
```

## Testing

```bash
# Unit/integration tests
cargo test --all -- --test-threads=1

# Ethereum Execution Spec Tests (download fixtures first)
make download-eest
make ef-tests
make ef-tests-nextest  # faster parallel execution (requires cargo-nextest)
```

## Linting and Formatting

```bash
# Clippy (CI runs with RUSTFLAGS="-D warnings")
cargo clippy --workspace --tests --all-features

# Format
cargo fmt --all

# Unused dependency check (requires nightly)
cargo +nightly udeps --workspace --lib --examples --tests --benches --all-features --locked
```

Linux build dependencies: `liburing-dev`, `pkg-config`, `libclang-dev`.

## Architecture

Entry point: `src/main.rs` → instantiates `BscNode` via reth's `Cli::run_with_components`.

### Key Modules

| Module | Purpose |
|---|---|
| `chainspec/` | `BscChainSpec`, chain definitions (mainnet, chapel testnet, rialto QA-net), genesis JSON files |
| `hardforks/` | `BscHardfork` enum (Frontier → Mendel), `BscHardforks` trait with per-hardfork activation helpers |
| `consensus/parlia/` | Parlia PoSA consensus engine: snapshots, validator sets, BLS voting, fork choice rules |
| `consensus/eip4844/` | BSC-specific EIP-4844/blob gas logic |
| `evm/api/` | `BscContext`, `BscEvm` wrappers around revm |
| `evm/precompiles/` | BSC custom precompiles (BLS, CometBFT, double-sign, IAVL, Tendermint) |
| `node/mod.rs` | `BscNode` implementing reth's `Node` trait, component builder composition |
| `node/evm/` | `BscEvmConfig`, `BscBlockExecutorFactory`, pre/post execution hooks |
| `node/network/` | P2P stack: BSC sub-protocols (bsc/1, bsc/2), block import, EVN (Enhanced Validator Network), vote propagation |
| `node/miner/` | Block production: mining loop, payload assembly, MEV bid simulation |
| `node/engine_api/` | BSC engine API types and validators |
| `rpc/` | Custom JSON-RPC: `parlia_getSnapshot`, MEV API, blob API |
| `system_contracts/` | On-chain system contract ABIs and per-hardfork bytecode (auto-generated at build time by `build.rs`) |
| `shared.rs` | Global singletons (`OnceLock`/static) for snapshot provider, network handle, block import senders, etc. |

### Component Wiring

```
BscNode
  ├── BscConsensusBuilder → Parlia consensus engine
  ├── BscExecutorBuilder  → BscEvmConfig → BscEvm + precompiles
  ├── BscNetworkBuilder   → BscBlockImport, BSC sub-protocols, EVN
  ├── BscPoolBuilder      → transaction pool
  ├── BscPayloadServiceBuilder → payload service
  └── BscNodeAddOns       → RPC extensions (parlia, mev, blob)
```

## Important Build-Time Codegen

`build.rs` auto-generates `src/system_contracts/embedded_contracts.rs` from hardfork contract hex files in `src/system_contracts/{mainnet,chapel,rialto}/`. **Do not edit that generated file manually.**

## Code Conventions

- Error handling: `eyre` throughout
- Logging: `tracing` with target strings like `"bsc::net"`, `"bsc::evn"`, `"reth::cli"`
- Sync primitives: `parking_lot::Mutex` for sync code, `tokio::sync::Mutex` for async
- `derive_more` used extensively for `Deref`/`DerefMut`
- Formatting: see `rustfmt.toml` — notably `imports_granularity = "Crate"`, `use_small_heuristics = "Max"`
