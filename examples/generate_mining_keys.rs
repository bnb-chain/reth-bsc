#!/usr/bin/env cargo
//! Generate mining keys for BSC development
//!
//! Usage: cargo run --example generate_mining_keys

use reth_bsc::node::miner::MiningConfig;

fn main() {
    println!("🔑 BSC Mining Key Generator");
    println!("============================");

    // Generate development keys
    let config = MiningConfig::development();

    if let (Some(address), Some(private_key)) = (config.validator_address, config.private_key_hex) {
        println!("✅ Generated new validator keys:");
        println!();
        println!("📍 Validator Address: {}", address);
        println!("🔐 Private Key: {}", private_key);
        println!();
        println!("💾 To use these keys (validator address is derived from the private key):");
        println!("export BSC_MINING_ENABLED=true");
        println!("export BSC_PRIVATE_KEY={}", private_key);
        println!();
        println!("🚀 Then start mining with:");
        println!("cargo run -- --chain bsc --datadir ./datadir");
        println!();
        println!("⚠️  Keep your private key secure!");
        println!("⚠️  These are development keys - not for mainnet!");
    } else {
        eprintln!("❌ Failed to generate keys");
    }
}
