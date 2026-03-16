use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use alloy_primitives::{hex, FixedBytes, B256};
use once_cell::sync::OnceCell;
use serde_json::Value as JsonValue;
use std::fs;
use zeroize::{Zeroize, Zeroizing};

use super::vote::{VoteAddress, VoteData, VoteEnvelope, VoteSignature};
use blst::min_pk::SecretKey;

/// Domain separation tag used across the codebase for BLS (POP scheme).
const BLST_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

#[derive(Debug)]
pub enum BlsSignerError {
    AlreadyInitialized,
    NotInitialized,
    InvalidSecret(String),
    SigningFailed(String),
}

impl std::fmt::Display for BlsSignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyInitialized => write!(f, "Global BLS signer already initialized"),
            Self::NotInitialized => write!(f, "Global BLS signer not initialized"),
            Self::InvalidSecret(e) => write!(f, "Invalid BLS secret: {e}"),
            Self::SigningFailed(e) => write!(f, "BLS signing failed: {e}"),
        }
    }
}

impl std::error::Error for BlsSignerError {}

/// In-memory BLS signer for vote envelopes.
pub struct BlsVoteSigner {
    /// Zeroized-on-drop secret key bytes (32 bytes)
    sk_bytes: Zeroizing<[u8; 32]>,
}

impl BlsVoteSigner {
    pub fn new_from_bytes(bytes: [u8; 32]) -> Result<Self, BlsSignerError> {
        // Validate secret key by attempting to parse
        SecretKey::from_bytes(&bytes)
            .map_err(|e| BlsSignerError::InvalidSecret(format!("{e:?}")))?;
        Ok(Self { sk_bytes: Zeroizing::new(bytes) })
    }

    fn secret_key(&self) -> Result<SecretKey, BlsSignerError> {
        SecretKey::from_bytes(self.sk_bytes.as_slice())
            .map_err(|e| BlsSignerError::InvalidSecret(format!("{e:?}")))
    }

    pub fn public_key(&self) -> Result<VoteAddress, BlsSignerError> {
        let sk = self.secret_key()?;
        let pk = sk.sk_to_pk();
        let bytes = pk.to_bytes(); // 48 bytes
        Ok(FixedBytes::from_slice(&bytes))
    }

    pub fn sign_hash(&self, hash: B256) -> Result<VoteSignature, BlsSignerError> {
        let sk = self.secret_key()?;
        let sig = sk.sign(hash.as_slice(), BLST_DST, &[]);
        // According to blst, sign returns Signature directly; no error expected.
        let bytes = sig.to_bytes(); // 96 bytes
        Ok(FixedBytes::from_slice(&bytes))
    }

    pub fn sign_vote(&self, data: VoteData) -> Result<VoteEnvelope, BlsSignerError> {
        let vote_address = self.public_key()?;
        let signature = self.sign_hash(data.hash())?;
        Ok(VoteEnvelope { vote_address, signature, data })
    }
}

static GLOBAL_BLS_SIGNER: OnceCell<Arc<BlsVoteSigner>> = OnceCell::new();

pub fn init_global_bls_signer_from_bytes(bytes: [u8; 32]) -> Result<(), BlsSignerError> {
    let signer = Arc::new(BlsVoteSigner::new_from_bytes(bytes)?);
    let vote_addr = signer.public_key()?;
    GLOBAL_BLS_SIGNER.set(signer).map_err(|_| BlsSignerError::AlreadyInitialized)?;
    tracing::info!(
        target: "bsc::bls",
        vote_address = %format!("0x{}", hex::encode(vote_addr)),
        "Initialized BLS signer successfully"
    );
    Ok(())
}

pub fn init_global_bls_signer_from_hex(hex_key: &str) -> Result<(), BlsSignerError> {
    let mut raw = hex::decode(hex_key.strip_prefix("0x").unwrap_or(hex_key))
        .map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
    if raw.len() != 32 {
        return Err(BlsSignerError::InvalidSecret("BLS secret must be 32 bytes".into()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&raw);
    raw.zeroize();
    init_global_bls_signer_from_bytes(arr)
}

pub fn init_global_bls_signer_from_keystore(
    path: &Path,
    password: &str,
) -> Result<(), BlsSignerError> {
    // If path is a directory (Prysm wallet), search keys/* for keystore(s)
    let target = if path.is_dir() {
        let mut keyfile: Option<PathBuf> = None;
        let keys_dir = path.join("keys");
        if keys_dir.is_dir() {
            for entry in
                fs::read_dir(&keys_dir).map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?
            {
                let entry = entry.map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
                let p = entry.path();
                if p.extension()
                    .and_then(|s| s.to_str())
                    .map(|s| s.eq_ignore_ascii_case("json"))
                    .unwrap_or(false)
                {
                    keyfile = Some(p);
                    break;
                }
            }
        }
        keyfile.ok_or_else(|| {
            BlsSignerError::InvalidSecret("No keystore JSON found under wallet/keys".into())
        })?
    } else {
        path.to_path_buf()
    };

    // Password may be provided as:
    // - literal (default)
    // - file:<path> (explicit password file)
    // For backward-compatibility, if the string happens to be a path to a file, we still read it
    // but emit a warning to prefer the explicit file: prefix to avoid ambiguity.
    let actual_password = if !password.is_empty() {
        if let Some(rest) = password.strip_prefix("file:") {
            let p = Path::new(rest);
            tracing::debug!("Reading BLS keystore password from file via file: prefix",);
            match fs::read_to_string(p) {
                Ok(s) => s.trim_end_matches(['\n', '\r']).to_string(),
                Err(e) => {
                    return Err(BlsSignerError::InvalidSecret(format!(
                        "failed to read password file: {e}"
                    )))
                }
            }
        } else {
            let p = Path::new(password);
            if p.is_file() {
                tracing::warn!(
                    "Interpreting BLS keystore password as file path; prefer 'file:<path>' prefix"
                );
                match fs::read_to_string(p) {
                    Ok(s) => s.trim_end_matches(['\n', '\r']).to_string(),
                    Err(e) => {
                        return Err(BlsSignerError::InvalidSecret(format!(
                            "failed to read password file: {e}"
                        )))
                    }
                }
            } else {
                password.to_string()
            }
        }
    } else {
        String::new()
    };

    // Detect EIP-2335 via JSON version==4; if so, use EIP-2335 path exclusively
    if let Ok(contents) = fs::read_to_string(&target) {
        if let Ok(json) = serde_json::from_str::<JsonValue>(&contents) {
            if json.get("version").and_then(|x| x.as_u64()) == Some(4) {
                let secret = decrypt_eip2335_keystore(&target, &actual_password)?;
                return init_global_bls_signer_from_bytes(secret);
            }
        }
    }

    // Otherwise, try Ethereum V3 keystore
    let mut key_bytes = eth_keystore::decrypt_key(&target, &actual_password)
        .map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
    if key_bytes.len() != 32 {
        return Err(BlsSignerError::InvalidSecret("BLS secret must be 32 bytes".into()));
    }
    let mut arr = [0u8; 32];
    arr.copy_from_slice(&key_bytes);
    key_bytes.zeroize();
    init_global_bls_signer_from_bytes(arr)
}

fn decrypt_eip2335_keystore(path: &Path, password: &str) -> Result<[u8; 32], BlsSignerError> {
    let contents =
        fs::read_to_string(path).map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
    let v: JsonValue = serde_json::from_str(&contents)
        .map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;

    let version = v
        .get("version")
        .and_then(|x| x.as_u64())
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing version".into()))?;
    if version != 4 {
        return Err(BlsSignerError::InvalidSecret("not EIP-2335 v4".into()));
    }

    let crypto =
        v.get("crypto").ok_or_else(|| BlsSignerError::InvalidSecret("missing crypto".into()))?;
    let kdf =
        crypto.get("kdf").ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf".into()))?;
    let kdf_fn = kdf
        .get("function")
        .and_then(|x| x.as_str())
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.function".into()))?;
    let kdf_params = kdf
        .get("params")
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params".into()))?;
    let dklen = kdf_params.get("dklen").and_then(|x| x.as_u64()).unwrap_or(32) as usize;
    let salt_hex = kdf_params
        .get("salt")
        .and_then(|x| x.as_str())
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params.salt".into()))?;
    let mut salt = decode_hex_noprefix(salt_hex)?;

    // Derive key
    let mut dk = vec![0u8; dklen];
    match kdf_fn.to_ascii_lowercase().as_str() {
        "scrypt" => {
            let n = kdf_params
                .get("n")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params.n".into()))?;
            let r = kdf_params
                .get("r")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params.r".into()))?
                as u32;
            let p = kdf_params
                .get("p")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params.p".into()))?
                as u32;
            // scrypt crate uses log2(N)
            if n == 0 || (n & (n - 1)) != 0 {
                return Err(BlsSignerError::InvalidSecret("scrypt N must be power of two".into()));
            }
            // log2(N) for power-of-two N. Using trailing_zeros avoids magic constants like 64 (u64::BITS).
            let log_n: u8 = n.trailing_zeros() as u8;
            let params = scrypt::Params::new(log_n, r, p, dklen)
                .map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
            scrypt::scrypt(password.as_bytes(), &salt, &params, &mut dk)
                .map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))?;
        }
        "pbkdf2" => {
            let c = kdf_params
                .get("c")
                .and_then(|x| x.as_u64())
                .ok_or_else(|| BlsSignerError::InvalidSecret("missing kdf.params.c".into()))?
                as u32;
            use pbkdf2::pbkdf2_hmac;
            use sha2::Sha256;
            pbkdf2_hmac::<Sha256>(password.as_bytes(), &salt, c, &mut dk);
        }
        other => return Err(BlsSignerError::InvalidSecret(format!("unsupported kdf: {other}"))),
    }
    // Best-effort wipe salt after use
    salt.zeroize();

    // Parse cipher params and ciphertext
    let cipher = crypto
        .get("cipher")
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing cipher".into()))?;
    let cipher_fn = cipher.get("function").and_then(|x| x.as_str()).unwrap_or("");
    let cipher_params = cipher
        .get("params")
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing cipher.params".into()))?;
    let iv_hex = cipher_params
        .get("iv")
        .and_then(|x| x.as_str())
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing cipher.params.iv".into()))?;
    let mut iv = decode_hex_noprefix(iv_hex)?;
    if iv.len() != 16 {
        return Err(BlsSignerError::InvalidSecret("iv must be 16 bytes".into()));
    }
    let ct_hex = cipher
        .get("message")
        .and_then(|x| x.as_str())
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing cipher.message".into()))?;
    let mut ct = decode_hex_noprefix(ct_hex)?;

    // Verify checksum first: sha256(derived_key[16..32] || ciphertext)
    let checksum = crypto
        .get("checksum")
        .ok_or_else(|| BlsSignerError::InvalidSecret("missing checksum".into()))?;
    let checksum_msg = checksum.get("message").and_then(|x| x.as_str()).unwrap_or("");
    use sha2::{Digest, Sha256};
    if dklen < 32 {
        return Err(BlsSignerError::InvalidSecret("dklen too small".into()));
    }
    let mut hasher = Sha256::new();
    hasher.update(&dk[16..32]);
    hasher.update(&ct);
    let calc = hasher.finalize();
    let calc_hex = hex::encode(calc);
    if !eq_hex_noprefix(&calc_hex, checksum_msg) {
        return Err(BlsSignerError::InvalidSecret("checksum mismatch".into()));
    }

    // Decrypt AES-CTR after checksum validation
    use aes::{Aes128, Aes256};
    use cipher::{KeyIvInit, StreamCipher};
    if cipher_fn.eq_ignore_ascii_case("aes-128-ctr") {
        type Aes128Ctr = ctr::Ctr128BE<Aes128>;
        let aes_key = &dk[0..16];
        let mut cipher = Aes128Ctr::new(aes_key.into(), iv.as_slice().into());
        cipher.apply_keystream(&mut ct);
    } else if cipher_fn.eq_ignore_ascii_case("aes-256-ctr") {
        type Aes256Ctr = ctr::Ctr128BE<Aes256>;
        let aes_key = &dk[0..32];
        let mut cipher = Aes256Ctr::new(aes_key.into(), iv.as_slice().into());
        cipher.apply_keystream(&mut ct);
    } else {
        return Err(BlsSignerError::InvalidSecret("unsupported cipher".into()));
    }

    if ct.len() != 32 {
        return Err(BlsSignerError::InvalidSecret("decrypted secret wrong length".into()));
    }

    let mut out = [0u8; 32];
    out.copy_from_slice(&ct);
    dk.zeroize();
    iv.zeroize();
    ct.zeroize();
    Ok(out)
}

fn eq_hex_noprefix(a: &str, b: &str) -> bool {
    let aa = a.strip_prefix("0x").unwrap_or(a);
    let bb = b.strip_prefix("0x").unwrap_or(b);
    aa.eq_ignore_ascii_case(bb)
}

fn decode_hex_noprefix(s: &str) -> Result<Vec<u8>, BlsSignerError> {
    let ss = s.strip_prefix("0x").unwrap_or(s);
    hex::decode(ss).map_err(|e| BlsSignerError::InvalidSecret(e.to_string()))
}

pub fn get_global_bls_signer() -> Option<&'static Arc<BlsVoteSigner>> {
    GLOBAL_BLS_SIGNER.get()
}

pub fn is_bls_signer_initialized() -> bool {
    GLOBAL_BLS_SIGNER.get().is_some()
}

pub fn global_bls_public_key() -> Result<VoteAddress, BlsSignerError> {
    get_global_bls_signer().ok_or(BlsSignerError::NotInitialized)?.public_key()
}

pub fn sign_vote_with_global(data: VoteData) -> Result<VoteEnvelope, BlsSignerError> {
    get_global_bls_signer().ok_or(BlsSignerError::NotInitialized)?.sign_vote(data)
}

/// Initialize the global BLS signer from env vars, if present:
/// - `BSC_BLS_PRIVATE_KEY` (0x-hex 32 bytes)
/// - `BSC_BLS_KEYSTORE_PATH` and `BSC_BLS_KEYSTORE_PASSWORD`
pub fn init_from_env_if_present() {
    let keystore_path = std::env::var("BSC_BLS_KEYSTORE_PATH").ok();
    let keystore_password = std::env::var("BSC_BLS_KEYSTORE_PASSWORD").ok();
    let bls_hex = std::env::var("BSC_BLS_PRIVATE_KEY").ok();

    if is_bls_signer_initialized() {
        return;
    }

    if let (Some(path), Some(pass)) = (keystore_path, keystore_password) {
        match init_global_bls_signer_from_keystore(Path::new(&path), &pass) {
            Ok(()) => tracing::info!("Initialized BLS signer from keystore"),
            Err(e) => tracing::warn!("Failed to init BLS signer from keystore: {}", e),
        }
        return;
    }

    if let Some(hex_key) = bls_hex {
        match init_global_bls_signer_from_hex(&hex_key) {
            Ok(()) => {
                tracing::warn!("Initialized BLS signer from hex (not recommended for production)")
            }
            Err(e) => tracing::warn!("Failed to init BLS signer from hex: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use blst::{
        min_pk::{PublicKey, Signature},
        BLST_ERROR,
    };

    #[test]
    fn bls_sign_and_verify_single_key() {
        // use a small valid BLS scalar: 1 (big-endian)
        let mut raw = [0u8; 32];
        raw[31] = 1;
        let signer = BlsVoteSigner::new_from_bytes(raw).expect("create bls signer");

        // Compose vote data
        let data = VoteData {
            source_number: 1,
            source_hash: B256::from_slice(&[1u8; 32]),
            target_number: 2,
            target_hash: B256::from_slice(&[2u8; 32]),
        };

        let envelope = signer.sign_vote(data).expect("sign vote");
        assert_eq!(envelope.data, data);

        // Verify signature with blst
        let pk = PublicKey::from_bytes(envelope.vote_address.as_slice()).expect("pk");
        let sig = Signature::from_bytes(envelope.signature.as_slice()).expect("sig");
        let pubkeys = vec![&pk];
        let res = sig.fast_aggregate_verify(true, data.hash().as_slice(), BLST_DST, &pubkeys);
        assert_eq!(res, BLST_ERROR::BLST_SUCCESS);
    }
}
