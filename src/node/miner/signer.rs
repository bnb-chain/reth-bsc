use once_cell::sync::OnceCell;
use std::sync::Arc;
use reth_primitives::{Transaction, TransactionSigned};
use reth_primitives_traits::crypto::secp256k1::sign_message;
use alloy_primitives::B256;
use alloy_consensus::SignableTransaction;

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

