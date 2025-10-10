use once_cell::sync::OnceCell;
use std::sync::Arc;
use reth_primitives::{Transaction, TransactionSigned};
use reth_primitives_traits::crypto::secp256k1::sign_message;
use alloy_primitives::B256;
use alloy_consensus::{SignableTransaction, Header};
use crate::consensus::parlia::{hash_with_chain_id, EXTRA_SEAL_LEN};
use secp256k1::{SECP256K1, Message, SecretKey};

pub struct MinerSigner {
    private_key: B256,
}

static GLOBAL_SIGNER: OnceCell<Arc<MinerSigner>> = OnceCell::new();

/// tmp signer for sign system transaction and seal block in mining mode.
/// TODO: refine it to more secure signer.
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
    pub fn new(private_key: B256) -> Self {
        Self { private_key }
    }

    pub fn sign_transaction(&self, transaction: Transaction) -> Result<TransactionSigned, SignerError> {
        let signature = sign_message(self.private_key, transaction.signature_hash())
            .map_err(|e| SignerError::SigningFailed(e.to_string()))?;
        let signed = transaction.into_signed(signature).into();
        Ok(signed)
    }

    pub fn seal_header(&self, header: &Header, chain_id: u64) -> Result<[u8; EXTRA_SEAL_LEN], SignerError> {
        let hash_data = hash_with_chain_id(header, chain_id);
        let secret_key = SecretKey::from_slice(self.private_key.as_ref())
            .map_err(|e| SignerError::SigningFailed(format!("Invalid private key: {}", e)))?;
        
        let message = Message::from_digest(hash_data.0);
        let recoverable_sig = SECP256K1.sign_ecdsa_recoverable(&message, &secret_key);
        let (recovery_id, signature_bytes) = recoverable_sig.serialize_compact();
        
        // [r(32) + s(32) + recovery_id(1)]
        let mut sig_bytes = [0u8; EXTRA_SEAL_LEN];
        sig_bytes[0..64].copy_from_slice(&signature_bytes);
        let raw_recovery_id = i32::from(recovery_id) as u8;
        sig_bytes[64] = raw_recovery_id;
        
        Ok(sig_bytes)
    }
}

pub fn init_global_signer(private_key: B256) -> Result<(), SignerError> {
    let signer = Arc::new(MinerSigner::new(private_key));
    GLOBAL_SIGNER.set(signer)
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

