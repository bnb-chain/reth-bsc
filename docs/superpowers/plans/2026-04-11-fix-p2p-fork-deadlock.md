# Fix P2P Fork Deadlock — Three P0 Bugs

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix three interconnected bugs that cause a two-validator reth-bsc network to fork and permanently stop producing blocks.

**Architecture:** All three fixes are in `src/node/network/block_import/service.rs`. Bug 1: `queued_blocks` entries are never removed, blocking retry of blocks that returned `Syncing`/`Invalid`. Bug 2: Discovery fetches blocks from a random BSC sub-protocol peer instead of the peer that reported the unknown hash. Bug 3: Discovery considers a block "known" if it exists in the DB at all, even as a non-canonical sidechain block, so it stops trying to sync the peer's chain.

**Tech Stack:** Rust, tokio, reth engine tree

---

## File Map

All changes are in one file:
- **Modify:** `src/node/network/block_import/service.rs`

No new files needed. No test files (these are integration-level bugs that require a multi-node network to reproduce; unit tests would be artificial).

---

### Task 1: Fix `queued_blocks` never cleaned — blocks stuck after Syncing/Invalid

**Problem:** When `new_payload` returns `Syncing` or `Invalid`, the future returns `None`. The outcome processing loop (`pending_imports.poll_next_unpin`) skips `None` outcomes, so the block hash stays in `queued_blocks` forever. Later attempts to import the same block (via discovery or P2P) are silently dropped by the `queued_blocks.contains()` check at line 467.

**Fix approach:** The `new_payload` future must return the block hash even for `Syncing`/`Invalid` so `queued_blocks` can be cleaned. We change the future to always return `Some(outcome)` with enough info to clean up, OR we move the cleanup into the future itself by passing a shared reference to `queued_blocks`. The cleanest approach: return the block hash from the future for ALL outcomes (including `None` ones), and clean `queued_blocks` in the poll loop.

Since the `ImportFut` returns `Option<Outcome>` and `Outcome` requires a full `BlockValidation` result, the simplest correct fix is to change `new_payload` to accept the block hash AND have the future remove from `queued_blocks` on ALL code paths. We pass a `queued_blocks` handle (wrapped in `Arc<parking_lot::RwLock<LruCache<B256>>>`) into the future.

**Files:**
- Modify: `src/node/network/block_import/service.rs:95-99` (change `queued_blocks` to `Arc<RwLock<LruCache<B256>>>`)
- Modify: `src/node/network/block_import/service.rs:138` (wrap in Arc+RwLock)
- Modify: `src/node/network/block_import/service.rs:154` (`new_payload` takes queued_blocks ref)
- Modify: `src/node/network/block_import/service.rs:186-320` (remove hash from queued_blocks on all paths)
- Modify: `src/node/network/block_import/service.rs:467-471` (use Arc read lock)
- Modify: `src/node/network/block_import/service.rs:650-655` (remove commented-out code)

- [ ] **Step 1: Change `queued_blocks` field to `Arc<RwLock<LruCache<B256>>>`**

In the `ImportService` struct (around line 98), change:
```rust
    /// Cache of queued block hashes to avoid processing the same block.
    queued_blocks: Arc<parking_lot::RwLock<LruCache<B256>>>,
```

In the constructor (line 138), change:
```rust
    queued_blocks: Arc::new(parking_lot::RwLock::new(LruCache::new(LRU_PROCESSED_BLOCKS_SIZE))),
```

- [ ] **Step 2: Update `on_new_block` to use the Arc**

In `on_new_block` (lines 467-471), change the contains/insert calls:
```rust
        if self.queued_blocks.read().contains(&block.hash) {
            tracing::trace!(target: "bsc::block_import", "Block already queued when receiving new block: number = {:?}, hash = {:?}", block.block.0.block.header.number, block.hash);
            return;
        }
        self.queued_blocks.write().insert(block.hash);
```

- [ ] **Step 3: Update `new_payload` to accept and clean `queued_blocks`**

Change `new_payload` signature (line 154):
```rust
    fn new_payload(&self, block: BlockMsg, peer_id: PeerId) -> ImportFut {
        let engine = self.engine.clone();
        let forkchoice_engine = self.forkchoice_engine.clone();
        let queued_blocks = self.queued_blocks.clone();
```

Then at EVERY exit point of the future, remove the hash from queued_blocks BEFORE returning. Specifically:

After hash mismatch rejection (around line 170), add before `return`:
```rust
                queued_blocks.write().remove(&announced_hash);
```

After `PayloadStatusEnum::Valid` (around line 195), add before returning the outcome:
```rust
                        queued_blocks.write().remove(&block.hash);
```

After `PayloadStatusEnum::Invalid` (around line 222), change return to:
```rust
                        queued_blocks.write().remove(&block.hash);
                        None
```

After `PayloadStatusEnum::Syncing` (around line 313), change return to:
```rust
                        queued_blocks.write().remove(&block.hash);
                        None
```

After the catch-all `_ => None` (line 316), change to:
```rust
                    _ => {
                        queued_blocks.write().remove(&block.hash);
                        None
                    }
```

After the `Err(err) => None` (line 318), change to:
```rust
                Err(err) => {
                    queued_blocks.write().remove(&block.hash);
                    None
                }
```

- [ ] **Step 4: Remove the commented-out queued_blocks removal code**

Remove lines 650-655 (the TODO comment and commented-out code):
```rust
                // TODO: add queued blocks removal later, to avoid milicious block import, and trigger next download.
                // now, it must wait backfilling to download the correct block.
                // the verified header can drop the peer later, it cannot transfer a bad header now.
                // if let Some(block_hash) = outcome.block.hash {
                //     this.queued_blocks.remove(&block_hash);
                // }
```

- [ ] **Step 5: Build and verify compilation**

Run: `cargo build --release 2>&1 | tail -20`
Expected: Successful compilation with no errors related to `queued_blocks`.

- [ ] **Step 6: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "fix: clean queued_blocks on all new_payload outcomes (Syncing/Invalid/Valid)

Previously, blocks that returned Syncing or Invalid from new_payload
stayed in the queued_blocks LRU cache permanently. Later attempts to
re-import the same block (via periodic discovery or P2P relay) were
silently dropped by the queued_blocks.contains() check, preventing
fork recovery."
```

---

### Task 2: Fix discovery fetching from wrong peer

**Problem:** When the periodic discovery (Part B, line 716) finds a peer with an unknown head hash, it tries to fetch blocks via `GetBlocksByRange`. But if the reporting peer is not registered as a BSC sub-protocol peer, it falls back to `list_registered_peers().into_iter().next()` — a random BSC peer that may not have the needed blocks. The fetched blocks may be from a different chain fork, making them useless.

**Fix approach:** Pass the `peer_info.remote_id` (the peer that reported the unknown hash) into `batch_request_range_and_await_import` directly. If that peer isn't a BSC sub-protocol peer, skip the GetBlocksByRange fetch entirely and rely on the FCU fallback (which uses eth protocol downloads). Don't fall back to a random BSC peer.

**Files:**
- Modify: `src/node/network/block_import/service.rs:767-774` (discovery peer selection)
- Modify: `src/node/network/block_import/service.rs:248-256` (Syncing handler peer selection)

- [ ] **Step 1: Fix discovery peer selection (Part B)**

Replace lines 767-774 with:
```rust
                            // Only fetch from the peer that actually reported the
                            // unknown hash.  Falling back to a random BSC peer would
                            // fetch blocks from a different fork, wasting bandwidth
                            // and poisoning queued_blocks.
                            let target = if crate::node::network::bsc_protocol::registry::has_registered_peer(peer_info.remote_id) {
                                Some(peer_info.remote_id)
                            } else {
                                None
                            };
```

- [ ] **Step 2: Fix Syncing handler peer selection**

In the `PayloadStatusEnum::Syncing` handler (around lines 249-256), apply the same fix:
```rust
                        tokio::spawn(async move {
                            let target_peer = if crate::node::network::bsc_protocol::registry::has_registered_peer(fetch_peer) {
                                Some(fetch_peer)
                            } else {
                                // Don't fall back to a random BSC peer — it likely
                                // has a different chain fork and would return wrong
                                // blocks.
                                None
                            };
```

- [ ] **Step 3: Build and verify compilation**

Run: `cargo build --release 2>&1 | tail -20`
Expected: Successful compilation.

- [ ] **Step 4: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "fix: only fetch blocks from the peer that reported the unknown hash

Previously, when the reporting peer was not a BSC sub-protocol peer,
we fell back to a random BSC peer that likely had a different chain
fork. This caused fetched blocks to be unusable (wrong parent chain)
and poisoned the queued_blocks cache, preventing recovery."
```

---

### Task 3: Fix discovery considering non-canonical blocks as "known"

**Problem:** The discovery loop (line 739) skips a peer when `provider.block_number(peer_hash).ok().flatten().is_some()`. This checks if the block hash exists in the persistent DB — but the block might exist as a non-canonical sidechain block that was never made canonical. Once skipped, discovery never tries again for that peer (the peer's head hash doesn't change after the miner stops), creating a permanent deadlock.

**Fix approach:** Instead of checking whether the hash exists at all, check whether it's part of the **canonical chain**. A block hash is canonical if `provider.block_number(hash)` returns a number AND that number's canonical hash matches. Alternatively, we can check if the hash matches our current canonical head or is an ancestor of it. The simplest correct approach: check if the block is canonical by looking up its number and verifying the canonical hash at that number matches.

**Files:**
- Modify: `src/node/network/block_import/service.rs:739` (discovery skip condition)

- [ ] **Step 1: Replace the "known block" check with a "canonical block" check**

Replace line 739:
```rust
                            if provider.block_number(peer_hash).ok().flatten().is_some() {
                                continue;
                            }
```

With:
```rust
                            // Only skip if this hash is part of our canonical chain.
                            // A non-canonical sidechain block that happens to be in
                            // the DB should NOT suppress discovery — the peer's chain
                            // may be the correct fork we need to switch to.
                            let is_canonical = provider
                                .block_number(peer_hash)
                                .ok()
                                .flatten()
                                .and_then(|num| {
                                    provider
                                        .block_hash(num)
                                        .ok()
                                        .flatten()
                                        .map(|canonical_hash| canonical_hash == peer_hash)
                                })
                                .unwrap_or(false);
                            if is_canonical {
                                continue;
                            }
```

- [ ] **Step 2: Verify `BlockHashReader` is available on the provider**

The `provider` has type `Provider` with trait bounds `BlockNumReader + HeaderProvider`. We need `BlockHashReader::block_hash(number)` which is provided by `BlockNumReader`. Verify that `block_hash` method is available.

Run: `grep -n "fn block_hash" ~/.cargo/git/checkouts/reth-*/*/crates/storage/provider-traits/src/block.rs | head -5`

If `block_hash` is not on `BlockNumReader`, check `BlockHashReader` and add the trait bound.

- [ ] **Step 3: Build and verify compilation**

Run: `cargo build --release 2>&1 | tail -20`
Expected: Successful compilation. If there's a missing trait bound, add `BlockHashReader` to the `Provider` bounds on the `impl Future for ImportService` block (line 607-614).

- [ ] **Step 4: Commit**

```bash
git add src/node/network/block_import/service.rs
git commit -m "fix: discovery must check canonical status, not just block existence

Previously, provider.block_number(peer_hash) returning Some caused
discovery to skip the peer permanently. But the block might be a
non-canonical sidechain block — the peer's chain could be the correct
fork we need to sync. Now we verify the block is actually canonical
before skipping."
```

---

### Task 4: Final verification

- [ ] **Step 1: Full build**

Run: `cargo build --release 2>&1 | tail -20`
Expected: Clean compilation.

- [ ] **Step 2: Run existing tests**

Run: `cargo test -p reth-bsc --lib -- block_import 2>&1 | tail -20`
Expected: All existing tests pass.

- [ ] **Step 3: Review all changes**

Run: `git diff HEAD~3 -- src/node/network/block_import/service.rs`
Verify:
1. `queued_blocks` is now `Arc<RwLock<LruCache>>` and cleaned on all `new_payload` outcomes
2. Discovery peer selection only uses the reporting peer, no random fallback
3. Discovery canonical check uses `block_hash(num) == peer_hash`, not just `block_number(hash).is_some()`
4. No unrelated changes
