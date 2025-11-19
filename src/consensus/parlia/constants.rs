use alloy_primitives::{b256, B256, U256};

/// Fixed 32-byte vanity prefix present in every header.
pub const EXTRA_VANITY_LEN: usize = 32;
/// Fixed 65-byte ECDSA signature suffix (r,s,v).
pub const EXTRA_SEAL_LEN: usize = 65;
/// 1-byte length field preceding validator bytes since Luban.
pub const VALIDATOR_NUMBER_SIZE: usize = 1;
/// Size of each validator address (20 bytes) before Luban.
pub const VALIDATOR_BYTES_LEN_BEFORE_LUBAN: usize = 20;
/// Size of each validator consensus address (20) + vote address (48) after Luban.
pub const VALIDATOR_BYTES_LEN_AFTER_LUBAN: usize = 68;
/// 1-byte turnLength suffix added in Bohr.
pub const TURN_LENGTH_SIZE: usize = 1;

/// Difficulty for in-turn block (when it's the proposer's turn)
pub const DIFF_INTURN: U256 = U256::from_limbs([2, 0, 0, 0]);
/// Difficulty for out-of-turn block (when it's not the proposer's turn)
pub const DIFF_NOTURN: U256 = U256::from_limbs([1, 0, 0, 0]); 

pub const COLLECT_ADDITIONAL_VOTES_REWARD_RATIO: usize = 100;

pub const BACKOFF_TIME_OF_INITIAL: u64 = 1000; // milliseconds
pub const LORENTZ_BACKOFF_TIME_OF_INITIAL: u64 = 2000; // milliseconds
pub const DEFAULT_TURN_LENGTH: u8 = 1;
pub const BACKOFF_TIME_OF_WIGGLE: u64 = 1000; // milliseconds

// system txs gas limit
pub const SYSTEM_TXS_GAS_HARD_LIMIT: u64 = 20_000_000; // Maximum gas reserved for system transactions (Parlia consensus only)
pub const SYSTEM_TXS_GAS_SOFT_LIMIT: u64 = 1_000_000; // Maximum gas reserved for system transactions, excluding validator update transactions (Parlia consensus only)

// Ramanujan HF constants
pub const FIXED_BACKOFF_TIME_BEFORE_FORK_MILLIS: u64 = 200; // 200 ms
pub const WIGGLE_TIME_BEFORE_FORK_MILLIS: u64 = 500; // 500 ms
pub const MILLISECONDS_UNIT: u64 = 50; // 50 ms
pub const K_ANCESTOR_GENERATION_DEPTH: u64 = 3;

// miner config default values
pub const DEFAULT_MIN_GAS_TIP: u128 = 50_000_000; // 0.05 Gwei

// FF reward distribution interval
pub const FF_REWARD_DISTRIBUTION_INTERVAL: u64 = 200;

// EmptyWithdrawalsHash is the known hash of the empty withdrawal set.
pub const EMPTY_WITHDRAWALS_HASH: B256 = b256!("56e81f171bcc55a6ff8345e692c0f86e5b48e01b996cadc001622fb5e363b421");

// EmptyRequestsHash is the known hash of an empty request set, sha256("").
pub const EMPTY_REQUESTS_HASH: B256 = b256!("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");