//! Test runner for blockchain tests using BSC chain specs
//!
//! This test runner validates that the BSC hardfork rules are compatible with
//! standard Ethereum execution spec tests. It uses the BSC chain spec (with BSC
//! hardfork rules) but the standard Ethereum block executor, since the EF tests
//! use standard Ethereum block types.
//!
//! For BSC-specific features (system transactions, custom precompiles), separate
//! integration tests should be used.

use crate::{
    models::{BlockchainTest, ForkSpec},
    result::ExecutionWitness,
    Case, Error, Suite,
};
use alloy_rlp::Decodable;
use rayon::iter::{ParallelBridge, ParallelIterator};
use reth_bsc::chainspec::BscChainSpec;
use reth_chainspec::ChainSpec;
use reth_db_common::init::{insert_genesis_hashes, insert_genesis_history, insert_genesis_state};
use reth_ethereum_primitives::Block;
use reth_evm::{execute::Executor, ConfigureEvm};
use reth_evm_ethereum::EthEvmConfig;
use reth_primitives_traits::{RecoveredBlock, SealedBlock};
use reth_provider::{
    test_utils::create_test_provider_factory_with_chain_spec, BlockWriter, DatabaseProviderFactory,
    ExecutionOutcome, HistoryWriter, OriginalValuesKnown, StateWriteConfig, StateWriter,
    StaticFileProviderFactory, StaticFileSegment, StaticFileWriter,
};
use reth_revm::{database::StateProviderDatabase, State};
use reth_trie::{HashedPostState, KeccakKeyHasher, StateRoot};
use reth_trie_db::DatabaseStateRoot;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

/// A handler for the blockchain test suite.
#[derive(Debug)]
pub struct BlockchainTests {
    suite_path: PathBuf,
}

impl BlockchainTests {
    /// Create a new suite for tests with blockchain tests format.
    pub const fn new(suite_path: PathBuf) -> Self {
        Self { suite_path }
    }
}

impl Suite for BlockchainTests {
    type Case = BlockchainTestCase;

    fn suite_path(&self) -> &Path {
        &self.suite_path
    }
}

/// An Ethereum blockchain test.
#[derive(Debug, PartialEq, Eq)]
pub struct BlockchainTestCase {
    /// The tests within this test case.
    pub tests: BTreeMap<String, BlockchainTest>,
    /// Whether to skip this test case.
    pub skip: bool,
}

impl BlockchainTestCase {
    /// Returns `true` if the fork is not supported.
    const fn excluded_fork(network: ForkSpec) -> bool {
        matches!(
            network,
            ForkSpec::ByzantiumToConstantinopleAt5
                | ForkSpec::Constantinople
                | ForkSpec::ConstantinopleFix
                | ForkSpec::MergeEOF
                | ForkSpec::MergeMeterInitCode
                | ForkSpec::MergePush0
        )
    }

    /// If the test expects an exception, return the block number
    /// at which it must occur together with the original message.
    #[inline]
    fn expected_failure(case: &BlockchainTest) -> Option<(u64, String)> {
        case.blocks.iter().enumerate().find_map(|(idx, blk)| {
            blk.expect_exception.as_ref().map(|msg| ((idx + 1) as u64, msg.clone()))
        })
    }

    /// Execute a single `BlockchainTest`, validating the outcome against the
    /// expectations encoded in the JSON file.
    pub fn run_single_case(
        name: &str,
        case: &BlockchainTest,
    ) -> Result<Vec<(RecoveredBlock<Block>, ExecutionWitness)>, Error> {
        let expectation = Self::expected_failure(case);
        match run_case(case) {
            Ok(program_inputs) => {
                if let Some((block, msg)) = expectation {
                    Err(Error::Assertion(format!(
                        "Test case: {name}\nExpected failure at block {block} - {msg}, but all blocks succeeded",
                    )))
                } else {
                    Ok(program_inputs)
                }
            }

            Err(Error::BlockProcessingFailed { block_number, partial_program_inputs, err }) => {
                match expectation {
                    Some((expected, _)) if block_number == expected => Ok(partial_program_inputs),
                    Some((expected, _)) => Err(Error::Assertion(format!(
                        "Test case: {name}\nExpected failure at block {expected}\nGot failure at block {block_number}",
                    ))),
                    None => Err(Error::BlockProcessingFailed { block_number, partial_program_inputs, err }),
                }
            }

            Err(other) => Err(other),
        }
    }
}

impl Case for BlockchainTestCase {
    fn load(path: &Path) -> Result<Self, Error> {
        Ok(Self {
            tests: {
                let s = fs::read_to_string(path)
                    .map_err(|error| Error::Io { path: path.into(), error })?;
                serde_json::from_str(&s)
                    .map_err(|error| Error::CouldNotDeserialize { path: path.into(), error })?
            },
            skip: should_skip(path),
        })
    }

    fn run(&self) -> Result<(), Error> {
        if self.skip {
            return Err(Error::Skipped);
        }

        self.tests
            .iter()
            .filter(|(_, case)| !Self::excluded_fork(case.network))
            .par_bridge()
            .try_for_each(|(name, case)| Self::run_single_case(name, case).map(|_| ()))?;

        Ok(())
    }
}

/// Executes a single `BlockchainTest` using BSC chain specs.
///
/// This uses the BSC chain specification (with BSC hardfork rules) but executes
/// using the standard Ethereum executor, since the EF tests use standard Ethereum
/// block types.
fn run_case(
    case: &BlockchainTest,
) -> Result<Vec<(RecoveredBlock<Block>, ExecutionWitness)>, Error> {
    // Create a BSC chain spec from the network fork specification
    let bsc_chain_spec: Arc<BscChainSpec> = case.network.into();

    // Extract the inner ChainSpec which contains the hardfork rules
    let chain_spec: Arc<ChainSpec> = Arc::new(bsc_chain_spec.inner.clone());

    // Create the test provider factory with our chain spec
    let factory = create_test_provider_factory_with_chain_spec(chain_spec.clone());
    let provider = factory.database_provider_rw().unwrap();

    // Insert initial test state into the provider.
    let genesis_block = SealedBlock::<Block>::from_sealed_parts(
        case.genesis_block_header.clone().into(),
        Default::default(),
    )
    .try_recover()
    .unwrap();

    provider
        .insert_block(&genesis_block)
        .map_err(|err| Error::block_failed(0, Default::default(), err))?;

    // Increment block number for receipts static file
    provider
        .static_file_provider()
        .latest_writer(StaticFileSegment::Receipts)
        .and_then(|mut writer| writer.increment_block(0))
        .map_err(|err| Error::block_failed(0, Default::default(), err))?;

    let genesis_state = case.pre.clone().into_genesis_state();
    insert_genesis_state(&provider, genesis_state.iter())
        .map_err(|err| Error::block_failed(0, Default::default(), err))?;
    insert_genesis_hashes(&provider, genesis_state.iter())
        .map_err(|err| Error::block_failed(0, Default::default(), err))?;
    insert_genesis_history(&provider, genesis_state.iter())
        .map_err(|err| Error::block_failed(0, Default::default(), err))?;

    // Decode blocks
    let blocks = decode_blocks(&case.blocks)?;

    // Use the standard Ethereum executor with BSC chain spec (for hardfork rules)
    let executor_provider = EthEvmConfig::ethereum(chain_spec.clone());

    let mut _parent = genesis_block;
    let mut program_inputs = Vec::new();

    for (block_index, block) in blocks.iter().enumerate() {
        let block_number = (block_index + 1) as u64;

        // Insert the block into the database
        provider
            .insert_block(block)
            .map_err(|err| Error::block_failed(block_number, Default::default(), err))?;
        provider
            .static_file_provider()
            .commit()
            .map_err(|err| Error::block_failed(block_number, Default::default(), err))?;

        // Execute the block
        let state_provider = provider.latest();
        let state_db = StateProviderDatabase::new(&state_provider);
        let executor = executor_provider.batch_executor(state_db);

        // Execute with state closure to get the full output including state
        let output = executor
            .execute_with_state_closure_always(&(*block).clone(), |_state: &State<_>| {
                // State recorded for witness generation if needed
            })
            .map_err(|err| Error::block_failed(block_number, program_inputs.clone(), err))?;

        // Build execution witness (simplified)
        let exec_witness = ExecutionWitness::default();
        program_inputs.push((block.clone(), exec_witness));

        // Compute and check the post state root
        let hashed_state =
            HashedPostState::from_bundle_state::<KeccakKeyHasher>(output.state.state());
        let (computed_state_root, _) = StateRoot::overlay_root_with_updates(
            provider.tx_ref(),
            &hashed_state.clone_into_sorted(),
        )
        .map_err(|err| Error::block_failed(block_number, program_inputs.clone(), err))?;

        if computed_state_root != block.state_root {
            return Err(Error::block_failed(
                block_number,
                program_inputs.clone(),
                Error::Assertion(format!(
                    "state root mismatch: computed {computed_state_root:?}, expected {:?}",
                    block.state_root
                )),
            ));
        }

        // Commit the post state to the database
        provider
            .write_state(
                &ExecutionOutcome::single(block.number, output),
                OriginalValuesKnown::Yes,
                StateWriteConfig::default(),
            )
            .map_err(|err| Error::block_failed(block_number, program_inputs.clone(), err))?;

        provider
            .write_hashed_state(&hashed_state.into_sorted())
            .map_err(|err| Error::block_failed(block_number, program_inputs.clone(), err))?;
        provider
            .update_history_indices(block.number..=block.number)
            .map_err(|err| Error::block_failed(block_number, program_inputs.clone(), err))?;

        _parent = block.clone();
    }

    // Validate the post-state
    if let Some(expected_post_state) = &case.post_state {
        for (address, account) in expected_post_state {
            account.assert_db(*address, provider.tx_ref())?;
        }
    }

    Ok(program_inputs)
}

fn decode_blocks(
    test_case_blocks: &[crate::models::Block],
) -> Result<Vec<RecoveredBlock<Block>>, Error> {
    let mut blocks = Vec::with_capacity(test_case_blocks.len());
    for (block_index, block) in test_case_blocks.iter().enumerate() {
        let block_number = (block_index + 1) as u64;

        let decoded = SealedBlock::<Block>::decode(&mut block.rlp.as_ref())
            .map_err(|err| Error::block_failed(block_number, Default::default(), err))?;

        let recovered_block = decoded
            .clone()
            .try_recover()
            .map_err(|err| Error::block_failed(block_number, Default::default(), err))?;

        blocks.push(recovered_block);
    }

    Ok(blocks)
}

/// Returns whether the test at the given path should be skipped.
pub fn should_skip(path: &Path) -> bool {
    let name = path.file_name().unwrap().to_str().unwrap();
    matches!(
        name,
        // funky test with `bigint 0x00` value in json
        | "ValueOverflow.json"
        | "ValueOverflowParis.json"
        // txbyte is of type 02 and we don't parse tx bytes for this test to fail.
        | "typeTwoBerlin.json"
        // Test checks if nonce overflows.
        | "CreateTransactionHighNonce.json"
        // Test check if gas price overflows
        | "HighGasPrice.json"
        | "HighGasPriceParis.json"
        // Skip test where basefee/accesslist/difficulty is present but it shouldn't be supported
        | "accessListExample.json"
        | "basefeeExample.json"
        | "eip1559.json"
        | "mergeTest.json"
        // These tests take a lot of time to execute
        | "loopExp.json"
        | "Call50000_sha256.json"
        | "static_Call50000_sha256.json"
        | "loopMul.json"
        | "CALLBlake2f_MaxRounds.json"
        | "shiftCombinations.json"
        // Skipped by revm as well
        | "RevertInCreateInInit_Paris.json"
        | "RevertInCreateInInit.json"
        | "dynamicAccountOverwriteEmpty.json"
        | "dynamicAccountOverwriteEmpty_Paris.json"
        | "RevertInCreateInInitCreate2Paris.json"
        | "create2collisionStorage.json"
        | "RevertInCreateInInitCreate2.json"
        | "create2collisionStorageParis.json"
        | "InitCollision.json"
        | "InitCollisionParis.json"
        // =============================================================================
        // EIP-7610: Revert creation in case of non-empty storage
        // https://eips.ethereum.org/EIPS/eip-7610
        // These tests are for CREATE/CREATE2 collision when target address has
        // non-empty storage (nonce=0, code=empty, but storage exists).
        // EIP-7610 is scheduled for Osaka/Fusaka hardfork, not Cancun.
        // revm's SpecId::CANCUN does not include EIP-7610 support.
        // =============================================================================
        | "test_init_collision_create_opcode.json"
        | "test_init_collision_create_tx.json"

        // =============================================================================
        // Block Header Validation Tests
        // These tests require consensus-level block validation that rejects invalid
        // blocks BEFORE execution. The test harness uses NoProof consensus and
        // EthEvmConfig which only validates EVM execution, not block headers.
        // These would need a full consensus engine (like Parlia for BSC) to validate.
        // =============================================================================

        // Gas limit validation (EIP-150: minimum gas limit is 5000)
        // Test sets gasLimit=0, expects BlockException.INVALID_GASLIMIT
        | "test_gas_limit_below_minimum.json"

        // Base fee validation (EIP-1559)
        // Test sets invalid base_fee_per_gas, expects BlockException.INVALID_BASEFEE_PER_GAS
        | "test_invalid_header.json"

        // Withdrawals root validation (EIP-4895)
        // Test provides mismatched withdrawals_root, expects BlockException.INVALID_WITHDRAWALS_ROOT
        | "test_withdrawals_root.json"

        // =============================================================================
        // EIP-4844 Blob Gas Validation Tests
        // These tests verify that blocks with invalid blob gas fields are rejected.
        // The test harness doesn't validate excess_blob_gas or blob_gas_used in headers.
        // Invalid blocks execute instead of being rejected, causing balance mismatches.
        // =============================================================================
        | "test_invalid_excess_blob_gas_target_blobs_increase_from_zero.json"
        | "test_invalid_non_multiple_excess_blob_gas.json"
        | "test_invalid_static_excess_blob_gas.json"
        | "test_invalid_excess_blob_gas_above_target_change.json"
        | "test_invalid_excess_blob_gas_change.json"
        | "test_invalid_zero_excess_blob_gas_in_header.json"
        | "test_invalid_blob_gas_used_in_header.json"
        | "test_invalid_static_excess_blob_gas_from_zero_on_blobs_above_target.json"
        // Tests blob fields presence before Cancun fork activation
        | "test_invalid_pre_fork_block_with_blob_fields.json"

        // =============================================================================
        // Prague (EIP-7685) Request Validation Tests
        // These tests verify that blocks with invalid execution layer requests
        // (deposits, withdrawals, consolidations) are rejected at consensus level.
        // The test harness doesn't validate requests_hash in block headers.
        // =============================================================================

        // EIP-7002: Execution layer triggerable withdrawals
        // Tests invalid withdrawal request lists, expects BlockException.INVALID_REQUESTS
        | "test_withdrawal_requests_negative.json"

        // EIP-6110: Supply validator deposits on chain
        // Tests invalid deposit lists, expects BlockException.INVALID_REQUESTS
        | "test_deposit_negative.json"

        // EIP-7251: Increase MAX_EFFECTIVE_BALANCE (consolidations)
        // Tests invalid consolidation lists, expects BlockException.INVALID_REQUESTS
        | "test_consolidation_requests_negative.json"

        // Tests multiple request types with invalid data
        | "test_invalid_multi_type_requests.json"

        // System contract deployment validation
        // Tests expect BlockException.SYSTEM_CONTRACT_EMPTY when system contracts
        // (deposit, withdrawal, consolidation contracts) don't exist at fork activation
        | "test_system_contract_deployment.json"

        // Prague deposit contract log validation
        // These test invalid deposit log formats from the deposit contract
        | "test_invalid_log_length.json"
        | "test_invalid_layout.json"
    )
}
