use crate::bench::config::BenchConfig;
use crate::bench::db_init;
use crate::bench::report::BlockTiming;
use crate::bench::tx_gen;
use crate::bench::validator_setup;
use crate::node::evm::config::{BscEvmConfig, BscNextBlockEnvAttributes};
use crate::node::evm::util::insert_header_to_cache;
use crate::node::evm::MinerTrieDbPrefetcher;
use crate::node::miner::bsc_miner::MiningContext;
use crate::node::miner::signer::init_global_signer;
use crate::node::miner::util::prepare_new_attributes;

use alloy_consensus::{transaction::Recovered, BlockHeader as _};
use alloy_evm::block::BlockExecutor;
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
use reth_revm::state::EvmState as RethEvmState;
use revm::database::State;
use rust_eth_triedb::get_global_triedb;
use rust_eth_triedb_common::{DiffLayer, DiffLayers};
use std::sync::Arc;
use std::time::Instant;

/// Benchmark-local triedb reuse context.
///
/// The direct benchmark commits every block to triedb immediately, so older diff layers are
/// already persisted to disk. Carrying only the latest parent difflayer still gives the next block
/// access to the miner's warm in-memory overlay and prefetch path without growing an unbounded
/// in-memory chain.
#[derive(Debug, Default, Clone)]
struct BenchTriedbContext {
    parent_difflayers: Option<DiffLayers>,
}

impl BenchTriedbContext {
    fn next_block_context(
        &self,
        triedb_enabled: bool,
        parent_header: &SealedHeader,
    ) -> (Option<DiffLayers>, Option<MinerTrieDbPrefetcher>) {
        if !triedb_enabled {
            return (None, None);
        }

        let parent_difflayers = self.parent_difflayers();
        let triedb_prefetcher = parent_difflayers.clone().and_then(|difflayers| {
            let mut triedb = get_global_triedb();
            let path_db = triedb.get_mut_path_db_ref().clone();
            MinerTrieDbPrefetcher::new(parent_header.state_root(), path_db, Some(difflayers)).ok()
        });

        (parent_difflayers, triedb_prefetcher)
    }

    fn record_finished_block(&mut self, difflayer: Option<Arc<DiffLayer>>) {
        self.parent_difflayers = difflayer.map(|difflayer| {
            let mut layers = DiffLayers::default();
            layers.insert_difflayer(difflayer);
            layers
        });
    }

    fn parent_difflayers(&self) -> Option<DiffLayers> {
        self.parent_difflayers.clone()
    }
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
        println!("\n[2/6] TrieDB enabled (difflayer flush + prefetch fast path active)");
    } else {
        println!("\n[2/6] TrieDB disabled (use --triedb to enable)");
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

    let turn_length = parent_snapshot.turn_length.unwrap_or(1) as usize;
    let num_validators = init.validator_addresses.len();
    let mut triedb_ctx = BenchTriedbContext::default();

    // Total blocks: 1 setup + N benchmark
    let total_blocks = config.num_blocks + 1;

    for block_idx in start_block_idx..total_blocks {
        let block_number = block_idx as u64 + 1;
        let is_setup_block = block_idx == 0;

        // Determine the in-turn validator using Parlia rotation logic
        let validator_index = determine_validator_index(block_number, num_validators, turn_length);
        let validator_addr = init.validator_addresses[validator_index];

        let has_cached_reads = cached_reads.is_some();

        let block_start = Instant::now();

        // Create MiningContext
        let mut mining_ctx = MiningContext {
            header: None,
            parent_header: parent_header.clone(),
            parent_snapshot: Arc::new(parent_snapshot.clone()),
            is_inturn: true,
            cached_reads: None,
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
            .latest()
            .map_err(|e| eyre::eyre!("Failed to get state for block {}: {:?}", block_number, e))?;

        // Wrap state provider for the EVM State DB
        let state_db = StateProviderDatabase::new(&*state_provider);

        // Build State with CachedReads (matches the real miner pipeline exactly)
        let mut cached: CachedReads = cached_reads.take().unwrap_or_default();
        let mut db =
            State::builder().with_database(cached.as_db_mut(state_db)).with_bundle_update().build();

        let state_setup_us = state_setup_start.elapsed().as_micros();

        // Build the block using the EVM config pipeline
        let pre_exec_start = Instant::now();
        let (parent_difflayers, triedb_prefetcher) =
            triedb_ctx.next_block_context(config.triedb, &parent_header);

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
                    parent_difflayers,
                    triedb_prefetcher: triedb_prefetcher.clone(),
                    validator_cache_sink: None,
                    turn_length_sink: None,
                },
            )
            .map_err(|e| {
                eyre::eyre!("Failed to create block builder for block {}: {:?}", block_number, e)
            })?;

        if let Some(prefetcher) = triedb_prefetcher {
            let pf = prefetcher.clone();
            builder.executor_mut().set_state_hook(Some(Box::new(
                move |_, update: &RethEvmState| {
                    pf.on_state_update(update);
                },
            )));
        }

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
        let finish_state_provider = init.factory.latest().map_err(|e| {
            eyre::eyre!("Failed to get finish state for block {}: {:?}", block_number, e)
        })?;

        let finish_start = Instant::now();

        let outcome = builder
            .finish_with_difflayer(&*finish_state_provider)
            .map_err(|e| eyre::eyre!("Block {} finish failed: {:?}", block_number, e))?;

        let finish_us = finish_start.elapsed().as_micros();

        // Extract metrics from the outcome before committing
        let hashed_accounts = outcome.inner.hashed_state.accounts.len();
        let hashed_storage_slots: usize =
            outcome.inner.hashed_state.storages.values().map(|s| s.storage.len()).sum();

        let difflayer = outcome.difflayer;
        let BlockBuilderOutcome { execution_result, block: recovered_block, .. } = outcome.inner;

        // Get the block header info we need before consuming the block
        let new_header = recovered_block.header().clone();
        let new_hash = new_header.hash_slow();
        let state_root = new_header.state_root;

        // Get a read-write database provider for committing
        let provider_rw = init.factory.database_provider_rw().map_err(|e| {
            eyre::eyre!("Failed to get provider_rw for block {}: {:?}", block_number, e)
        })?;
        let commit_start = Instant::now();
        let mut triedb_flush_us = 0u128;

        // Insert the block into the database
        let insert_block_start = Instant::now();
        provider_rw
            .insert_block(&recovered_block)
            .map_err(|e| eyre::eyre!("Failed to insert block {}: {:?}", block_number, e))?;
        let insert_block_us = insert_block_start.elapsed().as_micros();

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

        // Build ExecutionOutcome from the bundle state and write it
        let execution_outcome = ExecutionOutcome::new(
            db.take_bundle(),
            vec![execution_result.receipts],
            block_number,
            vec![execution_result.requests],
        );

        let write_state_start = Instant::now();
        provider_rw
            .write_state(&execution_outcome, OriginalValuesKnown::No, StateWriteConfig::default())
            .map_err(|e| {
                eyre::eyre!("Failed to write state for block {}: {:?}", block_number, e)
            })?;
        let write_state_us = write_state_start.elapsed().as_micros();

        if config.triedb {
            let mut triedb = rust_eth_triedb::get_global_triedb();
            let triedb_flush_start = Instant::now();
            // Keep the direct benchmark's persistence semantics unchanged: reuse the latest
            // difflayer in-memory to speed up the next block's finish path, but continue to
            // persist only the canonical root metadata here.
            triedb.flush(block_number, state_root, &None).map_err(|e| {
                eyre::eyre!("Failed to flush triedb for block {}: {:?}", block_number, e)
            })?;
            triedb_flush_us = triedb_flush_start.elapsed().as_micros();
        }

        // Commit the transaction (static files + DB)
        let provider_commit_start = Instant::now();
        provider_rw
            .commit()
            .map_err(|e| eyre::eyre!("Failed to commit block {}: {:?}", block_number, e))?;
        let provider_commit_us = provider_commit_start.elapsed().as_micros();

        let commit_us = commit_start.elapsed().as_micros();
        let total_us = block_start.elapsed().as_micros();

        // Populate CachedReads for next block from execution outcome
        cached_reads = Some(new_cached);
        triedb_ctx.record_finished_block(difflayer.clone());

        // Update chain tip
        let new_sealed = SealedHeader::new(new_header.clone(), new_hash);
        insert_header_to_cache(new_header);

        parent_header = new_sealed;
        parent_snapshot.block_number = block_number;
        parent_snapshot.block_hash = new_hash;

        if is_setup_block {
            db_init::persist_post_setup_cache(&config, &init, &parent_header, &parent_snapshot)?;
            // Setup block -- do NOT record timing
            println!(
                "  Setup block 0 complete: {} txs, {} gas used, {}us total (commit: {}us)",
                tx_count, gas_used, total_us, commit_us
            );
            continue;
        }

        // Record timing for benchmark blocks
        let timing = BlockTiming {
            block_number,
            validator_index,
            tx_count: tx_count as usize,
            gas_used,
            state_setup_us,
            pre_execution_us,
            execute_only_us,
            tx_execution_us,
            insert_block_us,
            write_state_us,
            triedb_flush_us,
            provider_commit_us,
            commit_us,
            finish_us,
            total_us,
            hashed_accounts,
            hashed_storage_slots,
            has_cached_reads,
        };

        if block_number.is_multiple_of(10) || block_number <= 2 {
            let flags = if has_cached_reads { "C" } else { "-" };
            println!(
                "  Block {:>4} | v[{}] {} | txs: {:>3} | gas: {:>8} | finish: {:>6}us | commit: {:>6}us | total: {:>6}us",
                block_number,
                validator_index,
                flags,
                tx_count,
                gas_used,
                finish_us,
                commit_us,
                total_us,
            );
        }

        timings.push(timing);
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
    use rust_eth_triedb_common::DiffLayer;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn sample_difflayer(marker: u8) -> Arc<DiffLayer> {
        let mut storage_roots = HashMap::new();
        storage_roots.insert(B256::repeat_byte(marker), B256::repeat_byte(marker.wrapping_add(1)));
        Arc::new(DiffLayer::new(Arc::new(HashMap::new()), Arc::new(storage_roots)))
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
    fn triedb_context_keeps_only_latest_parent_difflayer() {
        let mut ctx = BenchTriedbContext::default();
        let first = sample_difflayer(0x11);
        let second = sample_difflayer(0x22);

        ctx.record_finished_block(Some(first.clone()));
        let first_layers = ctx.parent_difflayers().expect("first difflayer should be tracked");
        assert_eq!(first_layers.diff_layers.len(), 1);
        assert_eq!(first_layers.diff_layers[0].diff_storage_roots, first.diff_storage_roots);

        ctx.record_finished_block(Some(second.clone()));
        let second_layers =
            ctx.parent_difflayers().expect("second difflayer should replace the first");
        assert_eq!(second_layers.diff_layers.len(), 1);
        assert_eq!(second_layers.diff_layers[0].diff_storage_roots, second.diff_storage_roots);
    }

    #[test]
    fn triedb_context_clears_parent_difflayers_when_finish_produces_none() {
        let mut ctx = BenchTriedbContext::default();
        ctx.record_finished_block(Some(sample_difflayer(0x33)));

        ctx.record_finished_block(None);

        assert!(ctx.parent_difflayers().is_none());
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
