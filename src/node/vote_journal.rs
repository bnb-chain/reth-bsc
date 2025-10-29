use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

// no top-level B256 import; tests import it under cfg(test)

use crate::consensus::parlia::vote::{VoteData, VoteEnvelope};

/// Maximum number of recent vote entries to keep in memory and on disk.
const MAX_RECENT_ENTRIES: usize = 512;
/// Scope for malicious vote slashing checks (in blocks).
pub const MALICIOUS_VOTE_SLASH_SCOPE: u64 = 256;
/// Upper bound window for forward-looking rule checks.
pub const UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER: u64 = 11; // keep consistent with BSC

/// Simple LRU-like cache keyed by `target_number` with FIFO eviction.
#[derive(Default)]
struct VoteDataLru {
    capacity: usize,
    order: VecDeque<u64>,
    map: HashMap<u64, VoteData>,
}

impl VoteDataLru {
    fn new(capacity: usize) -> Self { Self { capacity, order: VecDeque::new(), map: HashMap::new() } }

    fn add(&mut self, key: u64, value: VoteData) {
        if !self.map.contains_key(&key) {
            self.order.push_back(key);
            if self.order.len() > self.capacity {
                if let Some(old) = self.order.pop_front() {
                    self.map.remove(&old);
                }
            }
        }
        self.map.insert(key, value);
    }

    fn contains(&self, key: u64) -> bool { self.map.contains_key(&key) }

    fn get(&self, key: u64) -> Option<VoteData> { self.map.get(&key).cloned() }

    // no additional methods
}

/// On-disk journal of produced votes with an in-memory cache for rule checks.
pub struct VoteJournal {
    path: PathBuf,
    lru: VoteDataLru,
}

impl VoteJournal {
    fn resolve_default_path() -> PathBuf {
        // Priority: BSC_VOTE_JOURNAL_DIR -> RETH_DATADIR -> ./voteJournal
        if let Ok(dir) = std::env::var("BSC_VOTE_JOURNAL_DIR") { return PathBuf::from(dir).join("votes.jsonl"); }
        if let Ok(reth_dir) = std::env::var("RETH_DATADIR") { return PathBuf::from(reth_dir).join("voteJournal").join("votes.jsonl"); }
        PathBuf::from("voteJournal").join("votes.jsonl")
    }

    fn ensure_parent_dir(path: &Path) {
        if let Some(parent) = path.parent() { let _ = fs::create_dir_all(parent); }
    }

    fn open_file_append(path: &Path) -> std::io::Result<File> {
        Self::ensure_parent_dir(path);
        OpenOptions::new().create(true).append(true).open(path)
    }

    fn open_file_read(path: &Path) -> std::io::Result<File> { OpenOptions::new().read(true).open(path) }

    fn load_from_disk(path: &Path) -> VoteDataLru {
        let mut lru = VoteDataLru::new(MAX_RECENT_ENTRIES);
        if let Ok(file) = Self::open_file_read(path) {
            let reader = BufReader::new(file);
            // Read all lines and keep the last MAX_RECENT_ENTRIES
            // Each line is expected to be a JSON-serialized VoteEnvelope
            let mut buf: Vec<VoteData> = Vec::with_capacity(MAX_RECENT_ENTRIES);
            for line in reader.lines().map_while(Result::ok) {
                if line.is_empty() { continue; }
                if let Ok(env) = serde_json::from_str::<VoteEnvelope>(&line) {
                    buf.push(env.data);
                    if buf.len() > MAX_RECENT_ENTRIES { buf.remove(0); }
                }
            }
            for vd in buf { lru.add(vd.target_number, vd); }
        }
        lru
    }

    fn new(path: PathBuf) -> Self {
        let lru = Self::load_from_disk(&path);
        Self { path, lru }
    }

    /// Check vote rules against the in-memory buffer.
    /// Returns true if the vote is allowed under rules, along with the provided source/target context.
    pub fn under_rules(&self, source_number: u64, target_number: u64) -> bool {
        // Rule 1: must not publish two distinct votes for the same height
        if self.lru.contains(target_number) { return false; }

        // Rule 2: must not vote within the span of its other votes
        // Backward window
        let mut block_number = source_number.saturating_add(1);
        if block_number + MALICIOUS_VOTE_SLASH_SCOPE < target_number {
            block_number = target_number.saturating_sub(MALICIOUS_VOTE_SLASH_SCOPE);
        }
        while block_number < target_number {
            if let Some(vd) = self.lru.get(block_number) {
                if vd.source_number > source_number { return false; }
            }
            block_number += 1;
        }
        // Forward window
        let mut bn = target_number + 1;
        let upper = target_number + UPPER_LIMIT_OF_VOTE_BLOCK_NUMBER;
        while bn <= upper {
            if let Some(vd) = self.lru.get(bn) {
                if vd.source_number < source_number { return false; }
            }
            bn += 1;
        }
        true
    }

    /// Append a vote to the journal and update the in-memory cache.
    pub fn write_vote(&mut self, env: &VoteEnvelope) -> std::io::Result<()> {
        // Allow memory-only mode via env toggle.
        let mem_only = std::env::var("BSC_VOTE_JOURNAL_MEMORY_ONLY")
            .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "True"))
            .unwrap_or(false);
        if !mem_only {
            let mut file = Self::open_file_append(&self.path)?;
            let line = serde_json::to_string(env).unwrap_or_else(|_| String::new());
            if !line.is_empty() {
                file.write_all(line.as_bytes())?;
                file.write_all(b"\n")?;
                file.flush()?;
            }
        }
        self.lru.add(env.data.target_number, env.data);
        Ok(())
    }
}

/// Global vote journal instance, initialized lazily on first use.
static GLOBAL_JOURNAL: LazyLock<Mutex<VoteJournal>> = LazyLock::new(|| {
    let path = VoteJournal::resolve_default_path();
    Mutex::new(VoteJournal::new(path))
});

/// Get a guard to the global vote journal.
pub fn global() -> std::sync::MutexGuard<'static, VoteJournal> { GLOBAL_JOURNAL.lock().expect("vote journal poisoned") }

/// Helper for external modules to check the rules via global journal.
pub fn under_rules(source_number: u64, target_number: u64) -> bool { global().under_rules(source_number, target_number) }

/// Helper for external modules to persist a signed vote via global journal.
pub fn persist_vote(env: &VoteEnvelope) -> Result<(), String> {
    global().write_vote(env).map_err(|e| format!("Failed to write vote journal: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::B256;

    fn tmp_path(name: &str) -> PathBuf {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{}_{}.jsonl", name, ts))
    }

    fn mk_env(src_n: u64, src_h: B256, tgt_n: u64, tgt_h: B256) -> VoteEnvelope {
        VoteEnvelope {
            vote_address: Default::default(),
            signature: Default::default(),
            data: VoteData { source_number: src_n, source_hash: src_h, target_number: tgt_n, target_hash: tgt_h },
        }
    }

    #[test]
    fn rule1_duplicate_height_disallowed() {
        let path = tmp_path("journal_rule1");
        let mut j = VoteJournal::new(path);
        let env = mk_env(90, B256::from([1u8; 32]), 100, B256::from([2u8; 32]));
        j.write_vote(&env).unwrap();
        assert!(!j.under_rules(95, 100));
    }

    #[test]
    fn rule2_backward_across_span_disallowed() {
        let path = tmp_path("journal_rule2_back");
        let mut j = VoteJournal::new(path);
        // Previous vote with target 125 and source 120
        let env = mk_env(120, B256::from([3u8; 32]), 125, B256::from([4u8; 32]));
        j.write_vote(&env).unwrap();
        // New vote spans across: source 110, target 130 should be invalid (sees prior at 125 with higher source)
        assert!(!j.under_rules(110, 130));
    }

    #[test]
    fn rule2_forward_within_span_disallowed() {
        let path = tmp_path("journal_rule2_forward");
        let mut j = VoteJournal::new(path);
        // Previous vote with target 110 and lower source 90
        let env = mk_env(90, B256::from([5u8; 32]), 110, B256::from([6u8; 32]));
        j.write_vote(&env).unwrap();
        // New vote source 100, target 105: forward window includes 106..116; contains 110 with vd.source=90 < 100 => invalid
        assert!(!j.under_rules(100, 105));
    }

    #[test]
    fn allowed_vote_when_no_conflict() {
        let path = tmp_path("journal_ok");
        let mut j = VoteJournal::new(path);
        // Old vote far behind
        let env = mk_env(80, B256::from([7u8; 32]), 90, B256::from([8u8; 32]));
        j.write_vote(&env).unwrap();
        assert!(j.under_rules(100, 110));
    }

    #[test]
    fn persistence_reloads_buffer() {
        let path = tmp_path("journal_persist");
        {
            let mut j = VoteJournal::new(path.clone());
            let env1 = mk_env(10, B256::from([9u8; 32]), 20, B256::from([10u8; 32]));
            let env2 = mk_env(30, B256::from([11u8; 32]), 40, B256::from([12u8; 32]));
            j.write_vote(&env1).unwrap();
            j.write_vote(&env2).unwrap();
        }
        // Reopen
        let j2 = VoteJournal::new(path);
        // Rule1: contains 40
        assert!(!j2.under_rules(35, 40));
        // Rule2 forward: with source 35 target 33, forward window includes 34..44; 40 present with vd.source=30 < 35 => invalid
        assert!(!j2.under_rules(35, 33));
    }
}
