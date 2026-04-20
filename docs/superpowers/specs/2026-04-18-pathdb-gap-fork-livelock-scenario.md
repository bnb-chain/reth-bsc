# Pathdb-Gap Fork Livelock: Scenario Specification

**Status**: Problem scoped. Solution deferred to a follow-up spec.
**Related**:
- `2026-04-10-triedb-liveness-correctness-design.md` (introduced the "Disconnected on pathdb gap" guard that this scenario exercises)
- `2026-04-17-triedb-mdbx-startup-alignment-design.md` (documents the one-way alignment invariant that rules out a manual unwind remedy)
- `2026-04-17-fix-p2p-livelock-design.md` (fixes a *different* livelock — peer-gossip silence — also caused by validators stuck in fork state)

This spec describes **when and how** the bug manifests. It does not prescribe a fix.

---

## Background

reth-bsc runs with TrieDB (path-based state scheme, `rust_eth_triedb`) as the state backend. TrieDB is structured as:

- a single **disk layer** in RocksDB (one state-root "frozen" on disk);
- an in-memory stack of **diff layers**, one per post-disk block, each describing the state delta from the previous layer;
- state queries walk diff layers from newest to oldest, falling back to the disk layer when the target root is reached.

When a block arrives whose parent state is on the disk layer *and* no diff layer is present, the parent state is directly served by the disk layer. When a block arrives whose parent state is somewhere in the diff-layer stack, it is served from memory. When a block arrives whose parent state is **neither on the disk layer nor reachable through any held diff layer**, reth-bsc's engine-tree treats it as a "pathdb gap" — the block is buffered as `Disconnected` and `PayloadStatusEnum::Syncing` is returned to the caller (`reth/crates/engine/tree/src/tree/mod.rs:2826-2859`; guard introduced in `2026-04-10-triedb-liveness-correctness-design.md`).

That guard is explicitly designed to protect correctness: executing a block without verifiable parent state would let unverified data reach pathdb. The guard is therefore correct. The scenario below is what happens when the guard fires in a pattern the existing recovery paths cannot resolve.

Parallel background: BSC's Parlia consensus forbids a validator from signing two blocks within `validators_count / 2 + 1` slots — the "recent-signer" rule. In a `N=2` network the window is 2, so each validator must wait for the other one to sign before signing again.

---

## Problem Statement

In a small-validator-count BSC network (empirically observed on `N=2` qanet, theoretically applicable to any `N` where recent-signer can deadlock), the following combination produces a **permanent livelock** that no runtime code path will resolve:

1. At least one validator starts with its pathdb disk layer **exactly at** its own canonical tip, with **zero** diff layers retained.
2. Peers present a competing fork whose common ancestor lies strictly **below** that validator's disk-layer height.
3. Parlia recent-signer rule is saturated on every live validator.

When the three conditions hold simultaneously, the node cannot import the competing fork (pathdb gap), cannot rewind its own state (pathdb has no journal, no reverse-diff, no in-process unwind path usable on demand), and cannot produce a new block to break the tie. No existing code path escapes this state.

This spec formalizes the preconditions, event sequence, observable signals, and failure modes so a follow-up spec can reason about remediation.

---

## Glossary (this document)

- **Disk layer / pathdb persist root**: the single state root RocksDB currently stores; returned by `triedb.latest_persist_state()`.
- **Diff layer**: an in-memory delta on top of the disk layer, indexed by resulting state root; owned by engine-tree `TreeState`.
- **Common ancestor**: the deepest block present on two forks with identical hash.
- **Fork blocks**: blocks from peer's chain above the common ancestor, returned by `discover_fork_blocks` (`src/node/network/block_import/fork_recover.rs:144`).
- **Phase-2 first block**: the block at (common-ancestor-height + 1) that `recover_ancestors` imports first via `engine.new_payload`.
- **Recent-signer rule**: Parlia's constraint that a validator cannot re-sign within `N/2+1` blocks.
- **Clean restart**: process exit via `SIGTERM` or equivalent, not a crash; pathdb's `flush()` completed before exit.

---

## Preconditions (all required)

### P1. Divergent canonical chains exist

Two nodes (call them `node_A` and `node_B`) hold different canonical chains:

```
          ...→ H₀                     common ancestor, both agree
                │
                ├── Aₖ₊₁ → ... → Aₖ₊ₘ   node_A canonical (m blocks, m ≥ 1)
                │
                └── Bₖ₊₁ → ... → Bₖ₊ₙ   node_B canonical (n blocks, n ≥ 1)

          hash(Aₖ₊₁) ≠ hash(Bₖ₊₁)
```

A common ancestor at height H₀ exists and is ≤ MAX_FORK_DEPTH deep from both tips (`fork_recover.rs:26`, `MAX_FORK_DEPTH = 2048`).

### P2. node_B's pathdb cannot serve state for H₀

Equivalent formulation: `node_B.pathdb.persist_root ≠ H₀.state_root` AND `node_B.tree_state.merged_difflayer_by_hash(H₀.hash) == None`.

Concretely this holds when:

- node_B was restarted cleanly after sealing Bₖ₊ₙ, so pathdb flushed to (Bₖ₊ₙ).state_root;
- post-restart, no diff layers have been produced yet (fresh process);
- the guard at `reth/crates/engine/tree/src/tree/mod.rs:2842-2858` will therefore fire for any block whose parent is on the `A` fork at height ≤ Bₖ₊ₙ.

Observation: this is *the default post-restart state* of any triedb-backed validator. It is not an unusual state. What makes it load-bearing is combining it with P1.

### P3. Recent-signer rule saturates producer-side slots

Neither validator can produce the next block on its own canonical chain:

- node_A's miner: "Skip to mine new block due to signed recently, tip: Aₖ₊ₘ" (from `src/node/miner/bsc_miner.rs`).
- node_B's miner: "Skip to mine new block due to signed recently, tip: Bₖ₊ₙ".

On `N=2` networks this is automatic any time a validator has just signed a block — the next slot belongs to the other validator. On `N>2` networks P3 requires a more specific alignment, but the guard logic is the same.

---

## Triggering Sequence

A minimum-viable reproduction runs through the following events. The reference timeline was observed on `bsc-qanet`, Apr 18 2026, commit `139b720`.

### T0. Pre-restart state establishes divergence

A two-validator qanet network is running normally; both validators hold an identical canonical chain up to height H₀. Either (a) a network partition or (b) a staggered shutdown lets one validator (`node_A`) see no peers and take its own chain forward by `m` blocks, while `node_B` is offline. At the end of this phase:

- node_A canonical = `Aₖ₊ₘ`; pathdb flushed at `Aₖ₊ₘ.state_root`.
- node_B canonical = `Bₖ₊ₙ` from before the partition; pathdb flushed at `Bₖ₊ₙ.state_root`.

The two chains share `H₀` as the last common block. Note the partition is *not* required; any two staggered restarts of a freshly-divided network achieve the same outcome.

### T1. Both validators restart cleanly

Each process exits via `SIGTERM`; pathdb `flush()` completes; mdbx commits. Both processes start up.

At startup each node emits one line (proof of P2):

```
INFO reth::cli: Startup alignment: backends already in sync
               mdbx_tip=<tip> pathdb_block=<tip> gap=0 outcome="noop"
```

The `gap=0` outcome is *the expected path* — startup alignment was designed to only fire when mdbx drifted ahead of pathdb (documented in `2026-04-17-triedb-mdbx-startup-alignment-design.md`). It is not designed to install diff layers from nothing; there is no journal to restore.

### T2. Early-mining bypass triggers additional divergence (optional but common)

`src/node/miner/bsc_miner.rs` grants an off-turn mining grace after `BSC_SYNC_GATE_TIMEOUT` seconds (6s in the observed logs) even before any peer handshakes:

```
WARN bsc::miner: Sync gate timeout reached, allowing mining to break
                 potential all-validators-restart deadlock tip_number=<tip> elapsed_secs=6
DEBUG reth_bsc::node::miner::bsc_miner: Try off-turn mining, validator: ...
```

If node_A's pathdb had a runway (diff layers from a prior in-process session, or disk-layer at a distance from tip), node_A can solo-mine forward several blocks before node_B connects, widening `m`. In the reference trace, node_A produced 10 blocks (19293277 → 19293286) during this window.

This step is not a strict precondition — any way of reaching P1 is sufficient — but the off-turn grace is the trigger most often observed in practice and is the reason two validators can produce mutually-incompatible blocks instead of one waiting for the other.

### T3. Peers connect and exchange heads

Peer discovery / trusted-peers dial resolves; eth Status handshake and BSC subprotocol come up:

```
DEBUG bsc_protocol: Into connection, direction: incoming, peer_id: 0x<…>
DEBUG bsc::block_import: Spawning fork recovery for announced head
                         peer_id=<…> block_hash=<peer_head> block_number=<peer_head_num>
```

Each side sees the other's head (different hash than anything on local canonical chain) and enters `BscBlockImport::spawn_fork_recovery`.

### T4. node_A → B recovery: completes, but does not switch canonical

node_A imports the shorter `B` fork into its tree-state:

```
DEBUG bsc::fork_recover: Phase 1 complete peer=<…> head_hash=<Bₖ₊ₙ.hash>
                         head_num=<Bₖ₊ₙ.num> fork_blocks=n outcome=AncestorFound
DEBUG bsc::fork_recover: Phase 2 ... (each block: Valid)
INFO  bsc::fork_recover: Fork recovery FCU succeeded head_hash=<Bₖ₊ₙ.hash>
                         head_num=<Bₖ₊ₙ.num>
```

Phase-2 succeeds because node_A's pathdb has the state for H₀ (node_A was solo-mining from there, so its diff-layer stack still covers H₀'s root, or its disk layer itself is H₀'s root when `m` ≤ in-memory-depth). The `FCU succeeded` log message reflects only that `forkchoice_updated` returned `Ok(_)` — it does *not* mean canonical changed. engine-tree's canonical-selection keeps `Aₖ₊ₘ` as head because it is longer (higher TD), which is the *correct* behaviour under BSC's selection rules.

The side-effect of T4: node_A now holds the `B` fork as a known side-chain in `TreeState`. This will matter later (see "Observable Signals").

### T5. node_B → A recovery: fails on the first imported block

node_B attempts the symmetric recovery toward `Aₖ₊ₘ`:

```
DEBUG bsc::fork_recover: Phase 1 complete ... fork_blocks=m outcome=AncestorFound
DEBUG engine::tree: received new engine message msg=Request(NewPayload(
                    parent: <H₀.hash>, number: <H₀.num + 1>, hash: <Aₖ₊₁.hash>))
DEBUG engine::tree: Inserting new block into tree ...
DEBUG engine::tree: found canonical state for block in database hash=<H₀.hash> number=<H₀.num>
WARN  engine::tree: Triedb pathdb gap: no difflayers and parent state root
                    diverges from pathdb disk layer — buffering block as Disconnected
                    for sequential P2P recovery
                    parent_state_root=<H₀.state_root>
                    pathdb_persist_root=<Bₖ₊ₙ.state_root>
```

The guard at `reth/crates/engine/tree/src/tree/mod.rs:2842-2858` fires. The block is buffered as `Disconnected`; `on_new_payload` maps this to `PayloadStatusEnum::Syncing` (`reth/crates/engine/tree/src/tree/mod.rs:773-777`).

`recover_ancestors` Phase-2 (`src/node/network/block_import/fork_recover.rs:292-295`) receives `Syncing` and interprets it as "parent failed silently", returning `ImportSyncingMidChain`:

```
WARN bsc::block_import: Fork recovery failed head_hash=<Aₖ₊ₘ.hash> head_num=<Aₖ₊ₘ.num>
                        error=engine new_payload returned Syncing mid-chain for block
                        <H₀.num + 1> (parent should have been Valid)
```

The head is added to `FailedHeadsCooler` with a 30-second cooldown (`FAILED_HEAD_COOLDOWN` at `fork_recover.rs:48`). When node_A re-announces the same head after cooldown expires, T5 repeats identically — same block, same hash, same outcome — because none of the inputs have changed.

### T6. Miners cannot break the tie

Both miners continuously emit:

```
DEBUG reth_bsc::node::miner::bsc_miner: Skip to mine new block due to signed recently,
                                        validator: <addr>, tip: <tip>
```

node_A needs node_B to sign `Aₖ₊ₘ₊₁` before node_A can sign `Aₖ₊ₘ₊₂`. node_B needs node_A to sign `Bₖ₊ₙ₊₁` before node_B can sign `Bₖ₊ₙ₊₂`. Neither can make progress on its own canonical chain, and neither accepts the other's. The system is in a fixed point.

---

## Invariants That Hold During the Livelock

These invariants are mechanically checkable from a running node and are useful both as assertion targets in a regression test and as tripwires in monitoring. Each holds from the moment T5 is reached until an operator intervenes.

| ID | Invariant | Where to observe |
|----|-----------|------------------|
| I1 | `node_B.pathdb.persist_root == Bₖ₊ₙ.state_root` (unchanged since T1) | `triedb.latest_persist_state()` |
| I2 | `node_B.pathdb.persist_root ≠ H₀.state_root` | compare I1 against any `Aₖ₊₁.parent_state_root` in `new_payload` |
| I3 | `node_B.tree_state.merged_difflayer_by_hash(H₀.hash).is_none()` | engine-tree state; mirrored by the "no difflayers" part of the WARN log |
| I4 | `node_B.canonical_head.number == Bₖ₊ₙ.num`; unchanged across at least two `Status` intervals | `reth::cli` Status log |
| I5 | For every fork-recovery attempt on node_B: `fork_blocks.len() == m` and `ImportSyncingMidChain.num == H₀.num + 1` | `fork_recover.rs` Phase-1-complete + Phase-2-fail log pair |
| I6 | `node_A.canonical_head.number == Aₖ₊ₘ.num` (longer chain stays canonical regardless of how many FCUs point at Bₖ₊ₙ) | `reth::cli` Status log |
| I7 | For every fork-recovery attempt on node_A: `outcome=AncestorFound ∧ FCU succeeded` **but** `canonical_head` unchanged | log pair: Phase-1-complete followed by `Fork recovery FCU succeeded` |
| I8 | Miners: `signed_recently_for(validator=self, tip=self.canonical_head) == true` on both sides | miner's `Skip to mine...` log emitted every 3 s |

**Cross-invariant lemma (why I6+I7 are not the bug)**: I6/I7 together describe *correct* BSC/Parlia behaviour — a node must not reorg to a shorter chain just because FCU points there. Any remediation must preserve I6/I7's *correctness*; it is I2+I3 that need to change.

**Degenerate case where invariants relax**: on a network with `N ≥ 3` and a fresh on-turn validator `node_C` that is *not* saturated by recent-signer, I8 relaxes on node_C. node_C produces the next block on whichever chain it holds canonical. If that is the `A` fork, the livelock resolves; if it is the `B` fork, resolution depends on whether node_A can then accept. See "Scenario Variations".

---

## Reproduction Recipe

Deterministic reproduction on a controlled two-validator qanet. Assumes reth-bsc is built from a commit at or after the `2026-04-10-triedb-liveness-correctness-design.md` guard landing.

### Prerequisites

- Two machines (or two datadirs on one machine with distinct ports), each with its own `nodekey`, mining keystore, and BLS keystore.
- Both validators registered in the `bsc-qanet` genesis / validator set (`N=2`).
- A known shared tip `H₀` reached normally — i.e., both nodes have identical canonical up to `H₀` with `H₀.state_root` persisted on both pathdbs.

### Steps

1. **Stop node_B cleanly** (`SIGTERM`; wait for pathdb flush log confirmation).
   - Verify: `Startup alignment: ... mdbx_tip=H₀ pathdb_block=H₀ gap=0` would print on next start.
2. **Let node_A continue alone** for at least `m ≥ 1` blocks. Two sub-paths:
   - Natural path: block interval × sync-gate bypass fires. node_A's miner takes the off-turn grace and produces `m` blocks. This is observable via `WARN bsc::miner: Sync gate timeout reached`.
   - Forced path: submit a local transaction to node_A's RPC so it has a reason to mine, or set `BSC_SYNC_GATE_TIMEOUT=1` in node_A's environment to shorten the grace.
3. **Stop node_A cleanly**. Both pathdbs are now aligned with their own tips; both tips differ.
4. **Start node_B first**, then node_A. Start order is deliberate — the later-started node is the one whose pathdb locks at its tip.
5. **Wait for peer connect** (`Into connection` in `bsc_protocol`).
6. **Observe Fork recovery failed** on at least one node every ~33 seconds. Confirm the five observable signals.

### Parameters that control how deep the livelock gets

- `m` (node_A's solo-mined count): affects Phase-1 `fork_blocks` length on node_B and thus how many blocks node_B has to discover. Does not change the failure mode; Phase-2 still fails on the first block.
- Block interval (`block_interval`): shorter intervals shorten the repro window but do not change behaviour.
- `FAILED_HEAD_COOLDOWN` (currently 30 s): controls retry cadence. Not a causal knob.

### Minimum reproduction

`m = 1, n = 1` suffices: one block of divergence is enough. The reference incident used `m=10, n=3` only because the off-turn grace allowed node_A to run further before peer connect.

### Negative control (must *not* livelock)

Start both validators with an empty datadir and let them form the chain from genesis. `H₀ = 0` (genesis), both pathdbs eventually flush at the same root, and diff layers accumulate on both sides as the chain progresses. Any later reorg of depth ≤ retained diff-layer depth resolves normally. A regression test that fails this control but passes the positive repro confirms the pathdb gap is load-bearing.

---

## Scenario Variations

The core mechanism is generalisable. This section enumerates how the failure mode scales and degrades with different parameters, so a future remediation spec can state its scope precisely.

### V1. Generalisation to `N > 2`

The scenario generalises to any `N` where two conditions coincide:
- **(Fork)** A set of validators `S_A` holds canonical `A`; a disjoint set `S_B` holds canonical `B`; every validator's canonical descends from a shared ancestor `H₀`.
- **(Saturation)** For every validator `v ∈ S_A ∪ S_B`, `recent_signers(tip_of(v's canonical))` includes `v`. Equivalently: no validator on either canonical is eligible to sign the next block on its own chain.

At `N=21` (BSC mainnet), (Saturation) requires that `≥ (N/2+1) = 11` consecutive recent-signer slots on both canonicals are held by validators who *also* happen to be the ones that cannot accept the other fork. This is vanishingly unlikely in practice but not impossible during coordinated mass-restart events.

### V2. Asymmetric pathdb-gap (one side has runway, the other does not)

If node_A retained some diff-layer depth (e.g., it kept running through T3 without a restart) while node_B is the freshly-restarted side with pathdb flush == tip, the failure is one-sided:
- node_A's Phase-2 succeeds for B's fork (as in T4).
- node_B's Phase-2 fails on every retry (as in T5).
- If node_A is also Parlia-saturated, no block production → livelock.
- If node_A is *not* saturated (e.g., `N ≥ 3` with a fresh on-turn validator), node_A produces the next block on its canonical, appending to `A`. node_B remains stuck on `B` forever unless wiped.

### V3. Both sides have pathdb gap (symmetric)

If both node_A and node_B were cleanly restarted post-divergence (both pathdb flushed at their own tips with no diff layers), **neither** can accept the other's fork. Fork recovery fails on both sides. This is strictly worse than the observed incident, where node_A could at least accept B's fork into tree state. No recovery path fires at all; not even the "stash as side chain" fallback.

### V4. Non-clean restart (crash)

If either side crashes (non-clean exit), pathdb disk layer may be behind mdbx, triggering startup alignment (`2026-04-17-triedb-mdbx-startup-alignment-design.md`), which unwinds mdbx down to pathdb. The resulting state is `mdbx_tip == pathdb_block == <some height ≤ tip>` on that side. Two sub-cases:
- `pathdb_block < H₀`: pathdb's persist root might coincidentally match H₀'s parent state or something further back, giving the gap guard room to not fire. Behaviour depends on which state was last flushed. May or may not livelock.
- `pathdb_block > H₀` but `< tip`: same mechanism as the clean-restart case; gap will fire.

V4 is not reliably reproducible because pathdb flush timing is non-deterministic under crash.

### V5. Fork deeper than `MAX_FORK_DEPTH` (2048)

If the canonical chains diverge by more than `MAX_FORK_DEPTH` blocks, `discover_fork_blocks` aborts with `ForkTooDeep` before the pathdb guard is reached. Phase-2 never runs. This is not a separate livelock mode — it is a different, earlier failure. But it shares the same underlying cause (state unavailability at depth), and any remediation that fixes the pathdb gap scenario should be evaluated against this variation too.

### V6. Fork-recovery succeeds but pathdb flushes mid-import

If node_A's pathdb happens to flush during Phase-2 of a fork recovery (e.g., persistence threshold crossed), pathdb's disk layer advances to some mid-fork state while diff layers above it are rebuilt. If a concurrent FCU selects a head below the new disk layer, the pathdb guard can fire on an unexpected block. This is a rare but real race; we did not observe it in the reference incident but it is adjacent and should be tested against.

---

## Why Existing Recovery Paths Do Not Resolve This

Five pre-existing code paths in reth-bsc / upstream reth are designed to handle divergence or missing state. Each is enumerated below together with why it fails here.

### R1. Fork recovery with sequential P2P backfill

Intent: engine-tree returns `Syncing`, the block import service issues `GetBlocksByRange` to backfill missing ancestors.

Why it fails: the "missing ancestor" reported by the `Disconnected` status is `H₀` itself — a block node_B already has in its mdbx/canonical store. Backfill fetches nothing new; on retry, the pathdb guard fires on exactly the same block (reproducible across all 5 cooldown cycles in the observed log). See `fork_recover.rs:292` for the specific rejection.

### R2. Startup alignment (`align_mdbx_to_triedb_at_startup`)

Intent: close a gap where mdbx is ahead of pathdb after a crash, by unwinding mdbx down to pathdb.

Why it fails: alignment is explicitly one-way (`2026-04-17-triedb-mdbx-startup-alignment-design.md:271` — *"Aligning MDBX forward to match a newer TrieDB. Unrecoverable by design"*). In the scenario here `mdbx_tip == pathdb_block`, so alignment is a no-op and does not manufacture diff layers or rewind pathdb.

### R3. Runtime pathdb rewind (`on_remove_blocks_above`)

Intent: when engine-tree drops canonical blocks due to a reorg, reverse-apply MDBX changesets to roll pathdb back to the new canonical tip.

Why it fails:
- The TrieDB pipeline in `reth/crates/node/builder/src/setup.rs:128-144` explicitly `disable(StageId::MerkleUnwind)` and notes *"without merkle execute/unwind/changesets"*. The MDBX changesets this path relies on are not populated.
- The runtime path only fires when engine-tree itself decides to reorg down (setting a lower canonical tip). In the scenario, neither engine-tree ever chooses to reorg: node_A canonical stays at `Aₖ₊ₘ`, node_B canonical stays at `Bₖ₊ₙ`.
- Even if it could fire, it rewinds to the *new canonical tip*, not to an arbitrary ancestor. The scenario requires reaching H₀, which neither canonical selector ever picks.

### R4. `stage unwind to-block <H₀>`

Intent: operator-level manual rewind of the database.

Why it fails: in triedb mode the pipeline's MerkleUnwind stage is disabled (R3). `stage unwind` therefore only rewinds mdbx, leaving pathdb at its persist root. On next startup, alignment (R2) sees mdbx behind pathdb and tries to do the thing it explicitly declines to do; the node will not start. This is documented in `2026-04-17-triedb-mdbx-startup-alignment-design.md:271` and confirmed by reading `setup.rs:128-144`.

### R5. Periodic head-announcement loop (fix from `2026-04-17-fix-p2p-livelock-design.md`)

Intent: keep peers informed of local heads during standby so `NewBlockHashes` traffic continues even when no blocks are being imported.

Why it fails: that fix addresses a *different* livelock — the one where peers never learn of each other's forks at all. In the scenario here, head-announcement works correctly: each side fires fork recovery every 33s. The follow-up work (T5) is what fails. Announcements are load-bearing but not sufficient.

---

## Behaviour Comparison: geth-bsc Under the Same Conditions

geth-bsc does not livelock under identical preconditions P1+P2+P3. This section records the differences so the scope of the underlying correctness gap is visible; it is descriptive only and does not argue for any specific remediation.

### State-access layers available in geth-bsc pathdb

| Capability | geth-bsc pathdb | reth-bsc (`rust_eth_triedb`) |
|------------|-----------------|------------------------------|
| In-memory diff-layer stack | yes (`TriesInMemory`, default 128) | yes (engine-tree `TreeState.merged_difflayer_by_hash`) |
| Dirty-buffer journal persisted on clean shutdown | yes (`triesinmemory.journal`, written in `Close()`, restored in `loadJournal()`) | no |
| State-history / reverse-diff freezer | yes (`StateHistory`, default 90000 blocks on bsc) | no |
| On-demand `pathdb.Rollback(targetRoot)` | yes (replays reverse diffs from freezer) | no |

### What geth-bsc does in T0 → T5

- **T0 → T1 (restart)**: on clean shutdown, geth writes its in-memory diff layer stack to `triesinmemory.journal`. On startup, `loadJournal` restores them verbatim. Post-startup, pathdb disk layer is still at the pre-restart flush point but the diff-layer stack is non-empty; state for H₀ (which was one of the layers) is reachable from the restored stack.
- **T5 (cross-fork import)**: when node_B tries to import `Aₖ₊₁`, geth's pathdb finds H₀'s state via a restored diff layer. The block executes normally. Precondition P2 never holds on geth-bsc after a *clean* restart.
- **On crash**: journal cannot be trusted and is discarded. Geth then relies on the state-history freezer: when a reorg needs a root not on disk and not in memory, `pathdb.Rollback(targetRoot)` replays the stored reverse-diffs from the freezer. In the scenario here, that would roll pathdb back from `Bₖ₊ₙ.state_root` through each reverse-diff down to `H₀.state_root`, then let engine execute forward along the `A` fork.

### Why this matters for scenario scope

The absence of both the journal and the reverse-diff freezer in reth-bsc is not a bug in any single file — it is a structural capability gap compared to the reference implementation. P2 is therefore not a *transient* runtime condition on reth-bsc; it is an **inevitable consequence** of a clean restart on a pathdb-backed validator that last flushed at its own canonical tip. Any future remediation will have to target one of the capability gaps above (journal, reverse-diff, or an equivalent substitute) to eliminate P2; weakening P3 does not resolve V3 (symmetric pathdb gap).

---

## Observable Signals (for monitoring / runbooks)

A node is in the livelock iff *all* of the following hold simultaneously for ≥60 seconds:

1. `INFO reth::cli: Status connected_peers=≥1 latest_block=<N>` — peers present.
2. `latest_block` is unchanged across consecutive Status lines (75-second interval).
3. Repeating every ~33s: `WARN bsc::block_import: Fork recovery failed ... error=engine new_payload returned Syncing mid-chain for block <M> (parent should have been Valid)` with the *same* `head_hash` and `head_num`.
4. At the same cadence: `WARN engine::tree: Triedb pathdb gap: no difflayers and parent state root diverges from pathdb disk layer` with the *same* `parent_state_root` and `pathdb_persist_root`.
5. `DEBUG reth_bsc::node::miner::bsc_miner: Skip to mine new block due to signed recently, ... tip: <same N>` appearing every 3s.

Signals 3+4 together are the primary fingerprint. Signal 5 confirms the miner is not temporarily gated on unrelated reasons.

Counter-signal (livelock self-resolves): either side's `latest_block` advances. This indicates a diff layer became available (e.g., via T2-style off-turn mining on the other side), breaking P2.

---

## Operator Decision Tree (without code change)

When the observable signals above trigger an alert, the on-call must pick a remediation. The choice depends on three questions that can be answered from the logs alone.

```
Q1. Is the alert firing on ONE side or BOTH sides?
    ├── ONE side only   →  follow "one-sided stall" branch
    └── BOTH sides      →  follow "symmetric livelock" branch (Scenario V3)

Q2. On the failing side, what is `m = peer_head_num - local_head_num`?
    - m ≤ MAX_FORK_DEPTH (2048) →  Phase-1 discovery can complete; only
                                  Phase-2 fails. (The observed case.)
    - m  > MAX_FORK_DEPTH       →  Phase-1 itself aborts with
                                  `ForkTooDeep` (Scenario V5). Treat as
                                  "permanently diverged" → resync only.

Q3. Is the failing side a VALIDATOR or a FULLNODE?
    - Fullnode  →  livelock downgrades to one-sided stall; the rest of
                   the network continues. No rush unless this fullnode
                   serves downstream traffic.
    - Validator →  block production is blocked for the network subset
                   dependent on this validator's slot. Higher urgency.
```

Once classified, the viable remediations (from earlier analysis, preserved here as reference — not proposing any code change):

- **Preferred** (least state loss): wipe the failing side's `db/`, `rust_eth_triedb/`, `static_files/`, and `snapshots/` while preserving `geth/nodekey`, `keystore/`, and `bls/`. Restart and resync from the peer that holds the longer canonical.
- **Emergency alternative** (if the failing side holds transactions that must not be lost): rsync a healthy datadir from a peer, then replace only the node-identity files before startup.
- **Do not use**: `stage unwind to-block <H₀>` — leaves pathdb > mdbx, causes unrecoverable startup alignment failure (see R4).

This section describes current operator practice only. A code-level remediation is out of scope for this spec.

---

## Scope of Impact

### Deployments definitely affected

- Small-validator-count BSC networks (observed `N=2` on qanet). The smaller `N`, the more often P3 saturates.
- Any reth-bsc validator restart where exiting canonical tip coincides with pathdb disk layer — the default state after a clean shutdown.

### Deployments likely affected

- Mid-size test networks (`N=3..7`) during coordinated restart or rolling-restart windows.
- Any network where a single validator can be network-partitioned long enough to grow its canonical by more than the attacker-controlled peer's retained diff-layer depth.

### Deployments unlikely to hit in practice

- mainnet/testnet (`N ≥ 21`): P3 requires a large fraction of validators to simultaneously hit recent-signer saturation with divergent canonicals. The diversity of signers makes this rare.
- Fullnodes (no miner): P3 does not apply. The pathdb gap can still manifest (R1 does not clear it), but one side *can* make block-production progress, so the state is a one-sided stall rather than a livelock.

### Impact when it hits

- Permanent block-production stall. Chain health metrics (head latency, tx mempool drain) freeze.
- No data loss on either side — both canonical chains remain internally consistent, just incompatible.
- CPU/disk/network at idle save the 33s fork-recovery retry bursts and the 3s miner skip ticks.
- Recovery requires operator intervention (see "In-place recovery options", previous analysis): wipe one side's state DBs and resync, or rsync a healthy datadir. No unwind path works.

---

## Test Coverage Gaps

The incident slipped past existing tests despite being a straightforward two-validator + clean-restart sequence. This section records why, so any future remediation spec can add targeted coverage rather than broad regressions.

### TCG1. Fork-recovery unit tests do not exercise pathdb

`src/node/network/block_import/fork_recover.rs:452-739` exercises `discover_fork_blocks` and the FCU head resolver against a `FakeProvider` that has no pathdb backing. Every test in that module assumes the engine accepts every block as `Valid`. There is no test where `engine.new_payload` returns `Syncing` on the *first* block in Phase-2; `ImportSyncingMidChain` has no coverage.

### TCG2. engine-tree tests fire the gap guard only in single-node contexts

Tests around `reth/crates/engine/tree/src/tree/mod.rs:2826-2859` (the "Triedb pathdb gap" guard) validate that a single node correctly buffers and resumes via sequential backfill when blocks arrive out of order. They do not cover a two-validator scenario where the "missing ancestor" is a block the local node already has canonically — which is the case here.

### TCG3. Startup alignment tests treat `gap=0` as a passing state

The test suite for `align_mdbx_to_triedb_at_startup` covers the three outcomes `Aligned`, `TriedbAhead`, and `Unrecoverable`. `Aligned` (gap=0, outcome=noop) is treated as success. The scenario here is that `Aligned` at startup is the *prerequisite* for a later livelock; no alignment test links alignment outcome to post-startup reorg capability.

### TCG4. No integration test for "two validators, staggered clean restart"

Integration tests exist for single-node restart, crash recovery, and peer sync. There is no test for the specific operational pattern of this incident: two validators, one restarts while the other produces blocks, then both reconnect. A two-validator docker-compose or `reth-bsc` test harness targeted at this pattern would fail deterministically against the current codebase.

### TCG5. Metrics do not include "days since last successful reorg"

There is no metric answering the question "has this node ever actually exercised its reorg capability since startup?" A node whose pathdb gap is fatal will appear healthy on all standard metrics (head progressing, peers connected) right up until it hits a fork, at which point it collapses. Adding such a metric would shrink the "time to detect" for this class of bug from "never without paging an engineer" to "seconds after first reorg attempt".

Coverage improvements are out of scope for this spec; they belong to whichever future remediation spec owns this area.

---

## What This Spec Intentionally Does Not Cover

1. **Remediation**. No proposed fix, implementation sketch, or migration. Those live in a follow-up spec.
2. **Fork-recovery Phase-2 first-block semantics correction**. The `fork_recover.rs:292` comment *"Sequencing guarantees parents were already Valid"* is imprecise for the first imported block (its parent is the common ancestor, a pre-existing local block, not a block this call-site imported). Tightening that interpretation is a surface-level improvement but does not address the underlying state-availability problem. Out of scope.
3. **Sync-gate timeout tuning**. The 6-second off-turn mining grace (T2) is a trigger multiplier, not a root cause; restricting it to "have ≥1 peer connected" would reduce how often the scenario manifests but not eliminate it. Out of scope.
4. **pathdb journal / reverse-diff design**. Clearly the lever to pull; covered by a follow-up spec.
5. **BSC Parlia consensus behaviour**. Recent-signer is correct by design; no change proposed.

---

## Reference Evidence

- Logs: `start-3.log.1` (node_A, miner `0x5e2A...20f0f`) and `start-3.log.2` (node_B, miner `0xBbD1...049d`), recorded `2026-04-18T12:11:28Z` through `2026-04-18T12:17:53Z`, commit `139b720` on branch `fix-p2p`.
- Divergent chains: common ancestor `H₀ = 19293276`, node_A canonical head `19293286` (`0xc8918158…`), node_B canonical head `19293279` (`0x054d3055…`), `m=10`, `n=3`.
- Parent state root at `H₀`: `0x73c2e5a2eb935090352463458adffb116f696986b3fc692748f882c88f582ab5`.
- node_B pathdb persist root at time of failure: `0x690bbcecbbf680e9e4277b2266d664191b0d080d368643a71956550ec348fe29`.
- 5 consecutive fork-recovery failures on node_B at 33s intervals, all identical.
- 45 consecutive FCU-succeeded events on node_A with no canonical change.

---

## Acceptance of This Spec

This spec is accepted when:

1. A reader unfamiliar with the incident can, from this document alone, reproduce the livelock on a controlled two-validator qanet by following the preconditions and triggering sequence.
2. An operator on-call can use the "Observable Signals" section to classify an incoming alert without reading code.
3. Any future remediation spec can cite preconditions P1/P2/P3 by ID to state which of them its fix eliminates or weakens.
