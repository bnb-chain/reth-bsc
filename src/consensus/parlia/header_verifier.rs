//! Shared, execution-free Parlia header checks for import and block relay.
use super::{
    constants::K_ANCESTOR_GENERATION_DEPTH, util::debug_header, vote::MAX_ATTESTATION_EXTRA_LENGTH,
    Parlia, Snapshot, SnapshotProvider, VoteAddress, DIFF_INTURN, DIFF_NOTURN,
};
use crate::{
    hardforks::BscHardforks,
    metrics::BscVoteMetrics,
    node::evm::error::{BscBlockExecutionError, BscBlockValidationError},
};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::B256;
use bit_set::BitSet;
use blst::{
    min_pk::{PublicKey, Signature},
    BLST_ERROR,
};
use reth_chainspec::EthChainSpec;
use reth_evm::execute::BlockExecutionError;
use reth_primitives_traits::GotExpected;

const BLST_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

pub(crate) struct HeaderVerifier<'a, Spec> {
    parlia: &'a Parlia<Spec>,
    snapshot_provider: Option<&'a (dyn SnapshotProvider + Send + Sync)>,
    vote_metrics: &'a BscVoteMetrics,
}

impl<'a, Spec: EthChainSpec + BscHardforks + 'static> HeaderVerifier<'a, Spec> {
    pub(crate) fn new(
        parlia: &'a Parlia<Spec>,
        snapshot_provider: Option<&'a (dyn SnapshotProvider + Send + Sync)>,
        vote_metrics: &'a BscVoteMetrics,
    ) -> Self {
        Self { parlia, snapshot_provider, vote_metrics }
    }

    pub(crate) fn verify(
        &self,
        header: &Header,
        parent: &Header,
        snap: &Snapshot,
    ) -> Result<(), BlockExecutionError> {
        self.verify_block_time_for_ramanujan(snap, header, parent)?;

        // Verify vote attestation and track errors
        if let Err(err) = self.verify_vote_attestation(snap, header, parent) {
            // Update vote attestation error metric for all attestation-related errors
            self.vote_metrics.vote_attestation_errors_total.increment(1);
            return Err(err);
        }

        self.verify_seal(snap, header)?;

        Ok(())
    }

    fn verify_block_time_for_ramanujan(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        self.parlia.block_time_verify_for_ramanujan_fork(snap, header, parent)
    }

    fn verify_vote_attestation(
        &self,
        snap: &Snapshot,
        header: &Header,
        parent: &Header,
    ) -> Result<(), BlockExecutionError> {
        if !self.parlia.spec.is_plato_active_at_block(header.number()) {
            return Ok(());
        }

        let attestation = self
            .parlia
            .get_vote_attestation_from_header(header, snap.epoch_num)
            .map_err(|err| {
                tracing::error!(
                    "Failed to get vote attestation from header, block_number: {}, error: {:?}",
                    header.number(),
                    err
                );
                BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError {
                    error: err.into(),
                })
            })?;
        if let Some(attestation) = attestation {
            if attestation.extra.len() > MAX_ATTESTATION_EXTRA_LENGTH {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::TooLargeAttestationExtraLen {
                        extra_len: MAX_ATTESTATION_EXTRA_LENGTH,
                    },
                )
                .into());
            }

            // the attestation target block should be direct parent.
            let target_block = attestation.data.target_number;
            let target_hash = attestation.data.target_hash;
            let mut is_match = false;
            let mut ancestor = parent.clone();
            let depth =
                if self.parlia.spec.is_fermi_active_at_timestamp(header.number(), header.timestamp)
                {
                    K_ANCESTOR_GENERATION_DEPTH
                } else {
                    1
                };
            for _ in 0..depth {
                if ancestor.number() == target_block && ancestor.hash_slow() == target_hash {
                    is_match = true;
                    break;
                }
                ancestor =
                    crate::node::evm::util::get_header_by_hash_from_cache(&ancestor.parent_hash())
                        .ok_or_else(|| BscBlockExecutionError::UnknownHeader {
                            block_hash: ancestor.parent_hash(),
                        })?;
                tracing::debug!("ancestor: {:?}", ancestor);
            }

            if !is_match {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationTarget {
                        block_number: GotExpected {
                            got: target_block,
                            expected: ancestor.number(),
                        },
                        block_hash: GotExpected {
                            got: target_hash,
                            expected: ancestor.hash_slow(),
                        }
                        .into(),
                    },
                )
                .into());
            }

            // the attestation source block should be the highest justified block.
            let source_block = attestation.data.source_number;
            let source_hash = attestation.data.source_hash;

            let justified = self.get_justified_header(snap)?;
            if source_block != justified.number() || source_hash != justified.hash_slow() {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationSource {
                        block_number: GotExpected {
                            got: source_block,
                            expected: justified.number(),
                        },
                        block_hash: GotExpected {
                            got: source_hash,
                            expected: justified.hash_slow(),
                        }
                        .into(),
                    },
                )
                .into());
            }

            let pre_snap = self
                .snapshot_provider
                .as_ref()
                .ok_or_else(|| BlockExecutionError::msg("Snapshot provider is not available"))?
                .snapshot_by_hash(&ancestor.parent_hash)
                .ok_or(BlockExecutionError::msg(
                    "Failed to get pre snapshot from snapshot provider",
                ))?;

            // query bls keys from snapshot.
            let validators_count = pre_snap.validators.len();
            let vote_bit_set: BitSet<usize> = BitSet::from_iter(
                (0..64).filter(|&i| (attestation.vote_address_set >> i) & 1 != 0),
            );
            let bit_set_count = vote_bit_set.len();
            if bit_set_count > validators_count {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationVoteCount(GotExpected {
                        got: bit_set_count as u64,
                        expected: validators_count as u64,
                    }),
                )
                .into());
            }

            let mut vote_addrs: Vec<VoteAddress> = Vec::with_capacity(bit_set_count);
            for (i, val) in pre_snap.validators.iter().enumerate() {
                if !vote_bit_set.contains(i) {
                    continue;
                }

                let val_info = pre_snap
                    .validators_map
                    .get(val)
                    .ok_or(BscBlockExecutionError::VoteAddrNotFoundInSnap { address: *val })?;
                vote_addrs.push(val_info.vote_addr);
            }

            // check if voted validator count satisfied 2/3 + 1
            let at_least_votes = (validators_count * 2).div_ceil(3); // ceil division
            if vote_addrs.len() < at_least_votes {
                return Err(BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationVoteCount(GotExpected {
                        got: vote_addrs.len() as u64,
                        expected: at_least_votes as u64,
                    }),
                )
                .into());
            }

            // check bls aggregate sig
            let mut pubkeys: Vec<PublicKey> = Vec::with_capacity(vote_addrs.len());
            for addr in &vote_addrs {
                match PublicKey::from_bytes(addr.as_slice()) {
                    Ok(pk) => pubkeys.push(pk),
                    Err(_) => {
                        return Err(BscBlockExecutionError::Validation(
                            BscBlockValidationError::InvalidAttestationSignature,
                        )
                        .into());
                    }
                }
            }
            let vote_addrs_ref: Vec<&PublicKey> = pubkeys.iter().collect();

            let sig = Signature::from_bytes(&attestation.agg_signature[..]).map_err(|_| {
                BscBlockExecutionError::Validation(
                    BscBlockValidationError::InvalidAttestationSignature,
                )
            })?;

            // Track BLS verification attempt
            self.vote_metrics.bls_verifications_total.increment(1);
            let start = std::time::Instant::now();

            let err = sig.fast_aggregate_verify(
                true,
                attestation.data.hash().as_slice(),
                BLST_DST,
                &vote_addrs_ref,
            );

            // Record verification duration
            self.vote_metrics
                .bls_verification_duration_seconds
                .record(start.elapsed().as_secs_f64());

            return match err {
                BLST_ERROR::BLST_SUCCESS => Ok(()),
                _ => {
                    // Update BLS verification failure metric (kept here as it's a specific metric)
                    self.vote_metrics.bls_verification_failures_total.increment(1);
                    Err(BscBlockExecutionError::Validation(
                        BscBlockValidationError::InvalidAttestationSignature,
                    )
                    .into())
                }
            };
        }

        Ok(())
    }

    fn verify_seal(&self, snap: &Snapshot, header: &Header) -> Result<(), BlockExecutionError> {
        let proposer = self.parlia.recover_proposer(header).map_err(|err| {
            tracing::error!(
                "Failed to recover proposer from header, block_number: {}, error: {:?}",
                header.number(),
                err
            );
            BscBlockExecutionError::Validation(BscBlockValidationError::ParliaConsensusError {
                error: err.into(),
            })
        })?;

        if proposer != header.beneficiary {
            tracing::error!(
                "Wrong header signer, block_number: {}, proposer: {:?}, expected: {:?}",
                header.number(),
                proposer,
                header.beneficiary
            );
            debug_header(header, self.parlia.spec.chain().id(), "verify_seal_header");
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::WrongHeaderSigner {
                    block_number: header.number(),
                    signer: GotExpected { got: proposer, expected: header.beneficiary }.into(),
                },
            )
            .into());
        }

        if !snap.validators.contains(&proposer) {
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::SignerUnauthorized {
                    block_number: header.number(),
                    proposer,
                },
            )
            .into());
        }

        if snap.sign_recently(proposer) {
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::SignerOverLimit { proposer },
            )
            .into());
        }

        let is_inturn = snap.is_inturn(proposer);
        if (is_inturn && header.difficulty != DIFF_INTURN) ||
            (!is_inturn && header.difficulty != DIFF_NOTURN)
        {
            let expected_difficulty = if is_inturn { DIFF_INTURN } else { DIFF_NOTURN };
            tracing::warn!(
                target: "bsc::validation",
                block_number = header.number(),
                block_hash = ?header.hash_slow(),
                proposer = ?proposer,
                is_inturn,
                actual_difficulty = %header.difficulty,
                expected_difficulty = %expected_difficulty,
                diff_inturn = %DIFF_INTURN,
                diff_noturn = %DIFF_NOTURN,
                "Block difficulty validation failed: mismatch between inturn status and difficulty"
            );
            return Err(BscBlockExecutionError::Validation(
                BscBlockValidationError::InvalidDifficulty { difficulty: header.difficulty },
            )
            .into());
        }

        Ok(())
    }

    fn get_justified_header(&self, snap: &Snapshot) -> Result<Header, BlockExecutionError> {
        if snap.vote_data.source_hash == B256::ZERO && snap.vote_data.target_hash == B256::ZERO {
            return crate::node::evm::util::get_cannonical_header_from_cache(0).ok_or_else(|| {
                BscBlockExecutionError::UnknownHeader { block_hash: B256::ZERO }.into()
            });
        }

        crate::node::evm::util::get_header_by_hash_from_cache(&snap.vote_data.target_hash)
            .ok_or_else(|| {
                BscBlockExecutionError::UnknownHeader { block_hash: snap.vote_data.target_hash }
                    .into()
            })
    }
}
