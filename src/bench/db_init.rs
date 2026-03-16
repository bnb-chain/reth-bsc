use crate::bench::config::BenchConfig;
use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::Parlia;
use crate::consensus::parlia::provider::{EnhancedDbSnapshotProvider, SnapshotProvider};
use crate::consensus::parlia::snapshot::Snapshot;
use crate::hardforks::bsc::BscHardfork;
use crate::node::BscNode;
use crate::node::evm::util::insert_header_to_cache;

use alloy_consensus::Header;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, B256, Keccak256, U256};
use reth::api::NodeTypesWithDBAdapter;
use reth_chainspec::{
    BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, NamedChain, make_genesis_header,
};
use reth_db::{DatabaseEnv, init_db, mdbx::DatabaseArguments};
use reth_db_common::init::init_genesis;
use reth_primitives::SealedHeader;
use reth_provider::{ProviderFactory, providers::StaticFileProvider};
use rust_eth_triedb::triedb_manager::init_global_triedb_manager;
use secp256k1::{PublicKey, SECP256K1, SecretKey};
use std::collections::HashMap;
use std::sync::Arc;

/// The concrete NodeTypes adapter for the benchmark ProviderFactory.
pub type BscNodeTypes = NodeTypesWithDBAdapter<BscNode, Arc<DatabaseEnv>>;

/// Result of database initialization
pub struct InitResult {
    pub chain_spec: Arc<BscChainSpec>,
    pub genesis_header: SealedHeader,
    pub genesis_snapshot: Snapshot,
    pub parlia: Arc<Parlia<BscChainSpec>>,
    pub snapshot_provider: Arc<EnhancedDbSnapshotProvider<Arc<DatabaseEnv>>>,
    pub validator_addresses: Vec<Address>,
    pub funded_accounts: Vec<(B256, Address)>,
    pub factory: ProviderFactory<BscNodeTypes>,
    pub temp_dir: std::path::PathBuf,
    pub snapshot_db: Arc<DatabaseEnv>,
}

/// Derive Address from a private key B256
pub fn address_from_private_key(key: &B256) -> Address {
    let sk = SecretKey::from_slice(key.as_ref()).expect("invalid private key");
    let pk = PublicKey::from_secret_key(SECP256K1, &sk);
    let uncompressed = pk.serialize_uncompressed();
    let mut hasher = Keccak256::new();
    hasher.update(&uncompressed[1..]);
    let hash = hasher.finalize();
    Address::from_slice(&hash[12..])
}

/// Create the BscChainSpec from a user-provided genesis JSON.
pub fn create_chain_spec(genesis: Genesis) -> Arc<BscChainSpec> {
    let hardforks = BscHardfork::bsc_local();
    let genesis_header = make_genesis_header(&genesis, &hardforks);
    let hash = genesis_header.hash_slow();

    let inner = ChainSpec {
        chain: Chain::from_named(NamedChain::BinanceSmartChain),
        genesis,
        paris_block_and_final_difficulty: Some((0, U256::from(0))),
        hardforks,
        deposit_contract: None,
        base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
        prune_delete_limit: 3500,
        genesis_header: SealedHeader::new(genesis_header, hash),
        ..Default::default()
    };
    Arc::new(BscChainSpec { inner })
}

/// Initialize the benchmark database and all infrastructure from genesis.
///
/// Creates a real MDBX database with ProviderFactory, writes genesis state
/// via `init_genesis`, and returns a fully-initialized `InitResult` that
/// can serve state via `factory.latest()` / `factory.history_by_block_number()`.
///
/// If `config.triedb` is true, trieDB is initialized BEFORE genesis so that
/// `init_genesis` writes state to the trieDB PathDB instead of the MDBX trie tables.
pub fn init_benchmark(config: &BenchConfig) -> eyre::Result<InitResult> {
    // 1. Load genesis JSON
    let genesis_data = std::fs::read_to_string(&config.genesis_path)
        .map_err(|e| eyre::eyre!("Failed to read genesis file: {}", e))?;
    let genesis: Genesis = serde_json::from_str(&genesis_data)
        .map_err(|e| eyre::eyre!("Failed to parse genesis JSON: {}", e))?;

    // 2. Generate funded accounts early so we can inject them into genesis alloc
    let funded_accounts = generate_funded_accounts(config.funded_accounts);

    // 3. Inject funded accounts into genesis alloc (so init_genesis writes them to MDBX)
    let balance = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18)); // 1M BNB
    let mut genesis = genesis;
    for (_, addr) in &funded_accounts {
        genesis
            .alloc
            .entry(*addr)
            .or_insert_with(|| alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }

    // 3b. Inject background accounts to inflate the state trie (simulates mainnet state size)
    if config.background_accounts > 0 {
        println!(
            "  Generating {} background accounts with {} storage slots each...",
            config.background_accounts, config.storage_slots_per_account
        );
        let small_balance = U256::from(1u64) * U256::from(10u64).pow(U256::from(18)); // 1 BNB
        for i in 0..config.background_accounts {
            let mut key_bytes = [0u8; 32];
            key_bytes[0] = 0xBB; // prefix for background accounts
            let idx_bytes = (i as u64 + 1).to_be_bytes();
            key_bytes[24..32].copy_from_slice(&idx_bytes);
            let addr = address_from_private_key(&B256::from(key_bytes));

            // Add storage slots (simulates ERC20 balances, approvals, etc.)
            let storage = if config.storage_slots_per_account > 0 {
                let mut map = std::collections::BTreeMap::new();
                for s in 0..config.storage_slots_per_account {
                    let mut slot_bytes = [0u8; 32];
                    slot_bytes[24..32].copy_from_slice(&(s as u64).to_be_bytes());
                    let slot = B256::from(slot_bytes);
                    let mut val_bytes = [0u8; 32];
                    val_bytes[31] = 1; // non-zero value
                    map.insert(slot, B256::from(val_bytes));
                }
                Some(map)
            } else {
                None
            };

            genesis.alloc.entry(addr).or_insert_with(|| alloy_genesis::GenesisAccount {
                balance: small_balance,
                storage,
                ..Default::default()
            });
        }
        println!("  Injected {} background accounts into genesis", config.background_accounts);
    }

    // 4. Create chain spec (with funded accounts in alloc)
    let chain_spec = create_chain_spec(genesis);

    // 5. Get validator addresses from private keys
    let validator_addresses: Vec<Address> =
        config.private_keys.iter().map(address_from_private_key).collect();

    println!("Validators:");
    for (i, addr) in validator_addresses.iter().enumerate() {
        println!("  [{}] {}", i, addr);
    }

    // 4. Create temp directory structure for MDBX + static files
    let temp_dir = std::env::temp_dir().join(format!("miner_bench_{}", std::process::id()));
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir)?;
    }
    std::fs::create_dir_all(&temp_dir)?;

    let db_path = temp_dir.join("db");
    std::fs::create_dir_all(&db_path)?;

    let static_files_path = temp_dir.join("static_files");
    std::fs::create_dir_all(&static_files_path)?;

    // Production initializes the global triedb manager before any genesis/state writes.
    // The benchmark must do the same or `is_triedb_active()` stays false and no difflayers
    // or prefetchers are ever produced during block finalization.
    if config.triedb {
        let triedb_path = temp_dir.join("rust_eth_triedb");
        std::fs::create_dir_all(&triedb_path)?;
        let triedb_path_str = triedb_path.to_string_lossy();
        init_global_triedb_manager(triedb_path_str.as_ref());
    }

    // 5. Create MDBX database
    let db = Arc::new(
        init_db(&db_path, DatabaseArguments::new(Default::default()))
            .map_err(|e| eyre::eyre!("Failed to create main database: {}", e))?,
    );

    // 6. Create StaticFileProvider
    let static_file_provider = StaticFileProvider::read_write(&static_files_path)
        .map_err(|e| eyre::eyre!("Failed to create static file provider: {}", e))?;

    // 7. Create ProviderFactory
    let rocksdb_provider = reth_provider::providers::RocksDBProvider::new(temp_dir.join("rocksdb"))
        .map_err(|e| eyre::eyre!("Failed to create RocksDB provider: {}", e))?;
    let factory = ProviderFactory::<BscNodeTypes>::new(
        db.clone(),
        chain_spec.clone(),
        static_file_provider,
        rocksdb_provider,
    )
    .map_err(|e| eyre::eyre!("Failed to create ProviderFactory: {}", e))?;

    // 8. Write genesis state to the database
    //    When trieDB is active, this writes state to trieDB PathDB instead of MDBX trie tables
    let genesis_hash =
        init_genesis(&factory).map_err(|e| eyre::eyre!("Failed to initialize genesis: {}", e))?;

    println!("Genesis initialized in MDBX, hash: {}", genesis_hash);

    // 9. Create snapshot MDBX database (for Parlia consensus snapshots)
    let snap_db_path = temp_dir.join("snap_db");
    std::fs::create_dir_all(&snap_db_path)?;
    let snapshot_db = Arc::new(
        init_db(&snap_db_path, DatabaseArguments::new(Default::default()))
            .map_err(|e| eyre::eyre!("Failed to create snapshot database: {}", e))?,
    );

    // 10. Create Parlia consensus
    let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));

    // 11. Get the genesis header from chain spec
    let genesis_header_ref = chain_spec.inner.genesis_header();
    let genesis_sealed_hash = genesis_header_ref.hash_slow();
    let genesis_sealed = SealedHeader::new(genesis_header_ref.clone(), genesis_sealed_hash);

    // 12. Insert genesis header into the header cache
    insert_header_to_cache(genesis_header_ref.clone());

    // 13. Create genesis snapshot from header validators
    let genesis_snapshot =
        create_genesis_snapshot(&parlia, genesis_header_ref, &validator_addresses);

    // 14. Create snapshot provider and insert genesis snapshot
    let snapshot_provider =
        Arc::new(EnhancedDbSnapshotProvider::new(snapshot_db.clone(), 2048, chain_spec.clone()));
    snapshot_provider.insert(genesis_snapshot.clone());

    // 15. Register snapshot provider globally (needed by BscBlockExecutor::prepare_new_block)
    let _ = crate::shared::set_snapshot_provider(
        snapshot_provider.clone() as Arc<dyn SnapshotProvider + Send + Sync>
    );

    println!(
        "Database initialized: {} funded accounts, genesis hash: {}",
        funded_accounts.len(),
        genesis_hash
    );

    Ok(InitResult {
        chain_spec,
        genesis_header: genesis_sealed,
        genesis_snapshot,
        parlia,
        snapshot_provider,
        validator_addresses,
        funded_accounts,
        factory,
        temp_dir,
        snapshot_db,
    })
}

/// Create a genesis snapshot for the validator set.
fn create_genesis_snapshot(
    parlia: &Parlia<BscChainSpec>,
    genesis_header: &Header,
    validators: &[Address],
) -> Snapshot {
    // Try to parse validators from the genesis header extra_data first
    let epoch_length = parlia.epoch;
    let parsed = parlia.parse_validators_from_header(genesis_header, epoch_length);

    let (addrs, vote_addrs) = match parsed {
        Ok(info) => {
            let vote = info.vote_addrs;
            (info.consensus_addrs, vote)
        }
        Err(_) => {
            // Fallback: use provided validators with empty vote addresses
            use alloy_primitives::FixedBytes;
            let vote: Option<Vec<FixedBytes<48>>> = Some(vec![FixedBytes::ZERO; validators.len()]);
            (validators.to_vec(), vote)
        }
    };

    Snapshot::new(addrs, 0, genesis_header.hash_slow(), epoch_length, vote_addrs)
}

/// Generate N funded accounts, returning (private_key, address) pairs.
fn generate_funded_accounts(count: usize) -> Vec<(B256, Address)> {
    let mut accounts = Vec::with_capacity(count);
    for i in 0..count {
        // Deterministic key generation for reproducibility
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = 0x01; // prefix to avoid zero key
        let idx_bytes = (i as u64 + 1).to_be_bytes();
        key_bytes[24..32].copy_from_slice(&idx_bytes);
        let key = B256::from(key_bytes);
        let addr = address_from_private_key(&key);
        accounts.push((key, addr));
    }
    accounts
}

/// Build genesis alloc entries for funded accounts and validators.
/// Returns a map of address -> (balance, nonce, code, storage) suitable for genesis JSON.
pub fn build_funded_alloc(
    funded_accounts: &[(B256, Address)],
    validators: &[Address],
) -> HashMap<Address, alloy_genesis::GenesisAccount> {
    let mut alloc = HashMap::new();
    let balance = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18)); // 1M BNB each

    for (_, addr) in funded_accounts {
        alloc.insert(*addr, alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }

    // Also fund validators
    for addr in validators {
        alloc.insert(*addr, alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }

    alloc
}
