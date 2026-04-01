use crate::bench::cache::{self, CacheKind, CacheMetadata};
use crate::bench::config::BenchConfig;
use crate::chainspec::BscChainSpec;
use crate::consensus::parlia::provider::{EnhancedDbSnapshotProvider, SnapshotProvider};
use crate::consensus::parlia::snapshot::Snapshot;
use crate::consensus::parlia::Parlia;
use crate::hardforks::bsc::BscHardfork;
use crate::node::evm::util::insert_header_to_cache;
use crate::node::BscNode;

use alloy_consensus::Header;
use alloy_genesis::Genesis;
use alloy_primitives::{Address, Keccak256, B256, U256};
use eyre::Context;
use reth::api::NodeTypesWithDBAdapter;
use reth_chainspec::{
    make_genesis_header, BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, NamedChain,
};
use reth_db::{init_db, mdbx::DatabaseArguments, DatabaseEnv};
use reth_db_common::init::init_genesis;
use reth_primitives::SealedHeader;
use reth_provider::{
    providers::StaticFileProvider, BlockNumReader, HeaderProvider, ProviderFactory,
};
use rust_eth_triedb::triedb_manager::init_global_triedb_manager;
use secp256k1::{PublicKey, SecretKey, SECP256K1};
use std::collections::HashMap;
use std::path::PathBuf;
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
    pub temp_dir: PathBuf,
    pub snapshot_db: Arc<DatabaseEnv>,
    pub source_genesis: String,
}

pub struct RestoredBenchmark {
    pub init: InitResult,
    pub parent_header: SealedHeader,
    pub parent_snapshot: Snapshot,
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

/// Initialize the benchmark database and all infrastructure from genesis or a cached genesis DB.
pub fn init_benchmark(config: &BenchConfig) -> eyre::Result<InitResult> {
    let source_genesis = read_source_genesis(config)?;

    if config.wants_genesis_cache() {
        if let Some(restored) =
            cache::try_restore_cache(config, CacheKind::Genesis, &source_genesis)?
        {
            println!("  Reusing cached genesis DB from {}", restored.work_dir.display());
            return open_existing_runtime(config, source_genesis, restored.work_dir);
        }
    }

    let mut genesis = parse_source_genesis(&source_genesis)?;
    let funded_accounts = generate_funded_accounts(config.funded_accounts);
    inject_funded_accounts(&mut genesis, &funded_accounts);
    inject_background_accounts(&mut genesis, config);

    let temp_dir = cache::create_work_dir()?;
    cache::write_materialized_genesis(&temp_dir, &genesis)?;

    let init =
        open_runtime(config, source_genesis.clone(), temp_dir, genesis, funded_accounts, true)?;

    if config.wants_genesis_cache() {
        cache::persist_cache(
            config,
            CacheKind::Genesis,
            &source_genesis,
            &init.temp_dir,
            &CacheMetadata::genesis(),
        )?;
    }

    Ok(init)
}

pub fn try_restore_post_setup(config: &BenchConfig) -> eyre::Result<Option<RestoredBenchmark>> {
    if !config.wants_post_setup_cache() {
        return Ok(None);
    }

    let source_genesis = read_source_genesis(config)?;
    let Some(restored) = cache::try_restore_cache(config, CacheKind::PostSetup, &source_genesis)?
    else {
        return Ok(None);
    };

    println!("  Reusing cached post-setup DB from {}", restored.work_dir.display());

    let metadata = restored.metadata;
    let init = open_existing_runtime(config, source_genesis, restored.work_dir)?;

    let parent_block_number = metadata
        .parent_block_number
        .ok_or_else(|| eyre::eyre!("post-setup cache metadata missing parent_block_number"))?;
    let expected_parent_hash = metadata
        .parent_block_hash
        .ok_or_else(|| eyre::eyre!("post-setup cache metadata missing parent_block_hash"))?;
    let parent_snapshot = metadata
        .parent_snapshot
        .ok_or_else(|| eyre::eyre!("post-setup cache metadata missing parent_snapshot"))?;

    let parent_header = init
        .factory
        .sealed_header(parent_block_number)
        .wrap_err("failed to query cached post-setup header")?
        .ok_or_else(|| {
            eyre::eyre!("cached post-setup header {} not found in restored DB", parent_block_number)
        })?;

    if parent_header.hash() != expected_parent_hash {
        eyre::bail!(
            "post-setup cache header hash mismatch: expected {}, found {}",
            expected_parent_hash,
            parent_header.hash()
        );
    }

    insert_header_to_cache(parent_header.header().clone());

    Ok(Some(RestoredBenchmark { init, parent_header, parent_snapshot }))
}

pub fn persist_post_setup_cache(
    config: &BenchConfig,
    init: &InitResult,
    parent_header: &SealedHeader,
    parent_snapshot: &Snapshot,
) -> eyre::Result<()> {
    if !config.wants_post_setup_cache() {
        return Ok(());
    }

    let metadata = CacheMetadata::post_setup(
        parent_header.number,
        parent_header.hash(),
        parent_snapshot.clone(),
    );

    cache::persist_cache(
        config,
        CacheKind::PostSetup,
        &init.source_genesis,
        &init.temp_dir,
        &metadata,
    )
}

fn open_existing_runtime(
    config: &BenchConfig,
    source_genesis: String,
    temp_dir: PathBuf,
) -> eyre::Result<InitResult> {
    let genesis = cache::read_materialized_genesis(&temp_dir)?;
    let funded_accounts = generate_funded_accounts(config.funded_accounts);
    open_runtime(config, source_genesis, temp_dir, genesis, funded_accounts, false)
}

fn open_runtime(
    config: &BenchConfig,
    source_genesis: String,
    temp_dir: PathBuf,
    genesis: Genesis,
    funded_accounts: Vec<(B256, Address)>,
    initialize_genesis_db: bool,
) -> eyre::Result<InitResult> {
    let chain_spec = create_chain_spec(genesis);
    let validator_addresses: Vec<Address> =
        config.private_keys.iter().map(address_from_private_key).collect();

    println!("Validators:");
    for (i, addr) in validator_addresses.iter().enumerate() {
        println!("  [{}] {}", i, addr);
    }

    let db_path = temp_dir.join("db");
    let static_files_path = temp_dir.join("static_files");
    let rocksdb_path = temp_dir.join("rocksdb");
    let snap_db_path = temp_dir.join("snap_db");

    std::fs::create_dir_all(&db_path)
        .with_context(|| format!("failed to create {}", db_path.display()))?;
    std::fs::create_dir_all(&static_files_path)
        .with_context(|| format!("failed to create {}", static_files_path.display()))?;
    std::fs::create_dir_all(&snap_db_path)
        .with_context(|| format!("failed to create {}", snap_db_path.display()))?;

    if config.triedb {
        let triedb_path = temp_dir.join("rust_eth_triedb");
        std::fs::create_dir_all(&triedb_path)
            .with_context(|| format!("failed to create {}", triedb_path.display()))?;
        let triedb_path_str = triedb_path.to_string_lossy();
        init_global_triedb_manager(triedb_path_str.as_ref());
    }

    let db = Arc::new(
        init_db(&db_path, DatabaseArguments::new(Default::default()))
            .map_err(|e| eyre::eyre!("Failed to open main database: {}", e))?,
    );

    let static_file_provider = StaticFileProvider::read_write(&static_files_path)
        .map_err(|e| eyre::eyre!("Failed to create static file provider: {}", e))?;

    let rocksdb_provider = reth_provider::providers::RocksDBProvider::new(rocksdb_path)
        .map_err(|e| eyre::eyre!("Failed to create RocksDB provider: {}", e))?;
    let factory = ProviderFactory::<BscNodeTypes>::new(
        db.clone(),
        chain_spec.clone(),
        static_file_provider,
        rocksdb_provider,
    )
    .map_err(|e| eyre::eyre!("Failed to create ProviderFactory: {}", e))?;

    if initialize_genesis_db {
        let genesis_hash = init_genesis(&factory)
            .map_err(|e| eyre::eyre!("Failed to initialize genesis: {}", e))?;
        println!("Genesis initialized in MDBX, hash: {}", genesis_hash);
    }

    let snapshot_db = Arc::new(
        init_db(&snap_db_path, DatabaseArguments::new(Default::default()))
            .map_err(|e| eyre::eyre!("Failed to create snapshot database: {}", e))?,
    );

    let parlia = Arc::new(Parlia::new(chain_spec.clone(), 200));

    let genesis_header_ref = chain_spec.inner.genesis_header();
    let genesis_sealed_hash = genesis_header_ref.hash_slow();
    let genesis_sealed = SealedHeader::new(genesis_header_ref.clone(), genesis_sealed_hash);

    insert_header_to_cache(genesis_header_ref.clone());

    let genesis_snapshot =
        create_genesis_snapshot(&parlia, genesis_header_ref, &validator_addresses);

    let snapshot_provider =
        Arc::new(EnhancedDbSnapshotProvider::new(snapshot_db.clone(), 2048, chain_spec.clone()));
    snapshot_provider.insert(genesis_snapshot.clone());

    let _ = crate::shared::set_snapshot_provider(
        snapshot_provider.clone() as Arc<dyn SnapshotProvider + Send + Sync>
    );
    let _ = crate::shared::set_header_provider(Arc::new(factory.clone()));

    let best_block_number = factory.best_block_number().unwrap_or_default();
    println!(
        "Database ready: {} funded accounts, best block {}",
        funded_accounts.len(),
        best_block_number
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
        source_genesis,
    })
}

fn read_source_genesis(config: &BenchConfig) -> eyre::Result<String> {
    std::fs::read_to_string(&config.genesis_path)
        .with_context(|| format!("failed to read genesis file {}", config.genesis_path.display()))
}

fn parse_source_genesis(source_genesis: &str) -> eyre::Result<Genesis> {
    serde_json::from_str(source_genesis).wrap_err("failed to parse genesis JSON")
}

fn inject_funded_accounts(genesis: &mut Genesis, funded_accounts: &[(B256, Address)]) {
    let balance = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18));
    for (_, addr) in funded_accounts {
        genesis
            .alloc
            .entry(*addr)
            .or_insert_with(|| alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }
}

fn inject_background_accounts(genesis: &mut Genesis, config: &BenchConfig) {
    if config.background_accounts == 0 {
        return;
    }

    println!(
        "  Generating {} background accounts with {} storage slots each...",
        config.background_accounts, config.storage_slots_per_account
    );

    let small_balance = U256::from(1u64) * U256::from(10u64).pow(U256::from(18));
    for i in 0..config.background_accounts {
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = 0xBB;
        let idx_bytes = (i as u64 + 1).to_be_bytes();
        key_bytes[24..32].copy_from_slice(&idx_bytes);
        let addr = address_from_private_key(&B256::from(key_bytes));

        let storage = if config.storage_slots_per_account > 0 {
            let mut map = std::collections::BTreeMap::new();
            for s in 0..config.storage_slots_per_account {
                let mut slot_bytes = [0u8; 32];
                slot_bytes[24..32].copy_from_slice(&(s as u64).to_be_bytes());
                let slot = B256::from(slot_bytes);
                let mut val_bytes = [0u8; 32];
                val_bytes[31] = 1;
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

/// Create a genesis snapshot for the validator set.
fn create_genesis_snapshot(
    parlia: &Parlia<BscChainSpec>,
    genesis_header: &Header,
    validators: &[Address],
) -> Snapshot {
    let epoch_length = parlia.epoch;
    let parsed = parlia.parse_validators_from_header(genesis_header, epoch_length);

    let (addrs, vote_addrs) = match parsed {
        Ok(info) => {
            let vote = info.vote_addrs;
            (info.consensus_addrs, vote)
        }
        Err(_) => {
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
        let mut key_bytes = [0u8; 32];
        key_bytes[0] = 0x01;
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
    let balance = U256::from(1_000_000u64) * U256::from(10u64).pow(U256::from(18));

    for (_, addr) in funded_accounts {
        alloc.insert(*addr, alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }

    for addr in validators {
        alloc.insert(*addr, alloy_genesis::GenesisAccount { balance, ..Default::default() });
    }

    alloc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_account_injection_is_deterministic() {
        let mut genesis = Genesis::default();
        let config = BenchConfig {
            genesis_path: PathBuf::from("testing/genesis_local.json"),
            private_keys: vec![],
            deployer_key: B256::ZERO,
            num_blocks: 1,
            txs_per_block: 1,
            funded_accounts: 0,
            background_accounts: 2,
            storage_slots_per_account: 1,
            chain_difflayers: false,
            triedb: false,
            output_csv: PathBuf::from("benchmark.csv"),
            label: "default".to_string(),
            cache_dir: None,
            reuse_genesis_db: false,
            reuse_post_setup_db: false,
        };

        inject_background_accounts(&mut genesis, &config);

        assert_eq!(genesis.alloc.len(), 2);
    }
}
