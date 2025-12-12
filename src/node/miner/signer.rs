use once_cell::sync::OnceCell;
use std::sync::Arc;
use reth_primitives::{Transaction, TransactionSigned};
// reth signing helper avoided to not materialize a B256 from secret
use alloy_primitives::B256;
use alloy_consensus::{SignableTransaction, Header};
use crate::consensus::parlia::{hash_with_chain_id, EXTRA_SEAL_LEN};
use secp256k1::{SECP256K1, Message, SecretKey};
use zeroize::Zeroizing;
use k256::ecdsa::SigningKey as K256SigningKey;

pub struct MinerSigner {
    // Wrap raw key bytes to ensure zeroize-on-drop; reconstruct SecretKey as needed
    secret_bytes: Zeroizing<[u8; 32]>,
}

static GLOBAL_SIGNER: OnceCell<Arc<MinerSigner>> = OnceCell::new();

#[derive(Debug)]
pub enum SignerError {
    NotInitialized,
    AlreadyInitialized,
    SigningFailed(String),
}

impl std::fmt::Display for SignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SignerError::NotInitialized => write!(f, "Global signer not initialized"),
            SignerError::AlreadyInitialized => write!(f, "Global signer already initialized"),
            SignerError::SigningFailed(msg) => write!(f, "Signing failed: {}", msg),
        }
    }
}

impl std::error::Error for SignerError {}

impl MinerSigner {
    pub fn new(secret_key: SecretKey) -> Self {
        Self { secret_bytes: Zeroizing::new(secret_key.secret_bytes()) }
    }

    pub fn sign_transaction(&self, transaction: Transaction) -> Result<TransactionSigned, SignerError> {
        // Sign directly with libsecp256k1 using the in-memory secret to avoid constructing B256 of the key.
        let msg_hash = transaction.signature_hash();
        let message = Message::from_digest(msg_hash.0);
        let sk = SecretKey::from_slice(self.secret_bytes.as_slice())
            .map_err(|e| SignerError::SigningFailed(format!("Invalid private key: {}", e)))?;
        let recoverable_sig = SECP256K1.sign_ecdsa_recoverable(&message, &sk);
        let (recovery_id, sig_bytes) = recoverable_sig.serialize_compact();

        // Map into alloy Signature expected by Transaction::into_signed
        let r = B256::from_slice(&sig_bytes[0..32]).into();
        let s = B256::from_slice(&sig_bytes[32..64]).into();
        let odd_y_parity = i32::from(recovery_id) != 0;
        let signature = alloy_primitives::Signature::new(r, s, odd_y_parity);

        let signed = transaction.into_signed(signature).into();
        Ok(signed)
    }

    pub fn seal_header(&self, header: &Header, chain_id: u64) -> Result<[u8; EXTRA_SEAL_LEN], SignerError> {
        let hash_data = hash_with_chain_id(header, chain_id);
        let message = Message::from_digest(hash_data.0);
        let sk = SecretKey::from_slice(self.secret_bytes.as_slice())
            .map_err(|e| SignerError::SigningFailed(format!("Invalid private key: {}", e)))?;
        let recoverable_sig = SECP256K1.sign_ecdsa_recoverable(&message, &sk);
        let (recovery_id, signature_bytes) = recoverable_sig.serialize_compact();
        
        // [r(32) + s(32) + recovery_id(1)]
        let mut sig_bytes = [0u8; EXTRA_SEAL_LEN];
        sig_bytes[0..64].copy_from_slice(&signature_bytes);
        let raw_recovery_id = i32::from(recovery_id) as u8;
        sig_bytes[64] = raw_recovery_id;
        
        Ok(sig_bytes)
    }
}

/// Backwards-compatible initializer: create signer from raw 32-byte key material.
pub fn init_global_signer(private_key: B256) -> Result<(), SignerError> {
    let sk = SecretKey::from_slice(private_key.as_ref())
        .map_err(|e| SignerError::SigningFailed(format!("Invalid private key: {}", e)))?;
    let signer = Arc::new(MinerSigner::new(sk));
    GLOBAL_SIGNER
        .set(signer)
        .map_err(|_| SignerError::AlreadyInitialized)
}

/// Preferred initializer: use a k256 SigningKey to avoid exposing raw bytes.
pub fn init_global_signer_from_k256(signing_key: &K256SigningKey) -> Result<(), SignerError> {
    // Extract raw bytes (ephemeral) and build a libsecp256k1 SecretKey, then immediately wrap
    let raw = signing_key.to_bytes();
    let sk = SecretKey::from_slice(&raw)
        .map_err(|e| SignerError::SigningFailed(format!("Invalid private key: {}", e)))?;
    let signer = Arc::new(MinerSigner::new(sk));
    GLOBAL_SIGNER
        .set(signer)
        .map_err(|_| SignerError::AlreadyInitialized)
}

pub fn get_global_signer() -> Option<&'static Arc<MinerSigner>> {
    GLOBAL_SIGNER.get()
}

pub fn sign_system_transaction(tx: Transaction) -> Result<TransactionSigned, SignerError> {
    let signer = GLOBAL_SIGNER.get()
        .ok_or(SignerError::NotInitialized)?;
    
    signer.sign_transaction(tx)
}

pub fn is_signer_initialized() -> bool {
    GLOBAL_SIGNER.get().is_some()
}

pub fn seal_header_with_global_signer(header: &Header, chain_id: u64) -> Result<[u8; EXTRA_SEAL_LEN], SignerError> {
    let signer = GLOBAL_SIGNER.get()
        .ok_or(SignerError::NotInitialized)?;
    signer.seal_header(header, chain_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::parlia::{hash_with_chain_id, EXTRA_SEAL_LEN};
    use crate::node::miner::config::keystore::get_validator_address;
    use alloy_consensus::Header;
    use alloy_primitives::{keccak256, Address, Bytes, TxKind, U256};
    use alloy_consensus::TxLegacy;
    use reth_primitives::Transaction;
    use reth_primitives_traits::SignerRecoverable;
    use secp256k1::{ecdsa::RecoverableSignature, ecdsa::RecoveryId};

    fn dev_sk_bytes() -> [u8; 32] {
        // Same as MiningConfig::generate_for_development()
        let v = alloy_primitives::hex::decode(
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        )
        .unwrap();
        let mut out = [0u8; 32];
        out.copy_from_slice(&v);
        out
    }

    #[test]
    fn seal_header_recovers_expected_address() {
        let raw = dev_sk_bytes();
        let secp_sk = SecretKey::from_slice(&raw).unwrap();
        let k256_sk = k256::ecdsa::SigningKey::from_slice(&raw).unwrap();

        // Expected address from k256 verifying key
        let expected: Address = get_validator_address(&k256_sk);

        // Create signer without touching the global once cell
        let signer = MinerSigner::new(secp_sk);

        // Minimal header with required extra_data length
        let header = Header {
            number: 1,
            gas_limit: 30_000_000,
            timestamp: 1_700_000_000,
            extra_data: Bytes::from(vec![0u8; EXTRA_SEAL_LEN]),
            ..Default::default()
        };
        let chain_id = 56u64;

        let sig = signer.seal_header(&header, chain_id).expect("seal header");
        assert_eq!(sig.len(), EXTRA_SEAL_LEN);

        // Recover public key and compare to expected address
        let msg = Message::from_digest(hash_with_chain_id(&header, chain_id).0);
        let rec_id = RecoveryId::try_from(i32::from(sig[64])).unwrap();
        let rec_sig = RecoverableSignature::from_compact(&sig[0..64], rec_id).unwrap();
        let pubkey = SECP256K1.recover_ecdsa(&msg, &rec_sig).unwrap();
        let uncompressed = pubkey.serialize_uncompressed();
        let addr = Address::from_slice(&keccak256(&uncompressed[1..])[12..]);

        assert_eq!(addr, expected);
    }

    #[test]
    fn sign_transaction_and_recover_signer() {
        let raw = dev_sk_bytes();
        let secp_sk = SecretKey::from_slice(&raw).unwrap();
        let k256_sk = k256::ecdsa::SigningKey::from_slice(&raw).unwrap();
        let expected: Address = get_validator_address(&k256_sk);

        let signer = MinerSigner::new(secp_sk);

        // Build a simple legacy transaction
        let chain_id = 56u64;
        let tx = Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_id),
            nonce: 0,
            gas_limit: 21_000,
            gas_price: 1,
            value: U256::from(1u64),
            input: Bytes::new(),
            to: TxKind::Call(Address::ZERO),
        });

        let signed = signer.sign_transaction(tx).expect("sign tx");
        let recovered = signed.recover_signer().expect("recover signer");
        assert_eq!(recovered, expected);
    }
}
