# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project shape

`reth-bsc` is **not** a fork of Reth. It is a downstream crate that re-uses Reth's `NodeBuilder` API to assemble a BSC-compatible client. Everything BSC-specific (Parlia consensus, BSC hardforks, system contracts, PoSA mining, MEV/Parlia/Miner RPCs, EVN peer features, BSC precompiles) lives here; generic EL behavior comes from upstream Reth pinned by git `rev` in `Cargo.toml`.

- Workspace members: the root crate (binary `reth-bsc`, library `reth_bsc`) and `testing/bsc-ef-tests` (execution-spec tests harness).
- All `reth-*` deps are pinned to one commit in `Cargo.toml` (currently `bnb-chain/reth` rev `ef46a48…`). If you change the Reth rev, update **every** `reth-*` line — a mismatched rev produces duplicate-crate build failures. The `testing/bsc-ef-tests/Cargo.toml` uses `branch = "develop"` of the same fork; that's intentional but keep it aligned when bumping.
- Triedb backend (`--statedb.triedb`) pulls `rust-eth-triedb*` from `bnb-chain/reth-bsc-triedb` on the `develop` branch.
- `build.rs` scans `src/system_contracts/<hardfork>/{mainnet,chapel,rialto}/*` at build time and emits `src/system_contracts/embedded_contracts.rs` (a `phf_map` keyed as `"<hardfork>_<network>_<contract>"`). It also records the git SHA into `RETH_BSC_GIT_SHA` / `RETH_BSC_GIT_SHA_LONG` used at startup and in the P2P client string. If you add a new hardfork directory with system contracts, add it to the `hardforks` list in `build.rs` so cargo rebuilds when those files change.

## Common commands

Build:
```bash
cargo build                               # debug
cargo build --release                     # plain release
make build                                # release + jemalloc,asm-keccak + target-cpu=native
make maxperf                              # LTO=fat, single codegen-unit release
make bench-test                           # maxperf + `bench-test` feature (exposes engine_forkchoiceUpdatedV1)
```
The `jemalloc` feature is enabled by default and requires `tikv-jemallocator` on Unix; do not build with `--no-default-features` casually.

Lint / hygiene (match CI in `.github/workflows/ci.yml`):
```bash
cargo check
cargo clippy --workspace --tests --all-features     # CI runs with RUSTFLAGS="-D warnings"
cargo +nightly udeps --workspace --lib --examples --tests --benches --all-features --locked
cargo fmt                                           # nightly rustfmt settings in rustfmt.toml
```

Tests:
```bash
cargo test --all -- --test-threads=1                 # CI setting; many tests touch global OnceLocks / env
cargo test -p reth_bsc <test_name>                   # run a single test (substring match)
cargo test -p reth_bsc module::path::test_fn -- --exact --nocapture
```

Execution-spec tests (network-download fixtures):
```bash
make download-eest      # pulls EEST v5.4.0 fixtures into testing/bsc-ef-tests/execution-spec-tests
make ef-tests           # cargo test -p bsc-ef-tests --release --features ef-tests,jemalloc,asm-keccak
make ef-tests-nextest   # same, via cargo-nextest
make clean-eest
```

Run (binary is `reth-bsc`, chain ids `bsc`, `bsc-testnet`; optional `--statedb.triedb`):
```bash
./target/release/reth-bsc node --chain bsc --datadir ./data_dir
./target/release/reth-bsc node --full --chain bsc --datadir ./data_dir            # full node
./target/release/reth-bsc node --chain bsc --datadir ./data_dir --statedb.triedb  # triedb backend
```

## Where the wiring lives

The non-obvious cross-cutting pieces — read these together when anything spans components:

- **`src/main.rs`** is the integration point. It parses `BscCliArgs`, then uses `Cli::<BscChainSpecParser, BscCliArgs>::parse().run_with_components::<BscNode>(...)`. The async closure:
  1. applies the genesis-hash override,
  2. hydrates the global `MiningConfig` (CLI > env > defaults) and loads the signing key from keystore / hex,
  3. initializes the global BLS signer (CLI > env),
  4. builds and stores `EvnConfig` (EVN peer-tx-broadcast policy),
  5. parses proxyed peer IDs,
  6. calls `.extend_rpc_modules(...)` to register BSC-only RPC namespaces (`parlia`, `mev`, `miner`, BSC eth-extensions, blob). It **removes** reth's built-ins `miner_setExtra`/`setGasPrice`/`setGasLimit` and `eth_coinbase` before registering our versions — don't re-add them upstream-style,
  7. sends the beacon engine handle back to the network via a oneshot and stores the engine-API mpsc sender globally.

- **`src/shared.rs`** holds nearly all cross-component globals as `OnceLock`s + a few `RwLock`s: snapshot provider, header/block-number accessors, engine-API sender, network handle, payload-events broadcast, bid-package queue, proxyed peer IDs, IPC client, miner runtime knobs, etc. Components register into these from different phases of startup (consensus module publishes the snapshot provider, then the miner waits up to ~10s for it — see `node::engine::BscPayloadServiceBuilder`). When adding a cross-phase dependency, extend `shared.rs` rather than threading handles through builders.

- **`src/node/mod.rs` → `BscNode`** composes: `BscPoolBuilder` (pool), `BscExecutorBuilder` (EVM), `BscPayloadServiceBuilder` (payload + miner bootstrap), `BscNetworkBuilder`, `BscConsensusBuilder`. `BscNodeAddOns` wires `BscEthApiBuilder` plus engine/payload validators (`src/node/engine_api/`).

- **Consensus (`src/consensus/parlia/`)** is PoA via Parlia: `consensus.rs` is the main engine, `snapshot.rs` maintains validator-set history at epoch boundaries, `provider.rs` exposes `SnapshotProvider` (published to `shared::SNAPSHOT_PROVIDER`), `vote_pool.rs` + `vote.rs` + `bls_signer.rs` implement BLS vote attestation, `forkchoice_rule.rs` is the BSC-specific fork-choice rule, `go_rng.rs` reproduces Go's RNG for validator-order determinism.

- **EVM extensions (`src/evm/` and `src/node/evm/`)**: `src/evm/handler.rs` and `src/evm/precompiles/*` add BSC-specific precompiles (BLS, CometBFT/Tendermint light-client, IAVL, double-sign, `tm_secp256k1`). `src/node/evm/{pre_execution,post_execution}.rs` implement BSC's pre/post-block hooks (system-contract calls for epoch transitions, fee distribution, validator updates). `src/evm/blacklist.rs` is the BSC transaction blacklist.

- **PoSA mining (`src/node/miner/`)**: optional; started by `BscPayloadServiceBuilder` only when `MiningConfig::is_mining_enabled()` is true. Entry point `BscMiner::start` runs the build-loop; `payload.rs` builds blocks, `bid_simulator.rs` runs MEV bid simulation, `signer.rs` wraps the validator signing key, `util.rs` has timing/turn/backoff helpers. The miner waits for the snapshot provider to be published by consensus before starting. Configuration flows: CLI `--mining.*` → env `BSC_MINING_*` / `BSC_PRIVATE_KEY` / `BSC_GAS_LIMIT` / `BSC_MINING_INTERVAL_MS` → `MiningConfig::from_env()` → `set_global_mining_config`. See `MINING.md` and `MINING_QUICKSTART.md` for user-facing docs.

- **Hardforks (`src/hardforks/bsc.rs`)** defines the full BSC hardfork schedule (Ramanujan → Niels → MirrorSync → Bruno → Euler → Gibbs → Nano → Moran → Planck → Luban → Plato → Hertz → Kepler → Feynman → FeynmanFix → HaberFix → Bohr → Pascal → Prague → Lorentz → Maxwell → Fermi …). BSC timestamps are real block timestamps (not EL post-merge seconds-since-unix-epoch) and many system-contract behaviors are keyed to them. **Current mainnet block interval is ~450ms** (post-Maxwell) — see "Things that commonly bite" for why this matters when reasoning about timing. When touching fork gating, check both this file and `src/chainspec/{bsc,bsc_chapel,bsc_rialto}.rs`.

- **Network (`src/node/network/`)**: `bsc_protocol/` is the custom BSC sub-protocol, `handshake.rs` / `upgrade_status.rs` implement BSC's upgrade-status handshake extension (and signal the EVN intent). `evn.rs` + `evn_peers.rs` implement the Enhanced Validator Network behavior (disable incoming tx broadcast from flagged peers; activates only after head-timestamp lag < `BSC_EVN_SYNC_LAG_SECS`, default 30s). `block_import/` is the BSC block-import service (separate `IncomingBlock` vs `IncomingMinedBlock` channels).

- **Chainspec (`src/chainspec/`)**: `BscChainSpec` wraps reth's `ChainSpec`. `genesis_override.rs` lets `--genesis-hash` inject a custom genesis hash (the miner/consensus must treat it consistently — consult both sides before changing). Supported chains: `bsc` (mainnet, genesis.json), `bsc-testnet` / `chapel`, `bsc-rialto`, `local` (local.rs, uses `genesis_local.json` which is gitignored).

- **RPC namespaces (`src/rpc/`)**: `parlia.rs` (validator/snapshot queries), `mev.rs` (bid submission + MEV APIs), `miner.rs` (BSC redefinitions of `miner_set*`), `eth_ext.rs` (`eth_coinbase`, `eth_health`), `blob.rs` (blob fetching from pool/provider). All are merged in `main.rs`'s `.extend_rpc_modules` closure.

## Things that commonly bite

- **Current BSC block interval is ~450ms**, *not* 3s and *not* 1.5s. BSC's block time has shortened across hardforks (Lorentz dropped it to ~1.5s, Maxwell further down); the live mainnet target today is **~0.45s**. Anywhere you reason about TPS, gas-throughput budgets, build/import deadlines, missed-turn / backoff windows, EVN lag thresholds, or "is this slow?" — use **0.45s** as the working budget. Old numbers (3s/1.5s) only show up in historical blocks and pre-Maxwell logic; do not quote them as current.
- **Global `OnceLock`s in `shared.rs`**: tests that instantiate a node or publish into globals must run single-threaded (`--test-threads=1`, as CI does). Don't `expect` on `set_*` globals — every setter returns `Err` if already initialized; real callers log a warning and continue.
- **Pinned Reth rev**: bump every `reth-*` entry in `Cargo.toml` together. The ef-tests crate uses `branch = "develop"`; if you bump the rev, also verify its compatibility with whatever `develop` resolves to that day.
- **Auto-generated file**: `src/system_contracts/embedded_contracts.rs` is written by `build.rs` — don't edit by hand; add/modify under `src/system_contracts/<hardfork>/<network>/` and extend the `hardforks` list in `build.rs` if a new hardfork is introduced.
- **IPC is required**: `main.rs` panics if `--ipc.disable` is set; many BSC features (local mining RPC bridge, engine-API plumbing) assume IPC is available.
- **EVN is off by default, activates late**: even with `--evn.enabled`, behavior is gated on head-timestamp lag (`BSC_EVN_SYNC_LAG_SECS`, default 30s) so it won't kick in mid-sync.
- **CI runs cargo test with `--test-threads=1`**: parallel runs will flake because of the globals above.
