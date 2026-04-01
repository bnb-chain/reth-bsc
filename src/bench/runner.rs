use crate::bench::commit_service::CommitResult;
use crate::bench::config::BenchConfig;
use crate::bench::db_init;
use crate::bench::overlay::{BundleStateOverlay, MaybeOverlay};
use crate::bench::report::BlockTiming;
use crate::bench::tx_gen;
use crate::bench::validator_setup;
use crate::node::evm::config::{BscEvmConfig, BscNextBlockEnvAttributes};
use crate::node::evm::util::insert_header_to_cache;
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::signer::init_global_signer;
use crate::node::miner::util::prepare_new_attributes;

use alloy_consensus::{transaction::Recovered, BlockHeader};
use alloy_primitives::B256;
use reth_evm::execute::{BlockBuilder, BlockBuilderOutcome};
use reth_evm::{ConfigureEvm, NextBlockEnvAttributes};
use reth_payload_primitives::PayloadBuilderAttributes;
use reth_primitives::SealedHeader;
use reth_primitives_traits::SignerRecoverable;
use reth_provider::{
    AccountReader, BlockWriter, DBProvider, DatabaseProviderFactory, ExecutionOutcome,
    OriginalValuesKnown, StateWriteConfig, StateWriter,
};
use reth_revm::cached::CachedReads;
use reth_revm::database::StateProviderDatabase;
use revm::database::{BundleState, State};
use std::sync::Arc;
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BenchBuildSource {
    Canonical,
    Speculative,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingCommitTarget {
    block_number: u64,
    block_hash: B256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BenchSpeculativeState {
    durable_base_hash: B256,
    durable_base_number: u64,
    next_build_source: BenchBuildSource,
    pending_commit_target: Option<PendingCommitTarget>,
}

impl BenchSpeculativeState {
    fn new(durable_base_hash: B256, durable_base_number: u64) -> Self {
        Self {
            durable_base_hash,
            durable_base_number,
            next_build_source: BenchBuildSource::Canonical,
            pending_commit_target: None,
        }
    }

    fn state_base_hash(&self) -> B256 {
        self.durable_base_hash
    }

    fn state_base_number(&self) -> u64 {
        self.durable_base_number
    }

    fn next_build_source(&self) -> BenchBuildSource {
        self.next_build_source
    }

    fn on_submitted_parent(
        &mut self,
        block_hash: B256,
        block_number: u64,
        build_source: BenchBuildSource,
    ) {
        self.pending_commit_target = Some(PendingCommitTarget { block_number, block_hash });
        self.next_build_source = match build_source {
            BenchBuildSource::Canonical => BenchBuildSource::Speculative,
            BenchBuildSource::Speculative => BenchBuildSource::Canonical,
        };
    }

    fn on_commit_finished(&mut self, block_hash: B256, block_number: u64) {
        self.durable_base_hash = block_hash;
        self.durable_base_number = block_number;

        if self.pending_commit_target == Some(PendingCommitTarget { block_number, block_hash }) {
            self.pending_commit_target = None;
        }
    }

    fn requires_canonical_wait(&self) -> bool {
        matches!(self.next_build_source, BenchBuildSource::Canonical)
            && self.pending_commit_target.is_some()
    }
}

fn record_commit_result(
    speculative_state: &mut BenchSpeculativeState,
    pending_commit_hashes: &mut std::collections::HashMap<u64, B256>,
    commit_results: &mut std::collections::HashMap<u64, CommitResult>,
    commit_result: CommitResult,
) -> eyre::Result<()> {
    let block_hash =
        pending_commit_hashes.remove(&commit_result.block_number).ok_or_else(|| {
            eyre::eyre!("Missing pending commit hash for block {}", commit_result.block_number)
        })?;

    speculative_state.on_commit_finished(block_hash, commit_result.block_number);
    commit_results.insert(commit_result.block_number, commit_result);
    Ok(())
}

/// Run the full miner pipeline benchmark.
pub fn run_benchmark(config: BenchConfig) -> eyre::Result<Vec<BlockTiming>> {
    println!("=== BSC Execution/State-Root Microbenchmark ===");
    println!(
        "Blocks: {}, TXs/block: {}, Funded accounts: {}",
        config.num_blocks, config.txs_per_block, config.funded_accounts
    );

    // 1. Initialize infrastructure (parlia, snapshot, header cache, MDBX + ProviderFactory)
    let restored_setup = db_init::try_restore_post_setup(&config)?;
    let (init, mut parent_header, mut parent_snapshot, mut cached_reads, start_block_idx) =
        if let Some(restored) = restored_setup {
            println!("\n[1/6] Reusing cached post-setup benchmark state...");
            (restored.init, restored.parent_header, restored.parent_snapshot, None, 1usize)
        } else {
            println!("\n[1/6] Initializing infrastructure from genesis...");
            let init = db_init::init_benchmark(&config)?;
            let parent_header = init.genesis_header.clone();
            let parent_snapshot = init.genesis_snapshot.clone();
            (init, parent_header, parent_snapshot, None, 0usize)
        };

    let chain_id = init.chain_spec.inner.chain.id();
    let evm_config = BscEvmConfig::new(init.chain_spec.clone());

    // Initialize global signer for block sealing (first validator)
    init_global_signer(config.private_keys[0])
        .map_err(|e| eyre::eyre!("Failed to init global signer: {}", e))?;

    // 2. TrieDB status (initialized in db_init if --triedb was set)
    if config.triedb {
        println!("\n[2/6] TrieDB enabled (initialized during genesis)");
    } else {
        println!("\n[2/6] TrieDB disabled (use --triedb to enable)");
    }
    if config.chain_difflayers {
        println!("  Difflayer chaining: enabled (parent difflayers forwarded to builder)");
    }

    // 3. Verify genesis state is readable from MDBX
    println!("\n[3/6] Verifying genesis state in MDBX...");
    {
        let state_provider = init
            .factory
            .latest()
            .map_err(|e| eyre::eyre!("Failed to get latest state after genesis: {}", e))?;
        // Quick sanity check: deployer account should exist
        let deployer_addr = db_init::address_from_private_key(&config.deployer_key);
        let acct = state_provider
            .basic_account(&deployer_addr)
            .map_err(|e| eyre::eyre!("Failed to query deployer account: {}", e))?;
        if let Some(a) = &acct {
            println!("  Deployer {} balance: {}", deployer_addr, a.balance);
        } else {
            println!("  WARNING: Deployer {} not found in genesis state", deployer_addr);
        }
    }

    let deployer_key = &config.deployer_key;

    // 4. Generate setup transactions unless we resumed from a post-setup cache
    let (setup_txs, erc20_address): (Vec<Recovered<reth_primitives::TransactionSigned>>, _) =
        if start_block_idx == 0 {
            println!("\n[4/6] Generating BLS keys and registering validators...");
            let (create_val_txs, _bls_keys) = validator_setup::create_all_validator_txs(
                &config.private_keys,
                &init.validator_addresses,
                chain_id,
            );
            println!("  Generated {} createValidator transactions", create_val_txs.len());

            println!("\n[5/6] Preparing ERC20 deployment and distribution...");
            let (deploy_tx, erc20_address) = tx_gen::erc20_deploy_tx(deployer_key, 0, chain_id);
            println!("  ERC20 contract will be at: {}", erc20_address);

            let distribution_txs = tx_gen::erc20_distribution_txs(
                deployer_key,
                &init.funded_accounts,
                erc20_address,
                1,
                chain_id,
            );
            println!("  Generated {} distribution transactions", distribution_txs.len());

            let mut setup_txs = Vec::new();
            for tx in &create_val_txs {
                if let Ok(r) = tx.clone().try_into_recovered() {
                    setup_txs.push(r);
                }
            }
            setup_txs.push(deploy_tx);
            setup_txs.extend(distribution_txs);
            (setup_txs, erc20_address)
        } else {
            println!("\n[4/6] Reusing cached validator registration and ERC20 setup...");
            let (_deploy_tx, erc20_address) = tx_gen::erc20_deploy_tx(deployer_key, 0, chain_id);
            println!("  ERC20 contract already deployed at: {}", erc20_address);
            println!("\n[5/6] Skipping setup transaction generation due to post-setup cache");
            (Vec::new(), erc20_address)
        };

    // 6. Pre-generate benchmark transaction pool
    println!(
        "\n[6/6] Generating transaction pool ({} ERC20 transfers)...",
        config.num_blocks * config.txs_per_block
    );
    let tx_pool = tx_gen::generate_tx_pool(
        &init.funded_accounts,
        config.num_blocks,
        config.txs_per_block,
        chain_id,
        erc20_address,
    );

    // === Execute blocks ===
    let mut timings = Vec::with_capacity(config.num_blocks);
    let mut commit_results: std::collections::HashMap<u64, CommitResult> =
        std::collections::HashMap::new();
    let mut pending_commit_hashes: std::collections::HashMap<u64, B256> =
        std::collections::HashMap::new();

    // Difflayer from previous block for triedb warm cache
    let mut _prev_difflayer: Option<std::sync::Arc<rust_eth_triedb_common::DiffLayer>> = None;

    // Pipeline state: previous block's bundle for overlay
    let mut prev_bundle: Option<BundleState> = None;

    // Pipelined commit: closure-based channel + background thread
    let (commit_tx, commit_rx) = std::sync::mpsc::sync_channel::<
        Box<dyn FnOnce() -> Result<CommitResult, String> + Send>,
    >(1);
    let (result_tx, result_rx) = std::sync::mpsc::channel::<Result<CommitResult, String>>();

    let commit_thread = std::thread::Builder::new()
        .name("bench-commit".into())
        .spawn(move || {
            while let Ok(work_fn) = commit_rx.recv() {
                let result = work_fn();
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        })
        .expect("failed to spawn commit thread");

    let turn_length = parent_snapshot.turn_length.unwrap_or(1) as usize;
    let num_validators = init.validator_addresses.len();
    let mut speculative_state =
        BenchSpeculativeState::new(parent_header.hash(), parent_header.number());

    // Total blocks: 1 setup + N benchmark
    let total_blocks = config.num_blocks + 1;

    for block_idx in start_block_idx..total_blocks {
        let block_number = block_idx as u64 + 1;
        let is_setup_block = block_idx == 0;
        let block_start = Instant::now();
        let build_source = if is_setup_block {
            BenchBuildSource::Canonical
        } else {
            speculative_state.next_build_source()
        };
        let mut wait_for_base_us = 0u128;

        if !is_setup_block && speculative_state.requires_canonical_wait() {
            let wait_start = Instant::now();
            while speculative_state.requires_canonical_wait() {
                let commit_result = result_rx
                    .recv()
                    .map_err(|_| eyre::eyre!("Commit thread exited before result was returned"))?
                    .map_err(|e| eyre::eyre!("Commit error: {}", e))?;
                record_commit_result(
                    &mut speculative_state,
                    &mut pending_commit_hashes,
                    &mut commit_results,
                    commit_result,
                )?;
            }
            wait_for_base_us = wait_start.elapsed().as_micros();
        }

        // Determine the in-turn validator using Parlia rotation logic
        let validator_index = determine_validator_index(block_number, num_validators, turn_length);
        let validator_addr = init.validator_addresses[validator_index];

        let has_cached_reads = cached_reads.is_some();
        let state_base_hash =
            if is_setup_block { parent_header.hash() } else { speculative_state.state_base_hash() };
        let state_base_number = if is_setup_block {
            parent_header.number()
        } else {
            speculative_state.state_base_number()
        };

        // Create MiningContext
        let mut mining_ctx = MiningContext {
            header: None,
            parent_header: parent_header.clone(),
            parent_snapshot: Arc::new(parent_snapshot.clone()),
            is_inturn: true,
            cached_reads: None,
            parent_difflayers: None,
            source: match build_source {
                BenchBuildSource::Canonical => {
                    crate::node::miner::speculative::MiningContextSource::Canonical
                }
                BenchBuildSource::Speculative => {
                    crate::node::miner::speculative::MiningContextSource::Speculative
                }
            },
            state_base_hash: (state_base_hash != parent_header.hash()).then_some(state_base_hash),
            prev_bundle_state: None,
        };

        // Prepare attributes
        let attributes = prepare_new_attributes(
            &mut mining_ctx,
            init.parlia.clone(),
            &parent_header,
            validator_addr,
        );

        // TIME: State setup from MDBX
        let state_setup_start = Instant::now();

        // Get the current state from the MDBX database.
        // After genesis init or each committed block, latest() returns the correct parent state.
        let state_provider = init
            .factory
            .history_by_block_hash(state_base_hash)
            .map_err(|e| eyre::eyre!("Failed to get state for block {}: {:?}", block_number, e))?;

        // Wrap state provider for the EVM State DB.
        // When prev_bundle is available (pipelined commit in flight), layer it on top so that
        // reads for accounts modified in the previous block are satisfied from memory.
        let state_db = StateProviderDatabase::new(&*state_provider);
        let mut cached: CachedReads = cached_reads.take().unwrap_or_default();
        let maybe_overlay = if let Some(ref bundle) = prev_bundle {
            MaybeOverlay::Overlay(BundleStateOverlay::new(bundle.clone(), state_db))
        } else {
            MaybeOverlay::Plain(state_db)
        };
        let mut db = State::builder()
            .with_database(cached.as_db_mut(maybe_overlay))
            .with_bundle_update()
            .build();

        let state_setup_us = state_setup_start.elapsed().as_micros();

        // Build the block using the EVM config pipeline
        let pre_exec_start = Instant::now();

        let mut builder = evm_config
            .builder_for_next_block(
                &mut db,
                &parent_header,
                BscNextBlockEnvAttributes {
                    inner: NextBlockEnvAttributes {
                        timestamp: attributes.timestamp(),
                        suggested_fee_recipient: attributes.suggested_fee_recipient(),
                        prev_randao: attributes.prev_randao(),
                        gas_limit: parent_header.gas_limit,
                        parent_beacon_block_root: attributes.parent_beacon_block_root(),
                        withdrawals: Some(attributes.withdrawals().clone()),
                        extra_data: Default::default(),
                    },
                    // TODO: difflayer chaining disabled during debugging
                    parent_difflayers: None,
                    triedb_prefetcher: None,
                    validator_cache_sink: None,
                    turn_length_sink: None,
                },
            )
            .map_err(|e| {
                eyre::eyre!("Failed to create block builder for block {}: {:?}", block_number, e)
            })?;

        // Apply pre-execution changes (system contract calls)
        builder
            .apply_pre_execution_changes()
            .map_err(|e| eyre::eyre!("Pre-execution failed for block {}: {:?}", block_number, e))?;
        let pre_execution_us = pre_exec_start.elapsed().as_micros();

        // TIME: Transaction execution
        // Txs are pre-recovered (ecrecover done during pool generation, matching production
        // where txs arrive pre-recovered from P2P mempool).
        let tx_exec_start = Instant::now();
        let mut execute_only_us = 0u128;
        let mut tx_count = 0u64;
        let mut gas_used = 0u64;

        if is_setup_block {
            // Block 0 (setup): createValidator + ERC20 deploy + distribution
            for recovered in setup_txs.iter().cloned() {
                let execute_start = Instant::now();
                match builder.execute_transaction(recovered) {
                    Ok(g) => {
                        tx_count += 1;
                        gas_used += g;
                    }
                    Err(e) => {
                        tracing::trace!("Setup tx failed: {:?}", e);
                    }
                }
                execute_only_us += execute_start.elapsed().as_micros();
            }
        } else {
            // Benchmark blocks: execute ERC20 transfers from the pre-recovered tx pool
            let pool_idx = block_idx - 1; // block_idx 1 -> pool index 0
            let selected_txs = tx_pool.blocks.get(pool_idx).map(Vec::as_slice).unwrap_or(&[]);

            for recovered in selected_txs.iter().cloned() {
                let execute_start = Instant::now();
                match builder.execute_transaction(recovered) {
                    Ok(g) => {
                        tx_count += 1;
                        gas_used += g;
                    }
                    Err(e) => {
                        tracing::trace!("TX execution failed: {:?}", e);
                    }
                }
                execute_only_us += execute_start.elapsed().as_micros();
            }
        }
        let tx_execution_us = tx_exec_start.elapsed().as_micros();

        // TIME: Finish (post-exec + state root + assembly)
        // Get a separate state provider BEFORE timing starts to exclude MDBX overhead
        // (production finalize timing in payload.rs:623-625 doesn't include provider creation).
        let finish_state_provider =
            init.factory.history_by_block_hash(state_base_hash).map_err(|e| {
                eyre::eyre!("Failed to get finish state for block {}: {:?}", block_number, e)
            })?;

        let finish_start = Instant::now();

        // Use finish_with_difflayer to get both the block outcome and an optional difflayer
        // for chaining to the next block's triedb state root calculation.
        let outcome_with_dl = builder
            .finish_with_difflayer(&*finish_state_provider)
            .map_err(|e| eyre::eyre!("Block {} finish failed: {:?}", block_number, e))?;
        let produced_difflayer = outcome_with_dl.difflayer;
        let outcome = outcome_with_dl.inner;

        let finish_us = finish_start.elapsed().as_micros();

        // Extract metrics from the outcome before committing
        let hashed_accounts = outcome.hashed_state.accounts.len();
        let hashed_storage_slots: usize =
            outcome.hashed_state.storages.values().map(|s| s.storage.len()).sum();

        let BlockBuilderOutcome { execution_result, block: recovered_block, .. } = outcome;

        // Get the block header info we need before consuming the block
        let new_header = recovered_block.header().clone();
        let new_hash = new_header.hash_slow();
        let state_root = new_header.state_root;

        // Extract bundle_state BEFORE take_bundle() consumes it.
        // This clone is used as the overlay for the next block's reads during pipelined commit.
        let next_bundle = db.bundle_state.clone();

        // Extract cache from bundle_state before take_bundle() consumes it.
        // This mirrors production's cache_for_next() pattern (bsc_miner.rs:331-351).
        let mut new_cached = CachedReads::default();
        for (addr, acc) in db.bundle_state.state.iter() {
            if let Some(info) = acc.info.clone() {
                let storage =
                    acc.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
                new_cached.insert_account(*addr, info, storage);
            }
        }

        // Build ExecutionOutcome from the bundle state
        let execution_outcome = ExecutionOutcome::new(
            db.take_bundle(),
            vec![execution_result.receipts],
            block_number,
            vec![execution_result.requests],
        );

        // Get a read-write database provider for committing
        let provider_rw = init.factory.database_provider_rw().map_err(|e| {
            eyre::eyre!("Failed to get provider_rw for block {}: {:?}", block_number, e)
        })?;

        // Update chain tip (must happen on main thread before next block starts)
        let new_sealed = SealedHeader::new(new_header.clone(), new_hash);
        insert_header_to_cache(new_header);

        // Populate CachedReads for next block from execution outcome
        cached_reads = Some(new_cached);

        if is_setup_block {
            // Setup block: commit synchronously (no pipeline)
            let commit_start = Instant::now();

            provider_rw
                .insert_block(&recovered_block)
                .map_err(|e| eyre::eyre!("Failed to insert block {}: {:?}", block_number, e))?;

            provider_rw
                .write_state(
                    &execution_outcome,
                    OriginalValuesKnown::No,
                    StateWriteConfig::default(),
                )
                .map_err(|e| {
                    eyre::eyre!("Failed to write state for block {}: {:?}", block_number, e)
                })?;

            if config.triedb {
                let mut triedb = rust_eth_triedb::get_global_triedb();
                triedb.flush(block_number, state_root, &None).map_err(|e| {
                    eyre::eyre!("Failed to flush triedb for block {}: {:?}", block_number, e)
                })?;
            }

            provider_rw
                .commit()
                .map_err(|e| eyre::eyre!("Failed to commit block {}: {:?}", block_number, e))?;

            let commit_us = commit_start.elapsed().as_micros();
            let total_us = block_start.elapsed().as_micros();

            parent_header = new_sealed;
            parent_snapshot.block_number = block_number;
            parent_snapshot.block_hash = new_hash;

            // Update difflayer chain for next block
            _prev_difflayer = produced_difflayer;
            prev_bundle = None; // setup block committed synchronously, no overlay needed
            speculative_state = BenchSpeculativeState::new(new_hash, block_number);

            db_init::persist_post_setup_cache(&config, &init, &parent_header, &parent_snapshot)?;
            println!(
                "  Setup block 0 complete: {} txs, {} gas used, {}us total (commit: {}us)",
                tx_count, gas_used, total_us, commit_us
            );
            continue;
        }

        // Benchmark blocks: flush triedb on main thread (required before next block's
        // finish can compute state root), then send MDBX work to background thread.
        let mut triedb_flush_us = 0u128;
        if config.triedb {
            let mut triedb = rust_eth_triedb::get_global_triedb();
            let triedb_flush_start = Instant::now();
            triedb.flush(block_number, state_root, &None).map_err(|e| {
                eyre::eyre!("Failed to flush triedb for block {}: {:?}", block_number, e)
            })?;
            triedb_flush_us = triedb_flush_start.elapsed().as_micros();
        }

        let pipeline_send_start = Instant::now();
        commit_tx
            .send(Box::new(move || {
                let commit_start = Instant::now();

                let insert_block_start = Instant::now();
                provider_rw
                    .insert_block(&recovered_block)
                    .map_err(|e| format!("Failed to insert block {}: {:?}", block_number, e))?;
                let insert_block_us = insert_block_start.elapsed().as_micros();

                let write_state_start = Instant::now();
                provider_rw
                    .write_state(
                        &execution_outcome,
                        OriginalValuesKnown::No,
                        StateWriteConfig::default(),
                    )
                    .map_err(|e| {
                        format!("Failed to write state for block {}: {:?}", block_number, e)
                    })?;
                let write_state_us = write_state_start.elapsed().as_micros();

                let provider_commit_start = Instant::now();
                provider_rw
                    .commit()
                    .map_err(|e| format!("Failed to commit block {}: {:?}", block_number, e))?;
                let provider_commit_us = provider_commit_start.elapsed().as_micros();

                let commit_us = commit_start.elapsed().as_micros();

                Ok(CommitResult {
                    block_number,
                    insert_block_us,
                    write_state_us,
                    triedb_flush_us: 0,
                    provider_commit_us,
                    commit_us,
                })
            }))
            .map_err(|_| eyre::eyre!("Commit thread exited unexpectedly"))?;
        let pipeline_send_us = pipeline_send_start.elapsed().as_micros();

        let total_us = block_start.elapsed().as_micros();

        // Update parent header and snapshot for the next block
        parent_header = new_sealed;
        parent_snapshot.block_number = block_number;
        parent_snapshot.block_hash = new_hash;

        // Set overlay bundle so the next block can read from memory while commit is in flight
        prev_bundle = Some(next_bundle);

        pending_commit_hashes.insert(block_number, new_hash);
        speculative_state.on_submitted_parent(new_hash, block_number, build_source);

        // Chain difflayer for triedb warm cache
        _prev_difflayer = produced_difflayer;

        // Record timing for benchmark blocks (commit sub-fields are placeholders;
        // they will be merged from CommitResult after the block loop).
        let timing = BlockTiming {
            block_number,
            validator_index,
            tx_count: tx_count as usize,
            gas_used,
            is_speculative: matches!(build_source, BenchBuildSource::Speculative),
            state_base_number,
            wait_for_base_us,
            state_setup_us,
            pre_execution_us,
            execute_only_us,
            tx_execution_us,
            insert_block_us: 0,    // filled from CommitResult after loop
            write_state_us: 0,     // filled from CommitResult after loop
            triedb_flush_us,       // measured on main thread
            provider_commit_us: 0, // filled from CommitResult after loop
            commit_us: 0,          // filled from CommitResult after loop
            pipeline_send_us,
            finish_us,
            total_us,
            hashed_accounts,
            hashed_storage_slots,
            has_cached_reads,
        };

        if block_number.is_multiple_of(10) || block_number <= 2 {
            let flags = if has_cached_reads { "C" } else { "-" };
            println!(
                "  Block {:>4} | v[{}] {}{} | txs: {:>3} | gas: {:>8} | wait: {:>6}us | finish: {:>6}us | send: {:>6}us | total: {:>6}us",
                block_number,
                validator_index,
                flags,
                if matches!(build_source, BenchBuildSource::Speculative) { "S" } else { "-" },
                tx_count,
                gas_used,
                wait_for_base_us,
                finish_us,
                pipeline_send_us,
                total_us,
            );
        }

        timings.push(timing);
    }

    // Signal commit thread to exit and collect results
    drop(commit_tx);
    let _ = commit_thread.join();

    while let Ok(result) = result_rx.try_recv() {
        match result {
            Ok(cr) => {
                record_commit_result(
                    &mut speculative_state,
                    &mut pending_commit_hashes,
                    &mut commit_results,
                    cr,
                )?;
            }
            Err(e) => return Err(eyre::eyre!("Commit error: {}", e)),
        }
    }

    // Merge commit timings into block timings
    for timing in &mut timings {
        if let Some(cr) = commit_results.get(&timing.block_number) {
            timing.insert_block_us = cr.insert_block_us;
            timing.write_state_us = cr.write_state_us;
            // triedb_flush_us already set on main thread — don't overwrite
            timing.provider_commit_us = cr.provider_commit_us;
            timing.commit_us = cr.commit_us;
        }
    }

    Ok(timings)
}

/// Determine which validator is in-turn for a given block number.
fn determine_validator_index(
    block_number: u64,
    num_validators: usize,
    turn_length: usize,
) -> usize {
    let turn = (block_number.saturating_sub(1) / turn_length as u64) as usize;
    turn % num_validators
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn example_hash(number: u64) -> B256 {
        B256::with_last_byte(number as u8)
    }

    #[test]
    fn test_validator_rotation() {
        assert_eq!(determine_validator_index(1, 3, 1), 0);
        assert_eq!(determine_validator_index(2, 3, 1), 1);
        assert_eq!(determine_validator_index(3, 3, 1), 2);
        assert_eq!(determine_validator_index(4, 3, 1), 0);

        assert_eq!(determine_validator_index(1, 3, 2), 0);
        assert_eq!(determine_validator_index(2, 3, 2), 0);
        assert_eq!(determine_validator_index(3, 3, 2), 1);
        assert_eq!(determine_validator_index(4, 3, 2), 1);
        assert_eq!(determine_validator_index(5, 3, 2), 2);
        assert_eq!(determine_validator_index(6, 3, 2), 2);
        assert_eq!(determine_validator_index(7, 3, 2), 0);
    }

    #[test]
    fn bench_pipeline_keeps_durable_base_one_commit_behind_speculative_parent() {
        let mut state = BenchSpeculativeState::new(example_hash(99), 99);
        state.on_submitted_parent(example_hash(100), 100, BenchBuildSource::Canonical);

        assert_eq!(state.state_base_hash(), example_hash(99));
        assert_eq!(state.state_base_number(), 99);
        assert_eq!(state.next_build_source(), BenchBuildSource::Speculative);
    }

    #[test]
    fn bench_pipeline_advances_durable_base_after_commit_result() {
        let mut state = BenchSpeculativeState::new(example_hash(99), 99);
        state.on_submitted_parent(example_hash(100), 100, BenchBuildSource::Canonical);
        state.on_commit_finished(example_hash(100), 100);

        assert_eq!(state.state_base_hash(), example_hash(100));
        assert_eq!(state.state_base_number(), 100);
    }

    #[test]
    #[ignore] // Run with: cargo test --features bench-test -p reth_bsc bench::runner::tests::decrypt_dev_keystores -- --ignored --nocapture
    fn decrypt_dev_keystores() {
        let keystores = [
            "/Users/user/development/node-deploy/.local/node0/keystore/UTC--2024-05-10T03-37-35.756992000Z--bcdd0d2cda5f6423e57b6a4dcd75decbe31aecf0",
            "/Users/user/development/node-deploy/.local/node1/keystore/UTC--2024-05-10T03-37-36.999615000Z--bbd1acc20bd8304309d31d8fd235210d0efc049d",
            "/Users/user/development/node-deploy/.local/node2/keystore/UTC--2024-05-10T03-37-38.188296000Z--5e2a531a825d8b61bcc305a35a7433e9a8920f0f",
        ];
        let password = "0123456789";

        for (i, path) in keystores.iter().enumerate() {
            let pk = eth_keystore::decrypt_key(path, password).expect("decrypt failed");
            println!("Validator {} private key: {}", i, hex::encode(&pk));
        }
    }
}
