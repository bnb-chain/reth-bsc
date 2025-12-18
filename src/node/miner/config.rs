use alloy_primitives::Address;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::consensus::parlia::DEFAULT_MIN_GAS_TIP;

/// Mining configuration for BSC PoSA
#[derive(Clone, Serialize, Deserialize)]
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
    /// Minimum gas tip
    pub min_gas_tip: Option<u128>,
    /// Submit built payload to the import service
    pub submit_built_payload: bool,
    /// Enable greedy merge
    pub greedy_merge: bool,
    // MEV related parameters
    /// Validator commission rate (in basis points, 100 = 1%)
    pub validator_commission: Option<u64>,
    /// Bid simulation left over time in milliseconds
    pub bid_simulation_left_over: Option<u64>,
    /// No interrupt left over time in milliseconds
    pub no_interrupt_left_over: Option<u64>,
    /// Maximum bids per builder per block
    pub max_bids_per_builder: Option<u32>,
    /// Builder fee ceiling in wei (as hex string for large numbers)
    pub builder_fee_ceil: Option<u128>,
    /// List of allowed builder addresses (whitelist)
    pub allowed_builders: Option<Vec<Address>>,
}

impl std::fmt::Debug for MiningConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MiningConfig")
            .field("enabled", &self.enabled)
            .field("validator_address", &self.validator_address)
            .field("keystore_path", &self.keystore_path)
            .field("keystore_password", &self.keystore_password.as_ref().map(|_| "<redacted>"))
            .field("private_key_hex", &self.private_key_hex.as_ref().map(|_| "<redacted>"))
            .field("gas_limit", &self.gas_limit)
            .field("min_gas_tip", &self.min_gas_tip)
            .field("submit_built_payload", &self.submit_built_payload)
            .field("validator_commission", &self.validator_commission)
            .field("bid_simulation_left_over", &self.bid_simulation_left_over)
            .field("no_interrupt_left_over", &self.no_interrupt_left_over)
            .field("max_bids_per_builder", &self.max_bids_per_builder)
            .field("builder_fee_ceil", &self.builder_fee_ceil)
            .field("allowed_builders", &self.allowed_builders)
            .finish()
    }
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
            min_gas_tip: Some(DEFAULT_MIN_GAS_TIP),
            submit_built_payload: false,
            greedy_merge: true,
            // MEV defaults
            validator_commission: Some(100),    // 1%
            bid_simulation_left_over: Some(50), // 50ms
            no_interrupt_left_over: Some(500),  // 500ms
            max_bids_per_builder: Some(3),
            builder_fee_ceil: Some(1_000_000_000_000_000_000), // 1 BNB
            allowed_builders: None, // No whitelist by default (allow all)
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
            return Err(
                "Mining enabled but no keystore_path or private_key_hex specified".to_string()
            );
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

    /// Get the desired gas limit for the specified chain ID.
    /// Returns the configured gas_limit if set, otherwise returns chain-specific defaults:
    /// - BSC Mainnet (56): 140M
    /// - BSC Testnet (97): 100M  
    /// - Local/Other: 40M
    pub fn get_gas_limit(&self, chain_id: u64) -> u64 {
        self.gas_limit.unwrap_or({
            match chain_id {
                56 => 140_000_000, // BSC mainnet
                97 => 100_000_000, // BSC testnet
                _ => 40_000_000,   // Local development
            }
        })
    }

    pub fn get_min_gas_tip(&self) -> u128 {
        self.min_gas_tip.unwrap_or(DEFAULT_MIN_GAS_TIP)
    }

    // MEV parameter getters with defaults

    /// Get validator commission rate (in basis points, 100 = 1%)
    pub fn get_validator_commission(&self) -> u64 {
        self.validator_commission.unwrap_or(100) // Default: 1%
    }

    /// Get bid simulation left over time in milliseconds
    pub fn get_bid_simulation_left_over(&self) -> u64 {
        self.bid_simulation_left_over.unwrap_or(50) // Default: 50ms
    }

    /// Get no interrupt left over time in milliseconds
    pub fn get_no_interrupt_left_over(&self) -> u64 {
        self.no_interrupt_left_over.unwrap_or(500) // Default: 500ms
    }

    /// Get maximum bids per builder per block
    pub fn get_max_bids_per_builder(&self) -> u32 {
        self.max_bids_per_builder.unwrap_or(3) // Default: 3
    }

    /// Get builder fee ceiling in wei
    pub fn get_builder_fee_ceil(&self) -> u128 {
        self.builder_fee_ceil.unwrap_or(1_000_000_000_000_000_000) // Default: 1 BNB
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
                min_gas_tip: Some(DEFAULT_MIN_GAS_TIP),
                submit_built_payload: false,
                // Use default MEV parameters
                validator_commission: Some(100),
                bid_simulation_left_over: Some(50),
                no_interrupt_left_over: Some(500),
                max_bids_per_builder: Some(3),
                builder_fee_ceil: Some(1_000_000_000_000_000_000),
                allowed_builders: None,
                greedy_merge: true,
            }
        } else {
            // Fallback to default if key generation fails
            Self::default()
        }
    }

    /// Auto-generate keys if mining is enabled but no keys provided
    pub fn ensure_keys_available(mut self) -> Self {
        if self.enabled && self.keystore_path.is_none() && self.private_key_hex.is_none() {
            tracing::info!("Mining enabled but no keys provided - generating development keys");
            let generated = Self::generate_for_development();

            // Keep existing config but use generated keys
            self.validator_address = generated.validator_address;
            self.private_key_hex = generated.private_key_hex;

            if let Some(addr) = self.validator_address {
                tracing::warn!("AUTO-GENERATED validator keys for development:");
                tracing::warn!("Validator Address: {}", addr);
                tracing::warn!(
                    "Private Key: {} (KEEP SECURE!)",
                    self.private_key_hex.as_ref().unwrap()
                );
                tracing::warn!("These are DEVELOPMENT keys - do not use in production!");
            }
        }

        self
    }

    /// Create a ready-to-use development mining configuration
    pub fn development() -> Self {
        Self { enabled: true, ..Default::default() }.ensure_keys_available()
    }

    /// Load configuration from environment variables
    pub fn from_env() -> Self {
        let enabled = std::env::var("BSC_MINING_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let private_key_hex = std::env::var("BSC_PRIVATE_KEY").ok();

        let keystore_path = std::env::var("BSC_KEYSTORE_PATH").ok().map(Into::into);
        let keystore_password = std::env::var("BSC_KEYSTORE_PASSWORD").ok();

        let gas_limit = std::env::var("BSC_GAS_LIMIT").ok().and_then(|v| v.parse().ok());

        let min_gas_tip = std::env::var("BSC_MIN_GAS_TIP").ok().and_then(|v| v.parse().ok());

        let submit_built_payload = std::env::var("BSC_SUBMIT_BUILT_PAYLOAD")
            .ok()
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);

        let greedy_merge = std::env::var("BSC_GREEDY_MERGE")
            .ok()
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(true);

        // MEV parameters from environment
        let validator_commission =
            std::env::var("BSC_VALIDATOR_COMMISSION").ok().and_then(|v| v.parse().ok());

        let bid_simulation_left_over =
            std::env::var("BSC_BID_SIMULATION_LEFT_OVER").ok().and_then(|v| v.parse().ok());

        let no_interrupt_left_over =
            std::env::var("BSC_NO_INTERRUPT_LEFT_OVER").ok().and_then(|v| v.parse().ok());

        let max_bids_per_builder =
            std::env::var("BSC_MAX_BIDS_PER_BUILDER").ok().and_then(|v| v.parse().ok());

        let builder_fee_ceil =
            std::env::var("BSC_BUILDER_FEE_CEIL").ok().and_then(|v| v.parse().ok());

        // Parse allowed builders from comma-separated addresses
        let allowed_builders = std::env::var("BSC_ALLOWED_BUILDERS")
            .ok()
            .map(|s| {
                s.split(',')
                    .filter_map(|addr| addr.trim().parse::<Address>().ok())
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty());

        let mut cfg = Self {
            enabled,
            private_key_hex,
            keystore_path,
            keystore_password,
            gas_limit,
            min_gas_tip,
            submit_built_payload,
            greedy_merge,
            validator_commission,
            bid_simulation_left_over,
            no_interrupt_left_over,
            max_bids_per_builder,
            builder_fee_ceil,
            allowed_builders,
            ..Default::default()
        };

        // If a private key is present but validator_address is not, derive it automatically.
        if cfg.validator_address.is_none() {
            if let Some(ref pk_hex) = cfg.private_key_hex {
                if let Ok(sk) = keystore::load_private_key_from_hex(pk_hex) {
                    cfg.validator_address = Some(keystore::get_validator_address(&sk));
                }
            } else if let (Some(ref path), Some(ref pass)) =
                (&cfg.keystore_path, &cfg.keystore_password)
            {
                if let Ok(sk) = keystore::load_private_key_from_keystore(path, pass) {
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
    use alloy_primitives::keccak256;
    use alloy_primitives::Address;
    use k256::ecdsa::{signature::Signer, Signature, SigningKey};
    use std::path::Path;

    /// Load private key from keystore file
    pub fn load_private_key_from_keystore(
        keystore_path: &Path,
        password: &str,
    ) -> Result<SigningKey, Box<dyn std::error::Error + Send + Sync>> {
        let mut key_bytes = eth_keystore::decrypt_key(keystore_path, password)?; // Vec<u8>
        if key_bytes.len() != 32 {
            return Err("Decrypted private key must be 32 bytes".into());
        }
        let signing_key = SigningKey::from_slice(&key_bytes)?;
        // Immediately zeroize the decrypted key material from heap memory
        use zeroize::Zeroize;
        key_bytes.zeroize();
        Ok(signing_key)
    }

    /// Load private key from hex string
    pub fn load_private_key_from_hex(
        hex_key: &str,
    ) -> Result<SigningKey, Box<dyn std::error::Error + Send + Sync>> {
        let key_bytes =
            alloy_primitives::hex::decode(hex_key.strip_prefix("0x").unwrap_or(hex_key))?;
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
    ) -> impl Fn(Address, &str, &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>
           + Send
           + Sync
           + 'static {
        move |_addr: Address,
              _mimetype: &str,
              data: &[u8]|
              -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
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

        config.validator_address =
            Some("0x1234567890abcdef1234567890abcdef12345678".parse().unwrap());
        assert!(config.validate().is_err()); // Still no signing key specified

        config.private_key_hex =
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string());
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

    #[test]
    fn test_load_private_key_from_keystore_file() {
        use std::fs;
        use std::io::Write;
        use std::path::PathBuf;
        use uuid::Uuid;

        // This is a real V3 keystore JSON (address bcdd0d2c...) with password "0123456789"
        let keystore_json = r#"{"address":"bcdd0d2cda5f6423e57b6a4dcd75decbe31aecf0","crypto":{"cipher":"aes-128-ctr","ciphertext":"f7505ced32fe3037d6dc25ae7e9716858e516bacfb0dc28c9995f01cf7fee84a","cipherparams":{"iv":"d93e5314f18ccfc8330e1ad37534fa29"},"kdf":"scrypt","kdfparams":{"dklen":32,"n":262144,"p":1,"r":8,"salt":"1fb8f954f70fb0ddb8be5b1fb57d821df21dda700682f382baed311dde95a6c7"},"mac":"c501bb033f94cb9efc3c63fe1547555fc1412d3abb8b400cacda37bd9de888ec"},"id":"7246dc9c-d170-4009-8878-8628be169836","version":3}"#;

        // Write to a temporary file
        let mut path: PathBuf = std::env::temp_dir();
        let fname =
            format!("UTC--test-{}--bcdd0d2cda5f6423e57b6a4dcd75decbe31aecf0", Uuid::new_v4());
        path.push(fname);
        {
            let mut f = fs::File::create(&path).unwrap();
            f.write_all(keystore_json.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }

        // Decrypt with the known password
        let signing_key = keystore::load_private_key_from_keystore(&path, "0123456789").unwrap();
        // convert signing_key to hex string
        let signing_key_hex = alloy_primitives::hex::encode(signing_key.to_bytes());
        println!("signing_key: 0x{}", signing_key_hex);

        let address = keystore::get_validator_address(&signing_key);

        // Expect derived address to match the keystore address
        let expected: Address = "0xbcdd0d2cda5f6423e57b6a4dcd75decbe31aecf0".parse().unwrap();
        assert_eq!(address, expected);

        // Cleanup best-effort
        let _ = fs::remove_file(&path);
    }
}
