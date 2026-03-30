/// Timing result returned from the background commit thread.
#[derive(Debug, Clone)]
pub struct CommitResult {
    pub block_number: u64,
    pub insert_block_us: u128,
    pub write_state_us: u128,
    pub triedb_flush_us: u128,
    pub provider_commit_us: u128,
    pub commit_us: u128,
}
