# Reth @ BSC

A BSC-compatible Reth client implementation. This project is **not** a fork of Reth, but rather an extension that leverages Reth's powerful `NodeBuilder` API to provide BSC compatibility.

## About

This project aims to bring Reth's high-performance Ethereum client capabilities to the BSC network. By utilizing Reth's modular architecture and NodeBuilder API, we're building a BSC-compatible client that maintains compatibility with Reth's ecosystem while adding BSC-specific features.

## Current Status

- Historical Sync ✅
- BSC Pectra Support ✅
- Live Sync ✅
- Run as validator ❌ (soon)

### Sync Status (as of September 1st, 2025)

- **BSC Mainnet**: Synced to the tip ✅ (10.6T disk required)
- **BSC Testnet**: Synced to the tip ✅ (800GB disk usage)

## Building

### Clone and Build

```bash
# Clone the repository
git clone https://github.com/bnb-chain/reth-bsc.git
cd reth-bsc

# use cargo
cargo build  # debug mode
cargo build --release  # release mode 

# use makefile
# release mode default. 
# cargo build --bin reth-bsc --features "jemalloc,asm-keccak" --profile "release"
make build

# build in max perf profile
# RUSTFLAGS="-C target-cpu=native" cargo build --bin reth-bsc --profile maxperf --features jemalloc,asm-keccak
make maxperf 
```

## Running

### Full Node (Recommended)

A full node stores recent state and can serve RPC requests efficiently:

```bash
./target/${profile}/reth-bsc node --full --chain bsc --datadir ./data_dir
```

### Archive Node

An archive node stores the complete blockchain history and state:

```bash
./target/${profile}/reth-bsc node --chain bsc --datadir ./data_dir
```

### BSC Testnet

To run on BSC Testnet instead of Mainnet, simply replace `--chain bsc` with `--chain bsc-testnet` in any of the above commands:

```bash
# Example: Full node on BSC Testnet
./target/${profile}/reth-bsc node --full --chain bsc-testnet --datadir ./data_dir
```

### RPC Configuration

To enable RPC services, add these parameters to your node command:

#### HTTP & Websocket
```bash
# Full node with HTTP and WebSocket RPC enabled
./target/${profile}/reth-bsc node --full \
  --chain bsc \
  --datadir ./data_dir \
  --http \
  --http.addr 0.0.0.0 \
  --http.port 8545 \
  --http.api eth,net,web3,txpool,debug \
  --ws \
  --ws.addr 0.0.0.0 \
  --ws.port 8546 \
  --ws.api eth,net,web3,txpool
```

### Available API Modules

- `eth`: Ethereum JSON-RPC API (block info, transactions, etc.)
- `net`: Network information (peer count, network ID)
- `web3`: Web3 standard APIs (client version, sha3)
- `txpool`: Transaction pool information
- `debug`: Debug APIs for development (tracing, profiling)
- `trace`: Transaction tracing (requires archive node)
- `admin`: Administrative APIs (peer management)


## Sync

### 1. Genesis Sync

Sync from block 0 (will take weeks):

```bash
./target/release/reth-bsc node --chain bsc --datadir=./data_dir
```

### 2. Snapshot Sync

Refer to the [SNAPSHOT.md](https://github.com/bnb-chain/reth-bsc/blob/main/SNAPSHOT.md) for snapshot information

## Monitoring

### Check Sync Status

```bash
# Check current block
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' \
  http://localhost:8545

# Check sync status
curl -X POST \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","method":"eth_syncing","params":[],"id":1}' \
  http://localhost:8545
```

### Metrics

Enable metrics endpoint for monitoring:

```bash
--metrics 0.0.0.0:6060
```

Then access metrics at `http://localhost:6060/metrics`

### Logs

Enable detailed logging:

```bash
--log.file.verbosity debug \
--log.file.directory ./logs
```

## Contributing

We welcome community contributions! Whether you're interested in helping with historical sync implementation, BSC Pectra support, or live sync functionality, your help is valuable. Please feel free to open issues or submit pull requests. You can reach out to me on [Telegram](https://t.me/loocapro).

## Disclaimer

This project is experimental and under active development. Use at your own risk. Always backup your data and test on testnet first.

## Credits

This project is inspired by and builds upon the work of:

- [BNB Chain Reth](https://github.com/bnb-chain/reth) - The original BSC implementation of Reth
- The Reth team, especially [@mattsse](https://github.com/mattsse) for their invaluable contributions to the Reth ecosystem

## Acknowledgements from BNBChain team

This project based on the excellent community versions as foundation, We extend our sincere appreciation to the teams below:
- [Reth-bsc](https://github.com/loocapro/reth-bsc) - The BSC Reth implementation from community
- [Reth](https://github.com/paradigmxyz/reth) - The reth project
- Especially thanks to:
  - [@loocapro](https://github.com/loocapro)
  - [@mattsse](https://github.com/mattsse)
  - [@klkvr](https://github.com/klkvr)
  - All contributors on reth and reth-bsc
