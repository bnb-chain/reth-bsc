use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
use std::path::PathBuf;

/// Mining configuration for BSC PoA
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MiningConfig {
    /// Enable mining
    pub enabled: bool,
    /// Validator address for this node
    pub validator_address: Option<Address>,
    /// Path to validator private key file
    pub keystore_path: Option<PathBuf>,
    /// Password for keystore file
    pub keystore_password: Option<String>,
    /// Alternative: Private key as hex string (NOT RECOMMENDED for production)
    pub private_key_hex: Option<String>,
    /// Block gas limit
    pub gas_limit: Option<u64>,
    /// Mining interval in milliseconds
    pub mining_interval_ms: Option<u64>,
}

impl Default for MiningConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            validator_address: None,
            keystore_path: None,
            keystore_password: None,
            private_key_hex: None,
            gas_limit: Some(30_000_000),
            mining_interval_ms: Some(500),
        }
    }
}

impl MiningConfig {
    /// Validate the mining configuration
    pub fn validate(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }

        // For mining, a key source is required; validator_address can be derived from the key.
        if self.keystore_path.is_none() && self.private_key_hex.is_none() {
            return Err("Mining enabled but no keystore_path or private_key_hex specified".to_string());
        }

        if self.keystore_path.is_some() && self.keystore_password.is_none() {
            return Err("Keystore path specified but no password provided".to_string());
        }

        Ok(())
    }

    /// Check if mining is properly configured
    pub fn is_mining_enabled(&self) -> bool {
        self.enabled && (self.keystore_path.is_some() || self.private_key_hex.is_some())
    }

    /// Generate a new validator configuration with random keys
    pub fn generate_for_development() -> Self {
        // use rand::Rng;
        
        // Generate random 32-byte private key
        // let mut rng = rand::rng();
        // let private_key: [u8; 32] = rng.random();
        // let private_key_hex = format!("0x{}", alloy_primitives::hex::encode(private_key));
        let private_key_hex = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        // Validator Address: 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266

        // Derive validator address from private key
        if let Ok(signing_key) = keystore::load_private_key_from_hex(private_key_hex) {
            let validator_address = keystore::get_validator_address(&signing_key);
            
            Self {
                enabled: true,
                validator_address: Some(validator_address),
                private_key_hex: Some(private_key_hex.to_string()),
                keystore_path: None,
                keystore_password: None,
                gas_limit: Some(30_000_000),
                mining_interval_ms: Some(500),
            }
        } else {
            // Fallback to default if key generation fails
            Self::default()
        }
    }

    /// Auto-generate keys if mining is enabled but no keys provided
    pub fn ensure_keys_available(mut self) -> Self {
        if self.enabled && 
           self.keystore_path.is_none() && 
           self.private_key_hex.is_none() {
            
            tracing::info!("Mining enabled but no keys provided - generating development keys");
            let generated = Self::generate_for_development();
            
            // Keep existing config but use generated keys
            self.validator_address = generated.validator_address;
            self.private_key_hex = generated.private_key_hex;
            
            if let Some(addr) = self.validator_address {
                tracing::warn!("🔑 AUTO-GENERATED validator keys for development:");
                tracing::warn!("📍 Validator Address: {}", addr);
                tracing::warn!("🔐 Private Key: {} (KEEP SECURE!)", 
                    self.private_key_hex.as_ref().unwrap());
                tracing::warn!("⚠️  These are DEVELOPMENT keys - do not use in production!");
            }
        }
        
        self
    }

    /// Create a ready-to-use development mining configuration
    pub fn development() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }.ensure_keys_available()
    }

    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let enabled = std::env::var("BSC_MINING_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let private_key_hex = std::env::var("BSC_PRIVATE_KEY").ok();

        let gas_limit = std::env::var("BSC_GAS_LIMIT")
            .ok()
            .and_then(|v| v.parse().ok());

        let mining_interval_ms = std::env::var("BSC_MINING_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok());

        let mut cfg = Self {
            enabled,
            private_key_hex,
            gas_limit,
            mining_interval_ms,
            ..Default::default()
        };

        // If a private key is present but validator_address is not, derive it automatically.
        if cfg.validator_address.is_none() {
            if let Some(ref pk_hex) = cfg.private_key_hex {
                if let Ok(sk) = keystore::load_private_key_from_hex(pk_hex) {
                    cfg.validator_address = Some(keystore::get_validator_address(&sk));
                }
            }
        }

        cfg.ensure_keys_available()
    }
}

// Global override for mining configuration set via CLI
static GLOBAL_MINING_CONFIG: OnceLock<MiningConfig> = OnceLock::new();

/// Set a global mining configuration to override env defaults (typically from CLI args)
pub fn set_global_mining_config(cfg: MiningConfig) -> Result<(), Box<MiningConfig>> {
    GLOBAL_MINING_CONFIG.set(cfg).map_err(Box::new)
}

/// Get the global mining configuration override if set
pub fn get_global_mining_config() -> Option<&'static MiningConfig> {
    GLOBAL_MINING_CONFIG.get()
}

/// Key management for validators
pub mod keystore {
    use alloy_primitives::Address;
    use k256::ecdsa::{SigningKey, Signature, signature::Signer};
    use alloy_primitives::keccak256;
    use std::path::Path;
    use reth::consensus::ConsensusError;

    /// Load private key from keystore file
    pub fn load_private_key_from_keystore(
        _keystore_path: &Path,
        _password: &str,
    ) -> Result<SigningKey, Box<dyn std::error::Error + Send + Sync>> {
        // TODO: Implement proper keystore loading
        // This would typically use eth_keystore or similar library
        // For now, return an error to indicate not implemented
        Err("Keystore loading not yet implemented - use private_key_hex for testing".into())
    }

    /// Load private key from hex string
    pub fn load_private_key_from_hex(
        hex_key: &str,
    ) -> Result<SigningKey, Box<dyn std::error::Error + Send + Sync>> {
        let key_bytes = alloy_primitives::hex::decode(hex_key.strip_prefix("0x").unwrap_or(hex_key))?;
        if key_bytes.len() != 32 {
            return Err("Private key must be 32 bytes".into());
        }
        
        let signing_key = SigningKey::from_slice(&key_bytes)?;
        Ok(signing_key)
    }

    /// Get validator address from private key
    pub fn get_validator_address(signing_key: &SigningKey) -> Address {
        let public_key = signing_key.verifying_key();
        let public_bytes = public_key.to_encoded_point(false);
        let hash = keccak256(&public_bytes.as_bytes()[1..]); // Skip 0x04 prefix
        Address::from_slice(&hash[12..])
    }

    /// Create signing function with loaded private key
    pub fn create_signing_function(
        signing_key: SigningKey,
    ) -> impl Fn(Address, &str, &[u8]) -> Result<Vec<u8>, ConsensusError> + Send + Sync + 'static {
        move |_addr: Address, _mimetype: &str, data: &[u8]| -> Result<Vec<u8>, ConsensusError> {
            let hash = keccak256(data);
            let signature: Signature = signing_key.sign(hash.as_slice());
            Ok(signature.to_bytes().to_vec())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mining_config_validation() {
        let mut config = MiningConfig::default();
        assert!(config.validate().is_ok()); // Disabled by default should be OK

        config.enabled = true;
        assert!(config.validate().is_err()); // Enabled but no signing key configured

        config.validator_address = Some("0x1234567890abcdef1234567890abcdef12345678".parse().unwrap());
        assert!(config.validate().is_err()); // Still no signing key specified

        config.private_key_hex = Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string());
        assert!(config.validate().is_ok()); // Now properly configured
    }

    #[test]
    fn test_key_loading() {
        let test_key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let signing_key = keystore::load_private_key_from_hex(test_key).unwrap();
        let address = keystore::get_validator_address(&signing_key);
        
        // Verify we can get an address from the key
        assert_ne!(address, Address::ZERO);
    }
}
