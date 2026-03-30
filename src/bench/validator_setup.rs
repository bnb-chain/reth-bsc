use crate::node::miner::signer::MinerSigner;
use alloy_consensus::TxLegacy;
use alloy_primitives::{Address, Bytes, Keccak256, TxKind, U256};
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall;
use blst::min_pk::SecretKey as BlsSecretKey;
use reth_primitives::{Transaction, TransactionSigned};
use secp256k1::SecretKey;

/// StakeHub contract address
const STAKE_HUB: Address = Address::new([
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x20, 0x02,
]);

/// Delegation amount: 20001 BNB (matches create-validator script)
const DELEGATION_AMOUNT: u128 = 20_001u128 * 1_000_000_000_000_000_000u128;

sol! {
    #[allow(missing_docs)]
    struct Commission {
        uint64 rate;
        uint64 maxRate;
        uint64 maxChangeRate;
    }

    #[allow(missing_docs)]
    struct Description {
        string moniker;
        string identity;
        string website;
        string details;
    }

    #[allow(missing_docs)]
    function createValidator(
        address consensusAddress,
        bytes voteAddress,
        bytes blsProof,
        Commission commission,
        Description description
    ) payable;
}

/// Generated BLS key pair for a validator
pub struct ValidatorBlsKey {
    pub secret: BlsSecretKey,
    pub pubkey_bytes: Vec<u8>,
}

/// Generate a deterministic BLS key pair for a validator index.
pub fn generate_bls_key(validator_index: usize) -> ValidatorBlsKey {
    // Deterministic seed for reproducibility
    let mut ikm = [0u8; 32];
    ikm[0] = 0xBE; // prefix
    ikm[31] = validator_index as u8;
    let secret = BlsSecretKey::key_gen(&ikm, &[]).expect("valid BLS key");
    let pk = secret.sk_to_pk();
    let pubkey_bytes = pk.compress().to_vec();
    ValidatorBlsKey { secret, pubkey_bytes }
}

/// Create the BLS proof for createValidator.
/// proof = BLS_sign(keccak256(consensusAddr || blsPubKey || paddedChainId))
fn create_bls_proof(
    bls_secret: &BlsSecretKey,
    consensus_addr: Address,
    bls_pubkey: &[u8],
    chain_id: u64,
) -> Vec<u8> {
    let mut chain_id_padded = [0u8; 32];
    chain_id_padded[24..32].copy_from_slice(&chain_id.to_be_bytes());

    let mut hasher = Keccak256::new();
    hasher.update(consensus_addr.as_slice());
    hasher.update(bls_pubkey);
    hasher.update(chain_id_padded);
    let msg_hash = hasher.finalize();

    let dst = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";
    let sig = bls_secret.sign(msg_hash.as_slice(), dst, &[]);
    sig.compress().to_vec()
}

/// Build the createValidator transaction for a single validator.
/// The validator signs it themselves (msg.sender == consensusAddress).
pub fn create_validator_tx(
    validator_key: &alloy_primitives::B256,
    validator_addr: Address,
    bls_key: &ValidatorBlsKey,
    chain_id: u64,
    nonce: u64,
) -> TransactionSigned {
    let proof = create_bls_proof(&bls_key.secret, validator_addr, &bls_key.pubkey_bytes, chain_id);

    let calldata = createValidatorCall {
        consensusAddress: validator_addr,
        voteAddress: Bytes::from(bls_key.pubkey_bytes.clone()),
        blsProof: Bytes::from(proof),
        commission: Commission { rate: 100, maxRate: 1000, maxChangeRate: 100 },
        description: Description {
            moniker: format!("BenchVal{}", nonce),
            identity: String::new(),
            website: String::new(),
            details: String::new(),
        },
    }
    .abi_encode();

    let tx = Transaction::Legacy(TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_limit: 2_000_000,
        gas_price: 1,
        value: U256::from(DELEGATION_AMOUNT),
        input: Bytes::from(calldata),
        to: TxKind::Call(STAKE_HUB),
    });

    let sk = SecretKey::from_slice(validator_key.as_ref()).expect("valid key");
    let signer = MinerSigner::new(sk);
    signer.sign_transaction(tx).expect("signing failed")
}

/// Generate BLS keys and createValidator transactions for all 3 validators.
pub fn create_all_validator_txs(
    validator_keys: &[alloy_primitives::B256],
    validator_addrs: &[Address],
    chain_id: u64,
) -> (Vec<TransactionSigned>, Vec<ValidatorBlsKey>) {
    let mut txs = Vec::new();
    let mut bls_keys = Vec::new();

    for (i, (key, addr)) in validator_keys.iter().zip(validator_addrs.iter()).enumerate() {
        let bls_key = generate_bls_key(i);
        let tx = create_validator_tx(key, *addr, &bls_key, chain_id, 0);
        println!(
            "  Validator {}: addr={}, bls_pubkey={}...",
            i,
            addr,
            hex::encode(&bls_key.pubkey_bytes[..8])
        );
        txs.push(tx);
        bls_keys.push(bls_key);
    }

    (txs, bls_keys)
}
