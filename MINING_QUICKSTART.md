# BSC Mining Quick Start Guide

This guide shows how to quickly start mining with reth-bsc using automatic key generation.

## 🚀 Super Quick Start (Auto-Generated Keys)

Just set one environment variable and start mining:

```bash
export BSC_MINING_ENABLED=true
cargo run -- --chain bsc --datadir ./test-datadir
```

**That's it!** The system will:
1. ✅ Automatically generate a validator private key
2. ✅ Derive the validator address from the key
3. ✅ Start mining with proper Parlia consensus
4. ✅ Display your keys in the logs (save them!)

## 📋 What You'll See

When you start mining, you'll see logs like this:

```
WARN Mining enabled but no keys provided - generating development keys
WARN 🔑 AUTO-GENERATED validator keys for development:
WARN 📍 Validator Address: 0x1234567890abcdef1234567890abcdef12345678
WARN 🔐 Private Key: 0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef (KEEP SECURE!)
WARN ⚠️  These are DEVELOPMENT keys - do not use in production!
INFO Starting BSC mining service for validator: 0x1234567890abcdef1234567890abcdef12345678
```

**💾 SAVE THESE KEYS!** You'll need them to continue mining with the same identity.

## 🔧 Configuration Options

### Environment Variables

```bash
# Enable/disable mining
export BSC_MINING_ENABLED=true

# Use your own keys (optional)
export BSC_PRIVATE_KEY=0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef

# Mining parameters (optional)
export BSC_GAS_LIMIT=30000000
export BSC_MINING_INTERVAL_MS=500
```

### Using Your Own Keys

If you have existing validator keys:

```bash
export BSC_MINING_ENABLED=true
export BSC_PRIVATE_KEY=0xYOUR_PRIVATE_KEY
cargo run
```

### One-Line Mining

```bash
BSC_MINING_ENABLED=true cargo run -- --chain bsc --datadir ./datadir
```

## 🛡️ Security Notes

### Development vs Production

**Development (Current Setup)**:
- ✅ Auto-generates keys for easy testing
- ✅ Keys displayed in logs for convenience
- ⚠️ Not secure for real BSC network

**Production Setup**:
- 🔒 Store keys in hardware security modules (HSM)
- 🔒 Never log private keys
- 🔒 Use environment variables or secure vaults

### Key Security

```bash
# Generate secure private key
openssl rand -hex 32

# Or use existing BSC validator keys
# Make sure your validator is registered on the BSC network
```

## 🔍 Monitoring

Monitor your mining activity:

```bash
# Watch for mining logs
tail -f logs/reth.log | grep -i mining

# Key log messages:
# - "Starting BSC mining service"
# - "Mining new block on top of block"
# - "Successfully mined block"
```

## 🔄 Different Mining Modes

### 1. Development Mode (Auto Keys)
```bash
BSC_MINING_ENABLED=true cargo run
```

### 2. Test Mode (Specific Keys)  
```bash
BSC_MINING_ENABLED=true \
BSC_PRIVATE_KEY=0xabc... \
cargo run
```

### 3. Configuration File Mode
Config files are not supported. Use env vars or CLI.

## ❓ Troubleshooting

### Mining Not Starting
```
INFO Mining is disabled in configuration
```
**Solution**: Set `BSC_MINING_ENABLED=true`

### Key Generation Failed
```
ERROR Failed to create mining service: No signing key configured
```
**Solution**: Check that `BSC_MINING_ENABLED=true` and restart

### Database Issues
```
WARN Failed to open database for mining: ..., mining disabled
```
**Solution**: Make sure the `--datadir` path exists and is writable

### Not Authorized to Mine
```
DEBUG Mining attempt failed: Not authorized validator: 0x...
```
**Solution**: Your validator isn't in the current validator set. This is normal for development - you're testing the mining logic even if blocks won't be accepted by the real network.

## 🎯 Next Steps

1. **Test Basic Mining**: Start with auto-generated keys
2. **Save Your Keys**: Copy the generated keys for reuse
3. **Monitor Logs**: Watch for successful block creation
4. **Customize Config**: Set gas limits, intervals, etc.
5. **Production Setup**: Implement proper key management

## ⚡ Advanced Usage

### Custom Gas Limit
```bash
BSC_MINING_ENABLED=true BSC_GAS_LIMIT=25000000 cargo run
```

### Faster Mining Interval
```bash
BSC_MINING_ENABLED=true BSC_MINING_INTERVAL_MS=200 cargo run
```

### BEP-675 BidBlock over gRPC

The optional gRPC listener exposes the same BidBlock admission path as
`mev_sendBidBlock`. Enable both BidBlock admission and the listener:

```bash
./target/release/reth-bsc node \
  --chain bsc \
  --mining.enabled \
  --mining.bid-block-enabled \
  --mining.mev-grpc-port 8553
```

Equivalent environment settings:

```bash
export BSC_MINING_BID_BLOCK_ENABLED=true
export BSC_MEV_GRPC_PORT=8553
export BSC_MEV_GRPC_CONCURRENCY=32
export BSC_MEV_GRPC_REQUEST_TIMEOUT_MS=10000
```

The protobuf service is `mev.v1.BidBlockService`, with unary method
`SendBidBlock`. `bid_block_rlp` is the geth RLP encoding of
`[header, transactions, sidecars]`; `signature` is the builder's 65-byte ECDSA
signature. See [`proto/mev.proto`](proto/mev.proto) for the complete schema.

The listener binds to the configured HTTP host and is plaintext, matching the
go-bsc endpoint. Keep it on a private validator/sentry network or protect it
with a trusted proxy, network ACL, or mTLS termination.

### Programmatic Configuration
```rust
use reth_bsc::node::mining_config::MiningConfig;

// Quick development setup
let config = MiningConfig::development();

// Or from environment
let config = MiningConfig::from_env();
```

## 🚨 Important Notes

- **Auto-generated keys are for DEVELOPMENT only**
- **Save the generated keys if you want to reuse the same validator identity**
- **Mining will work but blocks may not be accepted without proper validator registration**
- **This is perfect for testing mining logic and development**

Start mining now with just one command:
```bash
BSC_MINING_ENABLED=true cargo run -- --chain bsc --datadir ./test-mining
```
