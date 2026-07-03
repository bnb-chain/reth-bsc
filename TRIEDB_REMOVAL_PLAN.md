# Plan: Complete Removal of the Trie DB (`reth-trie-db` and Merkle-trie maintenance)

> **Status (2026-07-03): Phase 1 implemented.** Decisions taken: locally-built
> blocks carry `B256::ZERO` as state root (BEP-675 BidBlocks keep the
> builder-supplied root); fastnode mode is force-enabled in `main.rs` for the
> `node` command; test oracles moved from root equality to hashed-post-state /
> fixture `postState` comparison; `reth-trie-db` and `reth-trie` removed from
> both Cargo.tomls (`reth-trie-common` remains for upstream trait signatures).
> Phases 2–4 (bnb-chain/reth fork changes — unwind trie-revert gating, genesis
> trie writes, table drop/migration — and sync/reorg validation) are open.

Scope: eliminate all Merkle-trie work from `reth-bsc-zerosim` — state-root
computation, `AccountsTrie`/`StoragesTrie` table maintenance, `TrieUpdates`
persistence, and the direct `reth-trie-db` / `reth-trie-common` dependencies —
for both the import (engine-tree) path and the miner path.

Baseline facts this plan is built on (verified against the pinned fork
`bnb-chain/reth @ 0dea17d` and this repo):

- The fork **already ships a "fastnode" mode**:
  `--engine.skip-state-root-validation` (`crates/node/core/src/args/engine.rs:436`)
  sets `TreeConfig::skip_state_root_validation`
  (`crates/engine/primitives/src/config.rs:259`) and calls
  `activate_fastnode()`. Under it, live-sync uses the header's declared root
  and attaches `TrieUpdates::default()`
  (`crates/engine/tree/src/tree/payload_validator.rs`, the
  `skip_state_root_validation()` branch), the pipeline drops
  `AccountHashingStage`/`StorageHashingStage`/`MerkleStage` via
  `disable_hashing_stages` (`crates/stages/stages/src/sets.rs:348`,
  `crates/node/builder/src/launch/common.rs:178`), and `eth_getProof` /
  `eth_getAccount` return `MethodNotAvailable`
  (`crates/rpc/rpc-eth-api/src/helpers/state.rs:171`).
- Under **storage v2** (the default, `StorageSettings::base()` →
  `storage_v2 = true`), `HashedAccounts`/`HashedStorages` are the **canonical
  state representation** (`crates/storage/db-api/src/models/metadata.rs:85`),
  not trie-support tables. They must stay. "Trie removal" therefore means:
  state-root computation, `AccountsTrie`/`StoragesTrie`, `TrieUpdates`, and
  the `reth-trie-*` API surface — not the hashed-state tables.
- This repo's own trie surface is concentrated in the **miner sparse-trie
  state-root path** plus the ef-test harnesses (inventory in Phase 1).

---

## Phase 0 — Decisions to lock first

These are protocol/product decisions; the code plan branches on them.

1. **What goes in the `state_root` header field of locally-built blocks?**
   A node that maintains no trie cannot compute a root for blocks it mines.
   Options:
   - **(a) Non-producing node only** — trie removal applies to sync/replica/
     simulation nodes; a producing validator keeps the trie (build two
     profiles, see Phase 3).
   - **(b) BEP-675-style delegation** — all produced blocks come from builder
     `BidBlock`s that carry the builder-computed root; local fallback building
     is disabled or emits a placeholder per (c).
   - **(c) Protocol change** — the chain stops verifying `state_root`
     (e.g. carries `B256::ZERO`, the parent root, or a deferred root). This
     requires a BEP and coordinated rollout with go-bsc; until then any block
     produced without a real root is rejected by every other client.
2. **Trust model for imports.** With no local root verification the node
   accepts whatever root the producer put in the header. Acceptable for
   zerosim/replica nodes; must be explicitly signed off for anything
   validator-adjacent.
3. **Genesis root**: computed from the alloc today
   (`crates/storage/db-common/src/init.rs:252`) and it feeds the genesis
   *hash*, which must keep matching mainnet/chapel. Decide: keep the one-time
   genesis computation (cheap, happens once — recommended) or hardcode the
   known roots per chain in the chainspec and fork `init.rs`.
4. **Definition of "complete".** Recommended target: *zero trie work at
   runtime and no trie tables on disk*. Excising the `reth-trie` crates from
   the upstream build graph entirely (engine-tree, provider, stages all link
   them) would mean gutting the fork for no runtime benefit — recommend
   explicitly declaring that out of scope.

---

## Phase 1 — Downstream removal (this repo)

All changes here are in `reth-bsc-zerosim` and deletions are self-contained.

### 1.1 Rip out the miner sparse-trie state-root machinery

| Site | What to remove |
|---|---|
| `src/node/engine.rs:116-291` | Entire `use_sparse_trie_state_root` block: `PayloadProcessor` construction, `ChangesetCache` (`reth_trie_db`), `OverlayStateProviderFactory`, `spawn_state_root`, `set_sparse_trie_spawn_fn` registration. |
| `src/shared.rs:70-77, 323-346` | `SparseTrieSpawnFn` type, `SPARSE_TRIE_SPAWN_FN` global, `set_sparse_trie_spawn_fn`, `spawn_sparse_trie_state_root`. |
| `src/node/miner/payload.rs:52, 354-387, 477-531` | `ChangesetCache` import, `state_root_precomputed` sink, `trie_handle`, `state_root_deadline_ms`, the conditional spawn. |
| `src/node/evm/config.rs:50-51, 72-90, 161-176` | `StateRootPrecomputedSink` alias; `state_root_precomputed_sink` / `trie_handle` / `state_root_deadline_ms` fields on `BscNextBlockEnvAttributes` and `BscBlockExecutionCtx`. |
| `src/node/evm/builder.rs:19, 42-86, 137-310` | `TrieUpdates` import, `precomputed_state_root` field + `with_precomputed_state_root`, and the whole `finish()` root-selection logic (sparse-trie wait, precomputed fast path, and the blocking `state_root_with_updates` fallback at line ~309). |
| `src/node/miner/config.rs:52-61, 112, 234, 325, 352` + `src/main.rs:48, 233-234` | `use_sparse_trie_state_root` flag, `BSC_MINING_USE_SPARSE_TRIE_STATE_ROOT` env var, CLI plumbing. |
| `src/node/evm/executor.rs:690` | `set_state_hook` / `OnStateHook` support **if** its only consumer is the sparse-trie diff stream (verify before deleting — prewarming may also use it). |
| `src/rpc/debug_builder.rs:206-208` | Drop the now-deleted ctx fields. |

Replacement in `builder.rs::finish()`: per the Phase 0.1 decision, set the
header root to the chosen placeholder (or the builder-supplied root on the
BidBlock path) and attach `TrieUpdates::default()` / empty hashed state to the
built payload so persistence has nothing trie-shaped to write.

### 1.2 Engine/import side

- `src/node/engine_api/validator.rs:175-181` —
  `validate_block_post_execution_with_hashed_state` is already a no-op. The
  `HashedPostState` type in its signature comes from the upstream trait, so
  the *transitive* dependency on `reth-trie-common` remains; only the direct
  Cargo entry can go (see 1.4).
- **Hard-enable fastnode** rather than relying on operators passing
  `--engine.skip-state-root-validation`: in `main.rs`, force the flag (or call
  `reth_engine_primitives::activate_fastnode()` + set
  `stages.disable_hashing_stages`) before launch, and log it loudly. A zerosim
  binary that silently falls back to full trie mode if a flag is forgotten
  defeats the point.

### 1.3 Tests (`testing/bsc-ef-tests`)

- `src/cases/blockchain_test.rs:31-256` — recomputes the root for every block
  (`StateRoot::overlay_root_with_updates`) and asserts against the fixture
  header. Without a trie this oracle is gone. Replace with bundle-state
  comparison: assert the post-state (accounts/storage/balances from the
  `ExecutionOutcome`) against the fixture's `postState` section instead of the
  root hash. Fixtures that only provide `postStateHash` lose coverage — gate
  those behind a `trie` feature or skip-list them explicitly, don't silently
  pass.
- `tests/bid_block_harness.rs:285-402` — the round-trip
  build→finalize→re-execute root equality (`assert_eq!(computed_root,
  reference_root)`) is the zerosim verify-mode invariant. Preserve the
  invariant at the bundle-state level: compare the two `BundleState`s
  byte-for-byte instead of their roots (stronger, and trie-free). The
  `state_root != B256::ZERO` assertion follows the Phase 0.1 decision.
- Drop `reth-trie` / `reth-trie-db` from `testing/bsc-ef-tests/Cargo.toml:39-40`.

### 1.4 Cargo

- Root `Cargo.toml`: remove line 70 (`reth-trie-db`) and the
  `"reth-trie-db/serde"` feature (line 205). `reth-trie-common` (line 69) can
  go only if nothing needs its types after 1.1/1.2 — likely it stays for the
  validator trait signature; if so, keep it and note why.
- Run `cargo +nightly udeps` to confirm nothing else regresses.

**Exit criteria for Phase 1:** no `reth_trie_db` references in the workspace;
node runs in forced-fastnode mode; miner builds blocks without any root
computation (per the Phase 0 decision); full test suite green with the new
bundle-state oracles.

---

## Phase 2 — Fork changes (`bnb-chain/reth`)

Fastnode *skips* trie work but leaves residue. To make removal complete:

1. **Unwind trie reverts** — `unwind_trie_state_from`
   (`crates/storage/provider/src/providers/database/provider.rs:867-898`)
   unconditionally computes changeset-derived trie reverts and writes them via
   `write_trie_updates_sorted`, and also unwinds account/storage *hashing* —
   on a fastnode DB with empty trie tables this is wasted work writing
   garbage. Gate the trie-revert portion (and the `MerkleStage`-style root
   validation it documents) on `is_fastnode_active()` /
   `skip_state_root_validation`. This is the main correctness item: **deep
   reorg/unwind on a trie-less datadir is currently untested territory.**
2. **Persistence** — `save_blocks` already writes `TrieUpdates::default()`
   under fastnode (no-op in practice); add an explicit skip of
   `write_trie_updates_sorted` when fastnode is active so the invariant is
   structural, not accidental.
3. **Genesis init** — per Phase 0.3: either leave the one-time computation
   (recommended; it also populates nothing ongoing) or add a
   chainspec-supplied `genesis_state_root` that bypasses
   `compute_state_root()` in `crates/storage/db-common/src/init.rs` (which
   currently also writes trie tables at lines ~835/855 — skip those writes
   under fastnode either way, so a fresh datadir starts with *empty*
   `AccountsTrie`/`StoragesTrie`).
4. **DB surface** — stop creating `AccountsTrie`/`StoragesTrie` on
   `init_db` for fastnode nodes, and provide a one-shot migration
   (`reth db drop`-style) to reclaim space on existing datadirs. Keep
   `HashedAccounts`/`HashedStorages` — canonical state under storage v2.
5. **RPC audit** — `eth_getProof`/`eth_getAccount` are already gated; sweep
   for remaining proof/witness surfaces (`debug_executionWitness`, invalid-
   block witness hook in `crates/engine/invalid-block-hooks/src/witness.rs`)
   and gate them the same way.

Sequencing note: land Phase 1 first (it's independent), then Phase 2 items
1–2 (safety), then 3–4 (disk/cleanliness).

---

## Phase 3 — Producer story (only if this node mines)

Per Phase 0.1:

- **(a) Two profiles**: a `trie` cargo feature (default on for the validator
  build) that keeps the current `builder.rs::finish()` root computation, and
  the zerosim build with it compiled out. The Phase 1.1 deletions become
  feature-gated instead of removed.
- **(b) BEP-675 delegation**: BidBlock path already avoids local execution
  before broadcast; extend so the *fallback* local build is either disabled
  (miner refuses to run without bids) or produces the placeholder root —
  which is only consensus-valid after (c).
- **(c) Protocol change**: track the BEP; until go-bsc stops verifying roots,
  any placeholder-root block is invalid to the rest of the network. Keep this
  behind an explicit, dangerous-sounding flag.

---

## Phase 4 — Validation & rollout

1. **Sync test**: fresh datadir, forced fastnode, sync a chapel + mainnet
   segment; assert `AccountsTrie`/`StoragesTrie` stay empty and hashed state
   matches a control node's.
2. **Reorg/unwind test**: drive multi-block reorgs (and a pipeline unwind via
   `reth stage unwind`) on the trie-less datadir — exercises Phase 2.1.
3. **ef-tests**: full run with the bundle-state oracle; keep a CI job matrix
   entry running the old root-checking oracle on a `trie`-featured build for
   as long as Phase 3(a) exists.
4. **RPC sweep**: scripted check that `eth_getProof`/`eth_getAccount`/witness
   endpoints return clean `MethodNotAvailable`, everything else unchanged.
5. **Perf/disk report**: block-import latency (state-root wait was the
   dominant cost), miner build latency without the sparse-trie wait, and disk
   delta from dropped trie tables.

## Key risks

- **Blind trust in header roots** (Phase 0.2) — by design, but must be a
  conscious, documented property of the zerosim binary.
- **Unwind on trie-less datadirs** (Phase 2.1) — the one place fastnode mode
  as shipped can still touch trie machinery; do not skip the reorg tests.
- **Losing the strongest correctness oracle**: state-root equality catches
  any state divergence in one assert. Bundle-state comparison is a good
  replacement but only as strong as its field coverage — make it exhaustive
  (nonce, balance, code hash, every storage slot, selfdestructs).
- **Ecosystem breakage**: anything downstream calling `eth_getProof` against
  this node (bridges, light clients, some indexers) breaks. Inventory
  consumers before rollout.
