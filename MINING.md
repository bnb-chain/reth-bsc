# BSC PoSA Mining Implementation

## Overview

This implementation adds Proof-of-Authority (PoSA) mining capability to reth-bsc, integrating with the existing Parlia consensus mechanism. The mining service automatically produces blocks when the node is configured as a validator.

## Architecture

### Key Components

1. **BscPoSAMiner** (`src/node/engine.rs`): Main mining service
   - Monitors validator authorization
   - Implements turn-based block production
   - Handles backoff timing for non-in-turn validators
   - Collects transactions from the mempool
   - Seals blocks using Parlia consensus

2. **SealBlock** (`src/consensus/parlia/seal.rs`): Block sealing logic
   - Integrates with Parlia consensus
   - Handles validator signatures
   - Implements vote attestation for governance

3. **SnapshotProvider** (`src/consensus/parlia/provider.rs`): Validator set management
   - Tracks current validator set
   - Manages validator rotation at epoch boundaries
   - Provides validator authorization checks

## Configuration

Use environment variables or CLI flags. Config files (TOML) are not supported.

### Environment Variables

Set configuration via environment:

```bash
export BSC_MINING_ENABLED=true
export BSC_PRIVATE_KEY=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
```

### Command Line Arguments

Pass configuration when starting the node:

```bash
reth-bsc --mining.enabled --mining.private-key 0x0123...
```

### Programmatic Configuration

For custom setups, create the config in code:

```rust
use crate::node::mining_config::MiningConfig;

let mining_config = MiningConfig {
    enabled: true,
    private_key_hex: Some("0123456789abcdef...".to_string()),
    gas_limit: Some(30_000_000),
    mining_interval_ms: Some(500),
    ..Default::default()
};
```

## Security Considerations

### ⚠️ Private Key Management

**NEVER** put private keys directly in configuration files in production. Use one of these secure approaches:

1. **Hardware Security Modules (HSMs)**: For production validators
2. **Environment Variables**: Better than files, but still not ideal for production
3. **Secure Key Vaults**: AWS KMS, HashiCorp Vault, etc.

### Key Validation

The system automatically derives the validator address from your private key:

```rust
// Derives address from private key and compares with configured address
let derived_address = keystore::get_validator_address(&signing_key);
// This derived address is used by the miner; no separate configuration required
```

## Mining Behavior

### Block Production Timing

1. **In-turn validators**: Mine immediately when scheduled (every ~3 seconds)
2. **Out-of-turn validators**: Wait for backoff time based on position in validator set
3. **Recently signed**: Skip turn to allow other validators to participate

### Validator Authorization

- Checks current validator set from snapshots
- Verifies node's validator address is authorized
- Implements recent signing restrictions to prevent spam

### Transaction Collection

- Fetches best transactions from the transaction pool
- Respects block gas limit constraints
- Currently simplified - full transaction processing to be implemented

## Current Limitations & TODOs

### 1. Transaction Processing
```rust
// TODO: Implement proper transaction cloning based on transaction type
// Current implementation skips actual transaction inclusion
```

### 2. Block Submission
```rust
// TODO: Implement block submission to engine API
// Currently just logs successful block creation
```

### 3. Chain State Integration
```rust
// TODO: Get current head block from chain state
// Currently uses mock block header
```

### 4. Configuration System
- Validator address configuration
- Private key management
- Database path configuration
- Mining parameters (block time, gas limits)

## Usage

### Starting Mining

The mining service automatically starts when the node launches with the PoSA payload service configured. It runs in a continuous loop:

1. Check if authorized to mine
2. Calculate optimal mining time based on turn
3. Collect transactions from pool
4. Build and seal block using Parlia consensus
5. Submit block (when implemented)

### Monitoring

Mining activity is logged with different levels:

```
INFO: Starting BSC PoSA mining service for validator: 0x...
INFO: Mining new block on top of block 12345
DEBUG: Mining attempt failed: Too early to mine, wait until 1640995200
```

### Development Testing

For development, you can:

1. Configure your private key for signing (address is derived automatically)
2. Ensure the database path is correct
3. Monitor logs for mining activity

## Integration Points

### With reth-bsc Components

- **Engine API**: Integrates with `BscPayloadServiceBuilder`
- **Consensus**: Uses existing Parlia consensus logic
- **Transaction Pool**: Pulls transactions for block building
- **Storage**: Accesses snapshots for validator authorization

### With BSC Network

- **Validator Set**: Respects BSC validator rotation
- **Block Time**: Maintains ~3 second block intervals
- **Governance**: Supports vote attestation mechanism

## Next Steps

1. **Complete Transaction Integration**: Implement proper transaction collection and processing
2. **Block Submission**: Connect to engine API or direct chain import
3. **Configuration System**: Add proper config file support
4. **Testing**: Add comprehensive tests for mining logic
5. **Monitoring**: Implement metrics and monitoring
6. **Error Handling**: Improve error recovery and retry logic

## Quick Start Guide

### Step 1: Generate Validator Keys

```bash
# Generate a new private key (for testing)
openssl rand -hex 32

 
```

### Start Mining

```bash
cargo build --release
./target/release/reth-bsc --chain bsc --datadir ./datadir
```

Monitor logs for mining activity:
- `INFO: Starting BSC PoSA mining service for validator: 0x...`
- `INFO: Mining new block on top of block 12345`

### Verify Mining

Check that blocks are being produced with your derived validator address as the beneficiary.

## Implementation Status

✅ **Complete**:
- Mining service architecture
- Validator authorization
- Turn-based mining with backoff
- Configuration system
- Security validation

🚧 **In Progress**:
- Configuration file loading
- Transaction processing
- Block submission to chain

🔄 **Todo**:
- Complete transaction integration
- Engine API connection
- Comprehensive testing
- Production hardening

This implementation provides the foundation for PoSA mining in reth-bsc. The modular design allows for gradual enhancement of each component while maintaining compatibility with the existing codebase.
