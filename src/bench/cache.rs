use crate::bench::config::BenchConfig;
use crate::consensus::parlia::snapshot::Snapshot;

use alloy_genesis::Genesis;
use alloy_primitives::{B256, Keccak256};
use eyre::Context;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const CACHE_LAYOUT_VERSION: u32 = 1;
const MATERIALIZED_GENESIS_FILENAME: &str = "materialized_genesis.cbor";
const CACHE_METADATA_FILENAME: &str = "bench_cache_metadata.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKind {
    Genesis,
    PostSetup,
}

impl CacheKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Genesis => "genesis",
            Self::PostSetup => "post_setup",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheMetadata {
    pub format_version: u32,
    pub kind: CacheKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_block_number: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_block_hash: Option<B256>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_snapshot: Option<Snapshot>,
}

impl CacheMetadata {
    pub fn genesis() -> Self {
        Self {
            format_version: CACHE_LAYOUT_VERSION,
            kind: CacheKind::Genesis,
            parent_block_number: None,
            parent_block_hash: None,
            parent_snapshot: None,
        }
    }

    pub fn post_setup(
        parent_block_number: u64,
        parent_block_hash: B256,
        parent_snapshot: Snapshot,
    ) -> Self {
        Self {
            format_version: CACHE_LAYOUT_VERSION,
            kind: CacheKind::PostSetup,
            parent_block_number: Some(parent_block_number),
            parent_block_hash: Some(parent_block_hash),
            parent_snapshot: Some(parent_snapshot),
        }
    }

    pub fn validate(&self, expected_kind: CacheKind) -> eyre::Result<()> {
        if self.format_version != CACHE_LAYOUT_VERSION {
            eyre::bail!(
                "unsupported cache format version {}, expected {}",
                self.format_version,
                CACHE_LAYOUT_VERSION
            );
        }
        if self.kind != expected_kind {
            eyre::bail!("cache kind mismatch: found {:?}, expected {:?}", self.kind, expected_kind);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct RestoredCache {
    pub work_dir: PathBuf,
    pub metadata: CacheMetadata,
}

pub fn state_cache_key(config: &BenchConfig, source_genesis_json: &str) -> String {
    let mut hasher = Keccak256::new();
    hasher.update(CACHE_LAYOUT_VERSION.to_be_bytes());
    hasher.update(source_genesis_json.as_bytes());
    hasher.update(config.funded_accounts.to_be_bytes());
    hasher.update(config.background_accounts.to_be_bytes());
    hasher.update(config.storage_slots_per_account.to_be_bytes());
    hasher.update([u8::from(config.triedb)]);
    hasher.update(&config.deployer_key[..]);
    for key in &config.private_keys {
        hasher.update(&key[..]);
    }
    hex::encode(hasher.finalize())
}

pub fn create_work_dir() -> eyre::Result<PathBuf> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .wrap_err("system time is before unix epoch")?
        .as_nanos();
    let work_dir =
        std::env::temp_dir().join(format!("miner_bench_{}_{}", std::process::id(), unique));
    if work_dir.exists() {
        fs::remove_dir_all(&work_dir)
            .with_context(|| format!("failed to remove stale work dir {}", work_dir.display()))?;
    }
    fs::create_dir_all(&work_dir)
        .with_context(|| format!("failed to create work dir {}", work_dir.display()))?;
    Ok(work_dir)
}

pub fn materialized_genesis_path(base_dir: &Path) -> PathBuf {
    base_dir.join(MATERIALIZED_GENESIS_FILENAME)
}

pub fn metadata_path(base_dir: &Path) -> PathBuf {
    base_dir.join(CACHE_METADATA_FILENAME)
}

pub fn write_materialized_genesis(base_dir: &Path, genesis: &Genesis) -> eyre::Result<()> {
    let bytes =
        serde_cbor::to_vec(genesis).wrap_err("failed to encode materialized genesis as CBOR")?;
    fs::write(materialized_genesis_path(base_dir), bytes).with_context(|| {
        format!(
            "failed to write materialized genesis to {}",
            materialized_genesis_path(base_dir).display()
        )
    })?;
    Ok(())
}

pub fn read_materialized_genesis(base_dir: &Path) -> eyre::Result<Genesis> {
    let path = materialized_genesis_path(base_dir);
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read materialized genesis {}", path.display()))?;
    serde_cbor::from_slice(&bytes)
        .with_context(|| format!("failed to decode materialized genesis {}", path.display()))
}

pub fn persist_cache(
    config: &BenchConfig,
    kind: CacheKind,
    source_genesis_json: &str,
    source_dir: &Path,
    metadata: &CacheMetadata,
) -> eyre::Result<()> {
    let Some(target_dir) = cache_entry_dir(config, kind, source_genesis_json) else {
        return Ok(());
    };
    let parent = target_dir
        .parent()
        .ok_or_else(|| eyre::eyre!("cache entry {} has no parent", target_dir.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("failed to create cache parent {}", parent.display()))?;

    let staging_dir = target_dir.with_extension("tmp");
    if staging_dir.exists() {
        fs::remove_dir_all(&staging_dir).with_context(|| {
            format!("failed to remove cache staging dir {}", staging_dir.display())
        })?;
    }

    copy_dir_recursive(source_dir, &staging_dir)?;
    write_metadata(&staging_dir, metadata)?;

    if target_dir.exists() {
        fs::remove_dir_all(&target_dir)
            .with_context(|| format!("failed to replace cache dir {}", target_dir.display()))?;
    }
    fs::rename(&staging_dir, &target_dir).with_context(|| {
        format!(
            "failed to move cache staging dir {} into place at {}",
            staging_dir.display(),
            target_dir.display()
        )
    })?;

    Ok(())
}

pub fn try_restore_cache(
    config: &BenchConfig,
    kind: CacheKind,
    source_genesis_json: &str,
) -> eyre::Result<Option<RestoredCache>> {
    let Some(cache_dir) = cache_entry_dir(config, kind, source_genesis_json) else {
        return Ok(None);
    };
    if !cache_dir.exists() {
        return Ok(None);
    }

    let work_dir = create_work_dir()?;
    copy_dir_recursive(&cache_dir, &work_dir)?;

    let metadata = read_metadata(&work_dir)?;
    metadata.validate(kind)?;

    Ok(Some(RestoredCache { work_dir, metadata }))
}

fn cache_entry_dir(
    config: &BenchConfig,
    kind: CacheKind,
    source_genesis_json: &str,
) -> Option<PathBuf> {
    let cache_dir = config.cache_dir.as_ref()?;
    Some(
        cache_dir
            .join(format!("v{}", CACHE_LAYOUT_VERSION))
            .join(kind.as_str())
            .join(state_cache_key(config, source_genesis_json)),
    )
}

fn write_metadata(base_dir: &Path, metadata: &CacheMetadata) -> eyre::Result<()> {
    let bytes = serde_json::to_vec_pretty(metadata).wrap_err("failed to encode cache metadata")?;
    let path = metadata_path(base_dir);
    fs::write(&path, bytes)
        .with_context(|| format!("failed to write cache metadata {}", path.display()))?;
    Ok(())
}

fn read_metadata(base_dir: &Path) -> eyre::Result<CacheMetadata> {
    let path = metadata_path(base_dir);
    let bytes = fs::read(&path)
        .with_context(|| format!("failed to read cache metadata {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode cache metadata {}", path.display()))
}

fn copy_dir_recursive(source: &Path, destination: &Path) -> eyre::Result<()> {
    fs::create_dir_all(destination)
        .with_context(|| format!("failed to create {}", destination.display()))?;

    for entry in
        fs::read_dir(source).with_context(|| format!("failed to read {}", source.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to read entry in {}", source.display()))?;
        let file_type = entry.file_type().with_context(|| {
            format!("failed to determine file type for {}", entry.path().display())
        })?;
        let src_path = entry.path();
        let dst_path = destination.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else if file_type.is_file() {
            fs::copy(&src_path, &dst_path).with_context(|| {
                format!("failed to copy {} to {}", src_path.display(), dst_path.display())
            })?;
        } else {
            eyre::bail!("unsupported cache entry type at {}", src_path.display());
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::Address;

    fn config() -> BenchConfig {
        BenchConfig {
            genesis_path: PathBuf::from("testing/genesis_local.json"),
            private_keys: vec![B256::repeat_byte(0x11)],
            deployer_key: B256::repeat_byte(0x22),
            num_blocks: 100,
            txs_per_block: 6000,
            funded_accounts: 5_000,
            background_accounts: 1_000_000,
            storage_slots_per_account: 1,
            chain_difflayers: false,
            triedb: true,
            output_csv: PathBuf::from("benchmark.csv"),
            label: "default".to_string(),
            cache_dir: Some(PathBuf::from("/tmp/bench-cache")),
            reuse_genesis_db: true,
            reuse_post_setup_db: false,
        }
    }

    #[test]
    fn metadata_round_trip_preserves_post_setup_state() {
        let temp_dir = create_work_dir().expect("temp dir");
        let metadata = CacheMetadata::post_setup(
            1,
            B256::repeat_byte(0xAB),
            Snapshot::new(vec![Address::repeat_byte(0x11)], 1, B256::repeat_byte(0xCD), 200, None),
        );

        write_metadata(&temp_dir, &metadata).expect("write metadata");
        let restored = read_metadata(&temp_dir).expect("read metadata");

        assert_eq!(restored, metadata);
    }

    #[test]
    fn copy_dir_recursive_preserves_nested_files() {
        let src = create_work_dir().expect("source dir");
        let dst = create_work_dir().expect("destination dir");
        let nested = src.join("nested");
        fs::create_dir_all(&nested).expect("create nested dir");
        fs::write(src.join("root.txt"), b"root").expect("write root file");
        fs::write(nested.join("inner.txt"), b"inner").expect("write nested file");

        copy_dir_recursive(&src, &dst).expect("copy recursive");

        assert_eq!(fs::read(dst.join("root.txt")).expect("read root copy"), b"root");
        assert_eq!(
            fs::read(dst.join("nested").join("inner.txt")).expect("read nested copy"),
            b"inner"
        );
    }

    #[test]
    fn state_cache_key_changes_when_triedb_changes() {
        let baseline = config();
        let changed = BenchConfig { triedb: false, ..baseline.clone() };
        let genesis_json = "{\"alloc\":{},\"config\":{},\"gasLimit\":\"0x1\",\"difficulty\":\"0x1\"}";

        assert_ne!(state_cache_key(&baseline, genesis_json), state_cache_key(&changed, genesis_json));
    }
}
