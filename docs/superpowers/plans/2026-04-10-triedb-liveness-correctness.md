# TrieDB Liveness & Correctness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the unsafe `skip_triedb_root` mechanism with a correct pathdb gap detection that buffers unverifiable blocks instead of accepting them without validation.

**Architecture:** Two changes in the reth engine tree crate. (1) Add a triedb readiness check in `insert_block_or_payload` that buffers blocks as `Disconnected` when pathdb gap is detected, triggering P2P sequential recovery. (2) Remove the `skip_triedb_root` logic from `payload_validator.rs` that previously accepted blocks without state_root verification.

**Tech Stack:** Rust, reth engine tree (`/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/`)

---

## File Map

| File | Responsibility | Change |
|------|---------------|--------|
| `tree/mod.rs` | Block insertion orchestration | Add pathdb gap check before validation |
| `tree/payload_validator.rs` | Block execution + triedb state root validation | Remove `skip_triedb_root` |

---

### Task 1: Add pathdb gap check in `insert_block_or_payload`

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/mod.rs:2823-2830`

- [ ] **Step 1: Add the pathdb gap check after the `state_provider_builder` match**

In `insert_block_or_payload`, insert the following block between `Ok(Some(_)) => {}` (line 2823) and `let is_fork = ...` (line 2826):

Replace this exact code in `/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/mod.rs`:

```rust
            Ok(Some(_)) => {}
        }

        // determine whether we are on a fork chain by comparing the block number with the
```

With:

```rust
            Ok(Some(_)) => {}
        }

        // Safety guard: when triedb is active but no difflayers bridge the pathdb disk
        // layer to the block's parent, we cannot compute a correct state root.  Rather
        // than executing with stale trie data, buffer the block as Disconnected so the
        // P2P layer fetches ancestors sequentially from the disk layer height, rebuilding
        // difflayers one block at a time.
        if rust_eth_triedb::triedb_manager::is_triedb_active() {
            let difflayers =
                self.state.tree_state.merged_difflayer_by_hash(block_id.parent);
            if difflayers.is_none() {
                let triedb = rust_eth_triedb::triedb_manager::get_global_triedb();
                if let Ok((_persist_block, persist_root)) = triedb.latest_persist_state() {
                    if let Ok(Some(parent_header)) =
                        self.sealed_header_by_hash(block_id.parent)
                    {
                        if parent_header.state_root() != persist_root {
                            warn!(
                                target: "engine::tree",
                                block = ?block_num_hash,
                                parent = ?block_id.parent,
                                parent_state_root = ?parent_header.state_root(),
                                pathdb_persist_root = ?persist_root,
                                "Triedb pathdb gap: no difflayers and parent state root \
                                 diverges from pathdb disk layer — buffering block as \
                                 Disconnected for sequential P2P recovery"
                            );
                            let block = convert_to_block(self, input)?;
                            let missing_ancestor = block.parent_num_hash();
                            self.state.buffer.insert_block(block);
                            return Ok(InsertPayloadOk::Inserted(
                                BlockStatus::Disconnected {
                                    head: self.state.tree_state.current_canonical_head,
                                    missing_ancestor,
                                },
                            ));
                        }
                    }
                }
            }
        }

        // determine whether we are on a fork chain by comparing the block number with the
```

- [ ] **Step 2: Verify the build compiles**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && cargo check -p reth-engine-tree 2>&1 | tail -20
```
Expected: compilation succeeds (0 errors). Warnings are acceptable.

- [ ] **Step 3: Commit**

```bash
cd /Users/jiaqiwang/workspace/reth && git add crates/engine/tree/src/tree/mod.rs && git commit -m "fix: buffer blocks as Disconnected when triedb pathdb gap detected

When triedb is active but no difflayers bridge the pathdb disk layer to
the block's parent, we cannot compute a correct state root.  Instead of
executing with stale trie data, buffer the block as Disconnected so the
P2P layer fetches ancestors sequentially from the disk layer height,
rebuilding difflayers one block at a time.

This replaces the unsafe skip_triedb_root approach that accepted blocks
without state_root verification."
```

---

### Task 2: Remove `skip_triedb_root` from payload_validator.rs

**Files:**
- Modify: `/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/payload_validator.rs:395-523`

- [ ] **Step 1: Remove the `skip_triedb_root` flag computation (lines 395-434)**

Replace this exact code in `/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/payload_validator.rs`:

```rust
        // Safety guard: when no difflayers are available, verify that the parent state root
        // matches the pathdb disk layer.  After a restart, in-memory difflayers are lost and
        // pathdb only holds the state at the last flushed block.  If the parent is beyond
        // that point, triedb would silently read stale nodes and compute a wrong state root.
        // In that case we skip the triedb state-root check and trust the block header's root,
        // accepting the block without a difflayer.  Persistence will recompute the difflayer
        // later when flushing this block to pathdb (via save_blocks).
        let skip_triedb_root = if difflayers.is_none() {
            match triedb.latest_persist_state() {
                Ok((persist_block, persist_root)) => {
                    if parent_block.state_root() != persist_root {
                        warn!(
                            target: "engine::tree",
                            block = ?block_num_hash,
                            parent = ?parent_hash,
                            parent_state_root = ?parent_block.state_root(),
                            pathdb_block = persist_block,
                            pathdb_root = ?persist_root,
                            "Triedb pathdb gap detected: no difflayers and parent state root \
                             diverges from pathdb disk layer — skipping triedb state root \
                             validation for this block"
                        );
                        true
                    } else {
                        false
                    }
                }
                Err(e) => {
                    warn!(
                        target: "engine::tree",
                        block = ?block_num_hash,
                        error = ?e,
                        "Failed to query pathdb latest_persist_state, proceeding with triedb validation"
                    );
                    false
                }
            }
        } else {
            false
        };

        let evm_env = self.evm_env_for(&input).map_err(NewPayloadError::other)?;
```

With:

```rust
        let evm_env = self.evm_env_for(&input).map_err(NewPayloadError::other)?;
```

- [ ] **Step 2: Remove the `skip_triedb_root` early return block (lines 500-523)**

Replace this exact code in `/Users/jiaqiwang/workspace/reth/crates/engine/tree/src/tree/payload_validator.rs`:

```rust
        // When pathdb gap is detected, skip the triedb state root calculation entirely.
        // The block is accepted with its declared state root; persistence will recompute
        // the difflayer when flushing (save_blocks uses latest_persist_state sequentially).
        if skip_triedb_root {
            triedb.clean();

            if let Some(valid_block_tx) = valid_block_tx {
                let _ = valid_block_tx.send(());
            }

            info!(
                target: "engine::tree",
                block = ?block_num_hash,
                state_root = ?block.state_root(),
                "Accepted block without triedb state root validation (pathdb gap)"
            );

            return Ok(ExecutedBlock {
                recovered_block: Arc::new(block),
                execution_output: output,
                trie_data: DeferredTrieData::ready(ComputedTrieData::default()),
                difflayer: None,
            });
        }

        // Wait for the prefetcher result (may be None if prefetch failed/wasn't available).
```

With:

```rust
        // Wait for the prefetcher result (may be None if prefetch failed/wasn't available).
```

- [ ] **Step 3: Verify the build compiles**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && cargo check -p reth-engine-tree 2>&1 | tail -20
```
Expected: compilation succeeds. There should be no reference to `skip_triedb_root` remaining.

- [ ] **Step 4: Verify no remaining references to `skip_triedb_root`**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && grep -rn "skip_triedb_root" crates/engine/tree/src/
```
Expected: no output (no matches found).

- [ ] **Step 5: Commit**

```bash
cd /Users/jiaqiwang/workspace/reth && git add crates/engine/tree/src/tree/payload_validator.rs && git commit -m "fix: remove skip_triedb_root — never accept blocks without state_root verification

The skip_triedb_root path accepted blocks without triedb state_root
verification and set difflayer: None.  Such blocks could be flushed to
pathdb by save_blocks, writing unverified data.

The pathdb gap case is now handled earlier in insert_block_or_payload,
which buffers the block as Disconnected and returns Syncing to trigger
sequential P2P recovery.  By the time a block reaches the validator,
triedb is guaranteed to be able to verify it."
```

---

### Task 3: End-to-end verification

- [ ] **Step 1: Full crate build check**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && cargo check -p reth-engine-tree 2>&1 | tail -20
```
Expected: 0 errors.

- [ ] **Step 2: Build reth-bsc to verify downstream compatibility**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth-bsc && cargo check 2>&1 | tail -20
```
Expected: 0 errors. The reth-bsc crate depends on reth-engine-tree via git; since we're using local path overrides or the same workspace, this confirms no API breakage.

- [ ] **Step 3: Verify the logic by reading the final code**

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && grep -n "pathdb gap\|Disconnected\|is_triedb_active" crates/engine/tree/src/tree/mod.rs | head -10
```
Expected: the new pathdb gap check appears in `insert_block_or_payload`.

Run:
```bash
cd /Users/jiaqiwang/workspace/reth && grep -n "skip_triedb_root\|pathdb gap" crates/engine/tree/src/tree/payload_validator.rs
```
Expected: no matches — all skip logic removed.
