use crate::node::miner::signer::MinerSigner;
use alloy_consensus::Transaction as TxTrait;
use alloy_consensus::TxLegacy;
use alloy_primitives::{Address, Bytes, TxKind, B256, U256};
use alloy_sol_macro::sol;
use alloy_sol_types::SolCall;
use rand::Rng;
use reth_primitives::{Transaction, TransactionSigned};
use alloy_consensus::transaction::Recovered;
use reth_primitives_traits::SignerRecoverable;
use secp256k1::SecretKey;
use std::collections::HashMap;

// Simple ERC20 contract - minimal implementation for benchmarking
sol! {
    #[allow(missing_docs)]
    function transfer(address to, uint256 amount) returns (bool);
    function balanceOf(address account) returns (uint256);
}

/// The ERC20 bytecode - a minimal token contract.
/// This is compiled from a simple Solidity ERC20 with mint in constructor.
/// We use a well-known minimal ERC20 bytecode for the benchmark.
pub const SIMPLE_ERC20_BYTECODE: &str = concat!(
    // Constructor: stores msg.sender balance as max uint
    // Runtime: supports transfer(address,uint256) and balanceOf(address)
    "608060405234801561001057600080fd5b50",
    "336000908152602081905260408120",
    "7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff9055",
    "6101e7806100456000396000f3fe",
    "608060405234801561001057600080fd5b50",
    "600436106100365760003560e01c806370a08231",
    "1461003b578063a9059cbb14610061575b600080fd5b",
    "61004e610049366004610152565b610081565b604051908152602001",
    "60405180910390f35b61007461006f366004610174565b61009e565b",
    "604051901515815260200160405180910390f35b",
    "6001600160a01b031660009081526020819052604090205490565b",
    "600080336001600160a01b0316815260208190526040812054",
    "83811015610100576040517f08c379a0000000000000000000000000",
    "000000000000000000000000000000008152600401",
    "6100f790602080825260049082015263189a5b9960e21b604082015260600190565b",
    "60405180910390fd5b336000908152602081905260408082208583900390556001600160a01b03",
    "8516825281208054850190556001915050610148565b92915050565b",
    "60006020828403121561015f57600080fd5b8135",
    "6001600160a01b038116811461014857600080fd5b",
    "6000806040838503121561018757600080fd5b8235",
    "6001600160a01b038116811461019b57600080fd5b94602093909301359350505056fea264"
);

/// Pre-generated pool of pre-recovered transactions.
///
/// Transactions are recovered (ecrecover) eagerly during pool generation so that the
/// benchmark loop only measures block-building work — matching production where txs arrive
/// pre-recovered from the P2P mempool.
pub struct TxPool {
    /// Transactions grouped by block. Each entry is a Vec of pre-recovered txs for one block.
    pub blocks: Vec<Vec<Recovered<TransactionSigned>>>,
    /// ERC20 contract address (deployed in genesis or block 0)
    pub erc20_address: Address,
}

/// Generate the transaction pool for the benchmark.
///
/// Creates `num_blocks` batches of `txs_per_block` transactions.
/// All transactions are ERC20 transfer() calls between funded accounts.
pub fn generate_tx_pool(
    funded_accounts: &[(B256, Address)],
    num_blocks: usize,
    txs_per_block: usize,
    chain_id: u64,
    erc20_address: Address,
) -> TxPool {
    let mut rng = rand::rng();
    let num_accounts = funded_accounts.len();

    // Track nonces per account
    let mut nonces: HashMap<Address, u64> = HashMap::new();
    for (_, addr) in funded_accounts {
        nonces.insert(*addr, 0);
    }

    let mut blocks = Vec::with_capacity(num_blocks);

    for _block_idx in 0..num_blocks {
        let mut block_txs = Vec::with_capacity(txs_per_block);

        for _tx_idx in 0..txs_per_block {
            // Pick random sender and receiver (different accounts)
            let sender_idx = rng.random_range(0..num_accounts);
            let mut receiver_idx = rng.random_range(0..num_accounts);
            while receiver_idx == sender_idx {
                receiver_idx = rng.random_range(0..num_accounts);
            }

            let (sender_key, sender_addr) = &funded_accounts[sender_idx];
            let (_, receiver_addr) = &funded_accounts[receiver_idx];
            let nonce = nonces.get_mut(sender_addr).unwrap();

            // Create ERC20 transfer call
            let amount = U256::from(rng.random_range(1u64..1000));
            let calldata = transferCall { to: *receiver_addr, amount }.abi_encode();

            let tx = Transaction::Legacy(TxLegacy {
                chain_id: Some(chain_id),
                nonce: *nonce,
                gas_limit: 60_000, // ERC20 transfer typically uses ~50k gas
                gas_price: 1,
                value: U256::ZERO,
                input: Bytes::from(calldata),
                to: TxKind::Call(erc20_address),
            });

            // Sign the transaction
            let sk = SecretKey::from_slice(sender_key.as_ref()).expect("valid key");
            let signer = MinerSigner::new(sk);
            let signed = signer.sign_transaction(tx).expect("signing failed");

            *nonce += 1;

            // Pre-recover immediately (mirrors production: txs enter mempool pre-recovered)
            let recovered = signed.try_into_recovered().expect("just-signed tx must recover");
            block_txs.push(recovered);
        }

        // Sort by (sender, nonce) so the EVM processes each sender's txs in order.
        // No ecrecover needed — signer is already cached in Recovered.
        block_txs.sort_by(|a, b| {
            a.signer().cmp(&b.signer()).then(a.nonce().cmp(&b.nonce()))
        });

        blocks.push(block_txs);
    }

    println!(
        "Generated {} blocks x {} txs = {} total ERC20 transfers",
        num_blocks,
        txs_per_block,
        num_blocks * txs_per_block
    );

    TxPool { blocks, erc20_address }
}

/// Get the deploy transaction for a simple ERC20 contract.
/// The deployer gets max balance minted in the constructor.
pub fn erc20_deploy_tx(
    deployer_key: &B256,
    nonce: u64,
    chain_id: u64,
) -> (Recovered<TransactionSigned>, Address) {
    let bytecode = hex::decode(SIMPLE_ERC20_BYTECODE).expect("valid hex bytecode");

    let tx = Transaction::Legacy(TxLegacy {
        chain_id: Some(chain_id),
        nonce,
        gas_limit: 500_000,
        gas_price: 1,
        value: U256::ZERO,
        input: Bytes::from(bytecode),
        to: TxKind::Create,
    });

    let sk = SecretKey::from_slice(deployer_key.as_ref()).expect("valid key");
    let signer = MinerSigner::new(sk);
    let signed = signer.sign_transaction(tx).expect("signing failed");
    let recovered = signed.try_into_recovered().expect("just-signed tx must recover");

    // Compute contract address: keccak256(rlp([sender, nonce]))[12..]
    let deployer_addr = crate::bench::db_init::address_from_private_key(deployer_key);
    let contract_addr = deployer_addr.create(nonce);

    (recovered, contract_addr)
}

/// Create ERC20 transfer transactions to distribute initial tokens to all funded accounts.
/// The deployer (who owns all tokens from constructor) sends tokens to each account.
pub fn erc20_distribution_txs(
    deployer_key: &B256,
    funded_accounts: &[(B256, Address)],
    erc20_address: Address,
    start_nonce: u64,
    chain_id: u64,
) -> Vec<Recovered<TransactionSigned>> {
    let sk = SecretKey::from_slice(deployer_key.as_ref()).expect("valid key");
    let signer = MinerSigner::new(sk);
    let mut txs = Vec::with_capacity(funded_accounts.len());
    let distribution_amount = U256::from(1_000_000_000u64); // 1B tokens each

    for (i, (_, addr)) in funded_accounts.iter().enumerate() {
        let calldata = transferCall { to: *addr, amount: distribution_amount }.abi_encode();

        let tx = Transaction::Legacy(TxLegacy {
            chain_id: Some(chain_id),
            nonce: start_nonce + i as u64,
            gas_limit: 60_000,
            gas_price: 1,
            value: U256::ZERO,
            input: Bytes::from(calldata),
            to: TxKind::Call(erc20_address),
        });

        let signed = signer.sign_transaction(tx).expect("signing failed");
        txs.push(signed.try_into_recovered().expect("just-signed tx must recover"));
    }

    txs
}
