# v0.0.8-beta

## Description

This release introduces the **Mendel hardfork** implementation, upgrades the reth dependency to **v1.10.2**, and includes numerous Parlia consensus fixes and improvements for stability and go-bsc parity.

### Features

* **Mendel Hardfork**: Full implementation of the Mendel hardfork with testnet activation timestamps (Osaka & Mendel)
* **Reth Upgrade**: Upgraded reth dependency from 1.9.3 to 1.10.2
* **Active Validator-Set Callback**: Wired active validator-set callback into BSC RPC builder
* **Ethereum Spec Tests**: Added support for Ethereum execution specification tests
* **Slashing Transaction Test**: Added slashing transaction test coverage

### Improvements

* **Parlia Consensus Parity**: Aligned Parlia implementation with go-bsc parity and added regression tests
* **Parlia Cache Warming**: Pre-warm cache on restart; fall back to header cache for votes/attestations when DB lags
* **TD Computation Fix**: Fixed total difficulty computation and querying issues
* **System Transaction Handling**: Bypass EIP-7825 gas limit cap for BSC system transactions; fixed Osaka gas limit
* **RPC Fixes**: Header RLP encoding fixes
* **Blob Handling**: Fixed blob bug and blob fee tracking
* **Receipt Root**: Ensured assembler and consensus use the same `calculate_receipt_root`
* **BSC Consensus Ports**: Ported consensus fixes from go-bsc (bnb-chain/bsc#3575, bnb-chain/bsc#3569)
* **Snapshots**: Updated reth-bsc snapshots

## What's Changed

* feat: update reth to 1.9.3 by @matuskysel
* fix: track blob fee by @matuskysel
* feat: implement mendel HF by @matuskysel
* chore: add Osaka and Mendel hardfork timestamps for testnet by @constwz in #280

**Full Changelog**: v0.0.7-beta...v0.0.8-beta
