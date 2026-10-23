//! Regression checks for the consensus callbacks used during block import.
//! Fixtures are unsigned, in-memory blocks; no engine, networking or persistence is started.
use alloy_consensus::{BlockBody, Header, EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH};
use alloy_eips::eip4895::Withdrawal;
use alloy_primitives::{Address, B256};
use reth::consensus::{Consensus, ConsensusError};
use reth_bsc::{
    chainspec::{bsc::bsc_mainnet, BscChainSpec},
    consensus::parlia::EMPTY_WITHDRAWALS_HASH,
    node::consensus::BscConsensus,
    BscBlock, BscBlockBody,
};
use reth_primitives_traits::SealedBlock;
use std::sync::Arc;

fn consensus() -> BscConsensus<BscChainSpec> {
    BscConsensus::new(Arc::new(BscChainSpec::from(bsc_mainnet())))
}

fn fixture() -> BscBlock {
    BscBlock {
        header: Header {
            number: 120_000_001,
            timestamp: 1_788_000_000,
            transactions_root: EMPTY_ROOT_HASH,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            withdrawals_root: Some(EMPTY_WITHDRAWALS_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            ..Default::default()
        },
        body: BscBlockBody {
            inner: BlockBody {
                transactions: vec![],
                ommers: vec![],
                withdrawals: Some(Default::default()),
            },
            sidecars: None,
        },
    }
}

fn import_verdicts(
    consensus: &BscConsensus<BscChainSpec>,
    block: &SealedBlock<BscBlock>,
) -> [Result<(), ConsensusError>; 3] {
    [
        consensus.validate_block_pre_execution(block),
        consensus.validate_block_pre_execution_with_tx_root(block, None),
        // Fixtures have no transactions, so this is their correctly computed root.
        consensus.validate_block_pre_execution_with_tx_root(block, Some(EMPTY_ROOT_HASH)),
    ]
}

fn assert_import_rejects(
    consensus: &BscConsensus<BscChainSpec>,
    block: &SealedBlock<BscBlock>,
    expected_error: fn(&ConsensusError) -> bool,
) {
    let results = import_verdicts(consensus, block);
    assert!(
        results.iter().all(|result| matches!(result, Err(error) if expected_error(error))),
        "body commitment failure escaped import validation: [direct, engine(None), engine(Some)] = {results:?}",
    );
}

#[test]
fn matching_body_commitments_pass_pre_execution() {
    let consensus = consensus();
    let block = SealedBlock::seal_slow(fixture());
    assert!(consensus.validate_body_against_header(block.body(), block.sealed_header()).is_ok());
    let results = import_verdicts(&consensus, &block);
    assert!(results.iter().all(Result::is_ok), "valid control rejected: {results:?}");
}

#[test]
fn withdrawals_must_match_header_commitment() {
    let consensus = consensus();
    let mut block = fixture();
    block.body.inner.withdrawals = Some(
        vec![Withdrawal { index: 0, validator_index: 0, address: Address::ZERO, amount: 0 }].into(),
    );
    let block = SealedBlock::seal_slow(block);
    assert!(matches!(
        consensus.validate_body_against_header(block.body(), block.sealed_header()),
        Err(ConsensusError::BodyWithdrawalsRootDiff(_)),
    ));
    assert_import_rejects(&consensus, &block, |error| {
        matches!(error, ConsensusError::BodyWithdrawalsRootDiff(_))
    });
}

#[test]
fn withdrawals_presence_must_match_header_commitment() {
    let consensus = consensus();
    let mut block = fixture();
    block.body.inner.withdrawals = None;
    let block = SealedBlock::seal_slow(block);
    assert!(matches!(
        consensus.validate_body_against_header(block.body(), block.sealed_header()),
        Err(ConsensusError::WithdrawalsRootUnexpected),
    ));
    assert_import_rejects(&consensus, &block, |error| {
        matches!(error, ConsensusError::WithdrawalsRootUnexpected)
    });
}

#[test]
fn ommers_must_match_header_commitment() {
    let consensus = consensus();
    let mut block = fixture();
    block.body.inner.ommers.push(Header::default());
    let block = SealedBlock::seal_slow(block);
    assert!(matches!(
        consensus.validate_body_against_header(block.body(), block.sealed_header()),
        Err(ConsensusError::BodyOmmersHashDiff(_)),
    ));
    assert_import_rejects(&consensus, &block, |error| {
        matches!(error, ConsensusError::BodyOmmersHashDiff(_))
    });
}

#[test]
fn transactions_must_match_header_commitment() {
    let consensus = consensus();
    let mut block = fixture();
    block.header.transactions_root = B256::ZERO;
    let block = SealedBlock::seal_slow(block);
    let results = import_verdicts(&consensus, &block);
    assert!(
        results
            .iter()
            .all(|result| matches!(result, Err(ConsensusError::BodyTransactionRootDiff(_)),)),
        "transaction commitment escaped import validation: {results:?}"
    );
}

fn pre_cancun_fixture() -> BscBlock {
    let mut block = fixture();
    block.header.number = 1;
    block.header.timestamp = 4;
    block.header.withdrawals_root = None;
    block.header.blob_gas_used = None;
    block.header.excess_blob_gas = None;
    block.body.inner.withdrawals = None;
    block
}

#[test]
fn pre_cancun_body_without_withdrawals_passes() {
    let consensus = consensus();
    let block = SealedBlock::seal_slow(pre_cancun_fixture());
    assert!(consensus.validate_body_against_header(block.body(), block.sealed_header()).is_ok());
    let results = import_verdicts(&consensus, &block);
    assert!(results.iter().all(Result::is_ok), "valid pre-Cancun body rejected: {results:?}");
}

#[test]
fn withdrawals_require_a_header_commitment_even_when_empty() {
    let consensus = consensus();
    let mut block = pre_cancun_fixture();
    block.body.inner.withdrawals = Some(Default::default());
    let block = SealedBlock::seal_slow(block);
    assert!(matches!(
        consensus.validate_body_against_header(block.body(), block.sealed_header()),
        Err(ConsensusError::WithdrawalsRootUnexpected),
    ));
    assert_import_rejects(&consensus, &block, |error| {
        matches!(error, ConsensusError::WithdrawalsRootUnexpected)
    });
}

#[test]
fn matching_commitments_do_not_skip_bsc_blob_gas_validation() {
    let consensus = consensus();
    let mut block = fixture();
    block.header.blob_gas_used = Some(alloy_eips::eip4844::DATA_GAS_PER_BLOB);
    let block = SealedBlock::seal_slow(block);
    assert!(consensus.validate_body_against_header(block.body(), block.sealed_header()).is_ok());
    assert_import_rejects(&consensus, &block, |error| {
        matches!(error, ConsensusError::BlobGasUsedDiff(_))
    });
}
