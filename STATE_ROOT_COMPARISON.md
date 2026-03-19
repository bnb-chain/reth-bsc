# State Root Computation: OLD vs NEW reth Comparison

Comparison of state root computation between:
- **OLD**: `/Users/constbh/.cargo/git/checkouts/reth-8428740b6850f139/012ffc0`
- **NEW**: `/Users/constbh/.cargo/git/checkouts/reth-8428740b6850f139/6b50539`

---

## 1. `validate_block_with_state` – Flow and Data Sources

### OLD (payload_validator.rs, lines 406–566)

**State root computation flow:**
- Uses **ConsistentDbView** for all DB access (line 510–511 when `use_state_root_task`, line 876 in `compute_state_root_parallel`)
- **Trie input** is built upfront via `compute_trie_input()` (lines 516–524, 406–407), including:
  - Revert state: `HashedPostState::from_reverts(provider.tx_ref(), block_number + 1)` (lines 818–821)
  - In-memory blocks: `input.extend_with_blocks(blocks.iter().rev().map(...))` (lines 851–853)
  - Current block: `input.append_ref(hashed_state)` (line 886 in `compute_state_root_parallel`)
- **Parallel path** (when `run_parallel_state_root` but not `use_state_root_task`):
  - State root runs **after** execution
  - Uses `compute_state_root_parallel(persisting_kind, parent_hash, &hashed_state, ctx.state())`
  - Timer: `root_time = Instant::now()` at line 455, before the parallel computation

**Data sources:**
- `ConsistentDbView` + `TrieInput` (revert state + in-memory blocks + current block)
- `TrieInput` holds `nodes` (TrieUpdates), `state` (HashedPostState), and `prefix_sets`

### NEW (payload_validator.rs, lines 383–560)

**State root computation flow:**
- Uses **OverlayStateProviderFactory** for all state/DB access (lines 411–415, 966)
- No `compute_trie_input` – trie input is implicit in the overlay
- **Parallel path** (`StateRootStrategy::Parallel`):
  - `compute_state_root_parallel(overlay_factory.clone(), &hashed_state)` (line 517)
  - Timer: `root_time = Instant::now()` at line 446, before the parallel computation

**Data sources:**
- `OverlayStateProviderFactory` with:
  - `get_parent_lazy_overlay(parent_hash, ctx.state())` – lazy ancestor overlay (lines 419–421)
  - `with_extended_hashed_state_overlay(hashed_state.clone_into_sorted())` – current block (line 966)
- Prefix sets: `hashed_state.construct_prefix_sets().freeze()` (line 964)

---

## 2. `compute_trie_input` – OLD Only

**OLD** (`payload_validator.rs`, lines 1003–850):

```rust
fn compute_trie_input<TP: DBProvider + BlockNumReader>(
    &self,
    persisting_kind: PersistingKind,
    provider: TP,
    parent_hash: B256,
    state: &EngineApiTreeState<N>,
    allocated_trie_input: Option<TrieInput>,
) -> ProviderResult<TrieInput>
```

**Behavior:**
1. Uses `provider.best_block_number()` and `state.tree_state.blocks_by_hash(parent_hash)`
2. If `persisting_kind.is_descendant()`, filters in-memory blocks vs DB tip
3. Builds revert state via `HashedPostState::from_reverts::<KeccakKeyHasher>(provider.tx_ref(), block_number + 1)`
4. Extends with in-memory blocks: `input.extend_with_blocks(blocks.iter().rev().map(|block| (block.hashed_state(), block.trie_updates())))`
5. Returns a `TrieInput` containing `nodes`, `state`, and `prefix_sets`

**NEW:** No equivalent. Ancestor state is provided via `LazyOverlay` and `OverlayStateProviderFactory`.

---

## 3. Proof Workers: ProofTaskManager vs ProofWorkerHandle

### OLD (ProofTaskManager / ProofTaskCtx)

**Locations:**
- `payload_processor/multiproof.rs`, lines 355, 368, 658: `ProofTaskManagerHandle<FactoryTx<Factory>>`
- `payload_processor/mod.rs`, line 176: `ProofTaskCtx::new(...)`, line 182: `ProofTaskManager::new(...)`

**Usage:** State root task path uses `ProofTaskCtx` and `ProofTaskManager` to coordinate proof generation.

### NEW (ProofWorkerHandle)

**Locations:**
- `payload_processor/multiproof.rs`, lines 260, 274, 570: `ProofWorkerHandle`
- `payload_processor/mod.rs`, line 279: `ProofWorkerHandle::new(...)`

**Usage:** Same role, but different API and worker lifecycle.

---

## 4. Trie Input and Prefix Sets

### OLD (TrieInput → ParallelStateRoot)

**`payload_validator.rs`, lines 873–887:**

```rust
let mut input = self.compute_trie_input(
    persisting_kind,
    consistent_view.provider_ro()?,
    parent_hash,
    state,
    None,
)?;
input.append_ref(hashed_state);

ParallelStateRoot::new(consistent_view, input).incremental_root_with_updates()
```

**`trie/parallel/src/root.rs`, lines 90–93:**

```rust
let trie_nodes_sorted = Arc::new(self.input.nodes.into_sorted());
let hashed_state_sorted = Arc::new(self.input.state.into_sorted());
let prefix_sets = self.input.prefix_sets.freeze();
```

- Sorting of `nodes` and `state` happens once at the start
- Shared via `Arc` to all storage root tasks

### NEW (prefix_sets from hashed_state)

**`payload_validator.rs`, lines 963–967:**

```rust
let prefix_sets = hashed_state.construct_prefix_sets().freeze();
let overlay_factory =
    overlay_factory.with_extended_hashed_state_overlay(hashed_state.clone_into_sorted());
ParallelStateRoot::new(overlay_factory, prefix_sets).incremental_root_with_updates()
```

- Prefix sets come only from the current block’s `hashed_state`
- Overlay = parent (via `OverlayStateProviderFactory` / `LazyOverlay`) + current block’s sorted hashed state
- No explicit `TrieInput`

---

## 5. `compute_state_root_parallel` – Implementation

### OLD

**`payload_validator.rs`, lines 868–888:**

```rust
fn compute_state_root_parallel(
    &self,
    persisting_kind: PersistingKind,
    parent_hash: B256,
    hashed_state: &HashedPostState,
    state: &EngineApiTreeState<N>,
) -> Result<(B256, TrieUpdates), ParallelStateRootError> {
    let consistent_view = ConsistentDbView::new_with_latest_tip(self.provider.clone())?;

    let mut input = self.compute_trie_input(
        persisting_kind,
        consistent_view.provider_ro()?,
        parent_hash,
        state,
        None,
    )?;
    input.append_ref(hashed_state);

    ParallelStateRoot::new(consistent_view, input).incremental_root_with_updates()
}
```

**`trie/parallel/src/root.rs`:**
- Constructor: `ParallelStateRoot::new(view: ConsistentDbView<Factory>, input: TrieInput)` (line 57)
- Uses `InMemoryTrieCursorFactory` and `HashedPostStateCursorFactory` with pre-sorted `Arc` data
- Each storage root task: `view.provider_ro()` + cursor factories using shared `trie_nodes_sorted` and `hashed_state_sorted`

### NEW

**`payload_validator.rs`, lines 955–968:**

```rust
fn compute_state_root_parallel(
    &self,
    overlay_factory: OverlayStateProviderFactory<P>,
    hashed_state: &HashedPostState,
) -> Result<(B256, TrieUpdates), ParallelStateRootError> {
    let prefix_sets = hashed_state.construct_prefix_sets().freeze();
    let overlay_factory =
        overlay_factory.with_extended_hashed_state_overlay(hashed_state.clone_into_sorted());
    ParallelStateRoot::new(overlay_factory, prefix_sets).incremental_root_with_updates()
}
```

**`trie/parallel/src/root.rs`:**
- Constructor: `ParallelStateRoot::new(factory: Factory, prefix_sets: TriePrefixSets)` (line 51)
- Each storage root task: `factory.database_provider_ro()` (line 115) – creates an `OverlayStateProvider` with overlay
- No `TrieInput`; overlay is inside each provider

---

## 6. Likely Reasons NEW Is Slower for BSC (~3000 txs)

### A. Per-task provider creation

- **OLD:** Uses `ConsistentDbView` and shared `Arc` overlay data. Each task gets a provider via `view.provider_ro()` but shares `trie_nodes_sorted` and `hashed_state_sorted`.
- **NEW:** Each of ~3000 storage root tasks calls `factory.database_provider_ro()` and gets an `OverlayStateProvider` wrapping a new DB transaction (`overlay.rs`, lines 354–369). For ~3000 modified accounts, that means ~3000 read transactions and ~3000 overlay provider wrappers.

### B. Overlay resolution and `LazyOverlay`

- **NEW:** Uses `OverlayStateProviderFactory` with `LazyOverlay` from `get_parent_lazy_overlay()` for in-memory ancestors.
- First `database_provider_ro()` triggers `resolve_overlays()` (possibly `lazy.as_overlay()`), which can block on deferred trie data.
- When `block_hash` is set, the first caller does `calculate_overlay()` (changeset reverts, `HashedPostStateSorted::from_reverts`, etc.), then caches. Others hit the cache, but lock contention on the cache is possible.
- When `block_hash` is `None`, there is no cache; every call uses `resolve_overlays()` directly (`overlay.rs`, lines 413–416).

### C. `with_extended_hashed_state_overlay` and Lazy

- For `OverlaySource::Lazy`, `with_extended_hashed_state_overlay` resolves the lazy overlay and merges with the block’s hashed state (`overlay.rs`, lines 174–177):
  ```rust
  let (trie, mut state) = lazy.as_overlay();  // May block
  Arc::make_mut(&mut state).extend_ref_and_sort(&other);  // Sort of large combined state
  ```
- For BSC with many in-memory ancestors, `extend_ref_and_sort` merges large structures and can dominate cost.

### D. Different run-time conditions

- **OLD:** `run_parallel_state_root` and `use_state_root_task` depend on `persisting_kind` and `has_ancestors_with_missing_trie_updates` (lines 479–394).
- **NEW:** `plan_state_root_computation()` only checks config (e.g. `use_state_root_task()`), so the parallel path may be chosen more often, even when the NEW implementation is slower than the OLD one in those cases.

### E. Single-pass sorting in OLD

- **OLD:** `TrieInput.nodes.into_sorted()` and `TrieInput.state.into_sorted()` happen once; results are shared via `Arc`.
- **NEW:** `hashed_state.clone_into_sorted()` is called in `compute_state_root_parallel` (line 966). Each of `construct_prefix_sets()` and `clone_into_sorted()` walks the full hashed state.

---

## Summary Table

| Aspect                     | OLD                                                                 | NEW                                                                 |
|----------------------------|---------------------------------------------------------------------|---------------------------------------------------------------------|
| Data source                | ConsistentDbView + TrieInput                                       | OverlayStateProviderFactory                                         |
| Trie input construction    | `compute_trie_input()` (revert state + in-memory blocks + block)   | Implicit via overlay + `with_extended_hashed_state_overlay`         |
| `compute_trie_input`       | Yes (lines 1003–850)                                               | No                                                                  |
| Prefix sets                | From TrieInput (revert + blocks + block)                           | From `hashed_state.construct_prefix_sets()` only                    |
| Overlay in ParallelStateRoot | Pre-sorted `Arc` data passed in                                  | Per-provider overlay from factory                                   |
| Provider per storage task  | `view.provider_ro()`                                               | `factory.database_provider_ro()`                                    |
| Proof workers              | ProofTaskManager / ProofTaskCtx                                    | ProofWorkerHandle                                                   |

---

## Files and Line References

| File                                      | OLD (012ffc0)          | NEW (6b50539)          |
|-------------------------------------------|------------------------|------------------------|
| `engine/tree/.../payload_validator.rs`    | Lines 406–566, 868–888, 1003–850 | Lines 383–560, 955–993 |
| `trie/parallel/src/root.rs`               | Lines 46–65, 86–165   | Lines 39–59, 82–145    |
| `storage/provider/.../overlay.rs`         | N/A                    | Lines 354–369, 407–436  |
