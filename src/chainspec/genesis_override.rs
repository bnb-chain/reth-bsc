use std::sync::OnceLock;
use alloy_primitives::B256;
use eyre::Result;

/// Global storage for genesis hash override
static GENESIS_HASH_OVERRIDE: OnceLock<Option<B256>> = OnceLock::new();

/// Set the global genesis hash override
pub fn set_genesis_hash_override(hash_str: Option<String>) -> Result<()> {
    let hash = match hash_str {
        Some(s) => {
            let hash = s.parse::<B256>()
                .map_err(|e| eyre::eyre!("Invalid genesis hash format: {}", e))?;
            Some(hash)
        }
        None => None,
    };
    
    GENESIS_HASH_OVERRIDE.set(hash)
        .map_err(|_| eyre::eyre!("Genesis hash override already set"))?;
    
    if let Some(hash) = hash {
        tracing::info!("Genesis hash override set to: {:#x}", hash);
    }
    
    Ok(())
}

/// Get the global genesis hash override
pub fn get_genesis_hash_override() -> Option<B256> {
    GENESIS_HASH_OVERRIDE.get().and_then(|h| *h)
}

/// Validate a block hash against the genesis hash override if set
pub fn validate_genesis_hash(block_hash: B256) -> bool {
    match get_genesis_hash_override() {
        Some(expected_hash) => {
            let valid = block_hash == expected_hash;
            if !valid {
                tracing::warn!(
                    "Genesis hash validation failed: expected {:#x}, got {:#x}",
                    expected_hash,
                    block_hash
                );
            } else {
                tracing::debug!("Genesis hash validation passed: {:#x}", block_hash);
            }
            valid
        }
        None => {
            // No genesis hash override set, always valid
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_genesis_hash_parsing() {
        let hash_str = "0xb4844167d735617495363867c84affa9f4069bcdae48411ae3badbe1d227d3e5".to_string();
        set_genesis_hash_override(Some(hash_str)).expect("Should set genesis hash override");
        
        let expected = "0xb4844167d735617495363867c84affa9f4069bcdae48411ae3badbe1d227d3e5"
            .parse::<B256>()
            .unwrap();
        
        assert_eq!(get_genesis_hash_override(), Some(expected));
    }

    #[test]
    fn test_genesis_hash_validation() {
        // Reset the global state for this test
        // Note: In real tests, you'd want to use a different approach to avoid global state
        let test_hash = "0xb4844167d735617495363867c84affa9f4069bcdae48411ae3badbe1d227d3e5"
            .parse::<B256>()
            .unwrap();
        
        // Test validation when no hash is set (should always pass)
        assert!(validate_genesis_hash(test_hash));
        
        // Test validation when hash matches
        // (This test would need a way to reset global state to work properly)
    }
}