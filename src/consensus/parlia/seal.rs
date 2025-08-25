use super::{
    constants::{DIFF_NOTURN, EXTRA_SEAL_LEN},
    Snapshot, SnapshotProvider, VoteAddress, VoteAttestation, VoteData, VoteEnvelope,
    VoteSignature,
};
use crate::consensus::parlia::util::encode_header_with_chain_id;
use crate::{hardforks::BscHardforks, BscBlock};
use alloy_consensus::{BlockHeader, Header};
use alloy_primitives::{
    map::foldhash::{HashSet, HashSetExt},
    Address, Bytes, B256,
};
use blst::min_pk::{AggregateSignature, Signature as blsSignature};
use bytes::BytesMut;
use rand::Rng;
use reth::consensus::ConsensusError;
use reth_chainspec::EthChainSpec;
use reth_primitives::SealedBlock;
use reth_primitives_traits::{Block, SealedHeader};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

type SignFnPtr = fn(Address, &str, &[u8]) -> Result<[u8; 65], ConsensusError>;

pub struct SealBlock<ChainSpec> {
    snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
    chain_spec: Arc<ChainSpec>,

    validator_address: Address,
    sign_fn: SignFnPtr,
}

impl<ChainSpec> SealBlock<ChainSpec>
where
    ChainSpec: EthChainSpec + BscHardforks,
{
    #[allow(dead_code)]
    pub(crate) fn new(
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        chain_spec: Arc<ChainSpec>,
        validator_address: Address,
    ) -> Self {
        Self { snapshot_provider, chain_spec, validator_address, sign_fn: default_sign_fn }
    }

    #[allow(dead_code)]
    pub(crate) fn new_with_sign_fn(
        snapshot_provider: Arc<dyn SnapshotProvider + Send + Sync>,
        chain_spec: Arc<ChainSpec>,
        validator_address: Address,
        sign_fn: SignFnPtr,
    ) -> Self {
        Self { snapshot_provider, chain_spec, validator_address, sign_fn }
    }

    #[allow(dead_code)]
    fn update_sign_fn(&mut self, sign_fn: SignFnPtr) {
        self.sign_fn = sign_fn;
    }

    pub fn seal(self, block: BscBlock) -> Result<SealedBlock<BscBlock>, ConsensusError> {
        let header = block.header();
        if header.number == 0 {
            return Err(ConsensusError::Other(
                "Unknown block (genesis sealing not supported)".into(),
            ));
        }

        let val = self.validator_address;
        let sign_fn = self.sign_fn;

        let parent_number = header.number - 1;
        let snap = self
            .snapshot_provider
            .snapshot(parent_number)
            .ok_or_else(|| ConsensusError::Other("Snapshot not found".into()))?;

        if !snap.validators.contains(&val) {
            return Err(ConsensusError::Other(format!("Unauthorized validator: {val}").into()));
        }

        if snap.sign_recently(val) {
            tracing::info!("Signed recently, must wait for others");
            return Err(ConsensusError::Other(
                format!("Signed recently, must wait for others, validator: {val}").into(),
            ));
        }

        let delay = self.delay_for_ramanujan_fork(&snap, header);
        tracing::info!(
            target: "parlia::seal",
            "Sealing block {} (delay {:?}, difficulty {:?})",
            header.number,
            delay,
            header.difficulty
        );

        std::thread::sleep(delay);

        let mut header = block.header;
        if let Err(e) = self.assemble_vote_attestation_stub(&mut header) {
            tracing::error!(target: "parlia::seal", "Assemble vote attestation failed: {e}");
        }

        let mut out = BytesMut::new();
        encode_header_with_chain_id(&header, &mut out, self.chain_spec.chain_id());
        match sign_fn(val, "mimetype/parlia", out.iter().as_slice()) {
            Ok(sig) => {
                let mut extra = header.extra_data.to_vec();
                if extra.len() >= EXTRA_SEAL_LEN {
                    let start = extra.len() - EXTRA_SEAL_LEN;
                    extra[start..].copy_from_slice(&sig);
                    header.extra_data = Bytes::from(extra);
                } else {
                    tracing::error!(target: "parlia::seal", "extra_data too short to insert seal");
                }
            }
            Err(e) => {
                tracing::debug!(target: "parlia::seal", "Sign for the block header failed when sealing, err {e}")
            }
        }

        let block_hash = header.hash_slow();
        Ok(BscBlock::new_sealed(SealedHeader::new(header, block_hash), block.body))
    }

    fn delay_for_ramanujan_fork(&self, snapshot: &Snapshot, header: &Header) -> Duration {
        let now_secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();

        let mut delay = Duration::from_secs(header.timestamp().saturating_sub(now_secs));

        if self.chain_spec.is_ramanujan_active_at_block(header.number) {
            return delay;
        }

        if header.difficulty == DIFF_NOTURN {
            const FIXED_BACKOFF_TIME_BEFORE_FORK: Duration = Duration::from_millis(200);
            const WIGGLE_TIME_BEFORE_FORK: u64 = 500 * 1000 * 1000; // 500 ms

            let validators = snapshot.validators.len();
            let rand_wiggle = rand::rng()
                .random_range(0..(WIGGLE_TIME_BEFORE_FORK * (validators / 2 + 1) as u64));

            delay += FIXED_BACKOFF_TIME_BEFORE_FORK + Duration::from_nanos(rand_wiggle);
        }

        delay
    }

    fn assemble_vote_attestation_stub(&self, header: &mut Header) -> Result<(), ConsensusError> {
        if !self.chain_spec.is_luban_active_at_block(header.number()) || header.number() < 2 {
            return Ok(());
        }

        let parent = self
            .snapshot_provider
            .get_header_by_hash(&header.parent_hash)
            .ok_or_else(|| ConsensusError::Other("parent not found".into()))?;
        let snap = self
            .snapshot_provider
            .snapshot(parent.number - 1)
            .ok_or_else(|| ConsensusError::Other("Snapshot not found".into()))?;

        //TODO
        // votes := p.VotePool.FetchVoteByBlockHash(parent.Hash())
        // if len(votes) < cmath.CeilDiv(len(snap.Validators)*2, 3) {
        //     return nil
        // }
        let votes: Vec<VoteEnvelope> = Vec::new();

        let (justified_block_number, justified_block_hash) =
            self.get_justified_number_and_hash(&parent).map_err(|e| e)?;

        let mut attestation = VoteAttestation::new_with_vote_data(VoteData {
            source_hash: justified_block_hash,
            source_number: justified_block_number,
            target_hash: parent.mix_hash,
            target_number: parent.number,
        });

        for vote in votes.iter() {
            if vote.data.hash() != attestation.data.hash() {
                return Err(ConsensusError::Other(
                    format!(
                        "vote check error, expected: {:?}, real: {:?}",
                        attestation.data, vote.data,
                    )
                    .into(),
                ));
            }
        }

        let mut vote_addr_set: HashSet<VoteAddress> = HashSet::new();
        let mut signatures: Vec<VoteSignature> = Vec::new();

        for vote in votes.iter() {
            vote_addr_set.insert(vote.vote_address);
            signatures.push(vote.signature);
        }

        let sigs: Vec<blsSignature> = signatures
            .iter()
            .map(|raw| {
                blsSignature::from_bytes(raw.as_slice()).map_err(|e| {
                    ConsensusError::Other(format!("BLS sig decode error: {:?}", e).into())
                })
            })
            .collect::<Result<_, _>>()?;
        let sigs_ref: Vec<&blsSignature> = sigs.iter().collect();
        attestation.agg_signature.copy_from_slice(
            &AggregateSignature::aggregate(&sigs_ref, false)
                .expect("aggregate failed")
                .to_signature()
                .to_bytes(),
        );

        for (_, val_info) in snap.validators_map.iter() {
            if vote_addr_set.contains(&val_info.vote_addr) {
                attestation.vote_address_set |= 1 << (val_info.index - 1)
            }
        }

        if attestation.vote_address_set.count_ones() as usize != signatures.len() {
            tracing::warn!(
                "assembleVoteAttestation, check VoteAddress Set failed, expected: {:?}, real: {:?}",
                signatures.len(),
                attestation.vote_address_set.count_ones()
            );
            return Err(ConsensusError::Other(
                "invalid attestation, check VoteAddress Set failed".into(),
            ));
        }

        let buf = alloy_rlp::encode(&attestation);
        let extra_seal_start = header.extra_data.len() - EXTRA_SEAL_LEN;
        let extra_seal_bytes = &header.extra_data[extra_seal_start..];

        let mut new_extra = Vec::with_capacity(extra_seal_start + buf.len() + EXTRA_SEAL_LEN);
        new_extra.extend_from_slice(&header.extra_data[..extra_seal_start]);
        new_extra.extend_from_slice(buf.as_ref());
        new_extra.extend_from_slice(extra_seal_bytes);

        header.extra_data = Bytes::from(new_extra);

        Ok(())
    }

    fn get_justified_number_and_hash(
        &self,
        header: &Header,
    ) -> Result<(u64, B256), ConsensusError> {
        let snap = self
            .snapshot_provider
            .snapshot(header.number - 1)
            .ok_or_else(|| ConsensusError::Other("Snapshot not found".into()))?;
        Ok((snap.vote_data.target_number, snap.vote_data.target_hash))
    }
}

pub fn default_sign_fn(_: Address, _: &str, _: &[u8]) -> Result<[u8; 65], ConsensusError> {
    Err(ConsensusError::Other("sign_fn not set".into()))
}
