//! CLI tuning shared by live block fetching and active startup catch-up.

use super::fork_recover::{
    RecoveryGate, FETCH_TIMEOUT, FORK_RECOVER_HOP_COUNT, MAX_CONCURRENT_RECOVERIES, MAX_FORK_DEPTH,
    MAX_PEER_ATTEMPTS, RECOVERY_IDLE_TIMEOUT,
};
use clap::Args;
use std::{sync::Arc, time::Duration};

/// BSC block retrieval tuning. Defaults match the geth-style near-head request path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Args)]
pub struct BlockSyncArgs {
    /// Timeout for the single-block bsc/2 quick request.
    #[arg(long = "bsc.block-sync.quick-timeout", default_value = humantime::format_duration(Self::default().quick_timeout).to_string(), value_parser = parse_duration)]
    pub quick_timeout: Duration,

    /// Delay before starting independent ETH header/body fallback.
    #[arg(long = "bsc.block-sync.eth-fallback-delay", default_value = humantime::format_duration(Self::default().eth_fallback_delay).to_string(), value_parser = parse_duration)]
    pub eth_fallback_delay: Duration,

    /// Total deadline for a near-head block fetch.
    #[arg(long = "bsc.block-sync.head-timeout", default_value = humantime::format_duration(Self::default().head_timeout).to_string(), value_parser = parse_duration)]
    pub head_timeout: Duration,

    /// Maximum concurrent head downloads (1..=64).
    #[arg(long = "bsc.block-sync.max-head-fetches", default_value_t = Self::default().max_head_fetches, value_parser = clap::value_parser!(u16).range(1..=64))]
    pub max_head_fetches: u16,

    /// Maximum distinct ETH announcers queried per head (1..=16).
    #[arg(long = "bsc.block-sync.eth-peer-attempts", default_value_t = Self::default().eth_peer_attempts, value_parser = clap::value_parser!(u16).range(1..=16))]
    pub eth_peer_attempts: u16,

    /// Ancestor blocks per request, including startup catch-up (1..=64).
    #[arg(long = "bsc.block-sync.recovery-hop-count", default_value_t = Self::default().recovery_hop_count, value_parser = clap::value_parser!(u64).range(1..=64))]
    pub recovery_hop_count: u64,

    /// Per-request ancestor timeout; hedged peers have independent deadlines.
    #[arg(long = "bsc.block-sync.recovery-timeout", default_value = humantime::format_duration(Self::default().recovery_timeout).to_string(), value_parser = parse_duration)]
    pub recovery_timeout: Duration,

    /// Delay before requesting another eligible BSC peer during ancestor recovery.
    #[arg(long = "bsc.block-sync.recovery-fallback-delay", default_value = humantime::format_duration(Self::default().recovery_fallback_delay).to_string(), value_parser = parse_duration)]
    pub recovery_fallback_delay: Duration,

    /// Maximum BSC announcer attempts per ancestor hop (1..=16).
    #[arg(long = "bsc.block-sync.recovery-peer-attempts", default_value_t = Self::default().recovery_peer_attempts, value_parser = clap::value_parser!(u16).range(1..=16))]
    pub recovery_peer_attempts: u16,

    /// Shared concurrent startup and announcement recovery slots (1..=8).
    #[arg(long = "bsc.block-sync.max-recoveries", default_value_t = Self::default().max_recoveries, value_parser = clap::value_parser!(u16).range(1..=8))]
    pub max_recoveries: u16,

    /// Ancestor walk limit and pipeline handoff distance (1..=8192).
    #[arg(long = "bsc.block-sync.max-depth", default_value_t = Self::default().max_depth, value_parser = clap::value_parser!(u64).range(1..=8192))]
    pub max_depth: u64,

    /// Reclaim a recovery slot after this long without completed progress.
    #[arg(long = "bsc.block-sync.recovery-idle-timeout", default_value = humantime::format_duration(Self::default().recovery_idle_timeout).to_string(), value_parser = parse_duration)]
    pub recovery_idle_timeout: Duration,

    /// Maximum forward distance using single-block fetching (1..=8192).
    #[arg(long = "bsc.block-sync.near-head-distance", default_value_t = Self::default().near_head_distance, value_parser = clap::value_parser!(u64).range(1..=8192))]
    pub near_head_distance: u64,

    /// Maximum backward distance admitting announced heads (1..=8192).
    #[arg(long = "bsc.block-sync.stale-distance", default_value_t = Self::default().stale_distance, value_parser = clap::value_parser!(u64).range(1..=8192))]
    pub stale_distance: u64,
}

impl Default for BlockSyncArgs {
    fn default() -> Self {
        Self {
            quick_timeout: Duration::from_millis(400),
            eth_fallback_delay: Duration::from_millis(200),
            head_timeout: Duration::from_secs(5),
            max_head_fetches: 32,
            eth_peer_attempts: 3,
            recovery_hop_count: FORK_RECOVER_HOP_COUNT,
            recovery_timeout: FETCH_TIMEOUT,
            recovery_fallback_delay: Duration::from_millis(200),
            recovery_peer_attempts: MAX_PEER_ATTEMPTS as u16,
            max_recoveries: MAX_CONCURRENT_RECOVERIES as u16,
            max_depth: MAX_FORK_DEPTH,
            recovery_idle_timeout: RECOVERY_IDLE_TIMEOUT,
            near_head_distance: 32,
            stale_distance: 11,
        }
    }
}

/// Immutable per-node tuning and shared recovery admission; no process-global overrides.
#[derive(Debug)]
pub struct BlockSyncConfig {
    /// Validated retrieval limits and deadlines.
    pub args: BlockSyncArgs,
    pub(crate) recovery_gate: RecoveryGate,
    pub(crate) downloader: super::head_fetch::BlockDownloader,
}

impl Default for BlockSyncConfig {
    fn default() -> Self {
        let args = BlockSyncArgs::default();
        Self {
            recovery_gate: RecoveryGate::new(
                usize::from(args.max_recoveries),
                args.recovery_idle_timeout,
            ),
            args,
            downloader: Default::default(),
        }
    }
}

impl BlockSyncArgs {
    /// Validate cross-field limits before constructing the shared per-node recovery gate.
    pub fn into_config(self) -> Result<Arc<BlockSyncConfig>, BlockSyncConfigError> {
        for duration in [
            self.quick_timeout,
            self.eth_fallback_delay,
            self.head_timeout,
            self.recovery_timeout,
            self.recovery_fallback_delay,
            self.recovery_idle_timeout,
        ] {
            validate_duration(duration)?;
        }
        for (name, value, max) in [
            ("max-head-fetches", u64::from(self.max_head_fetches), 64),
            ("eth-peer-attempts", u64::from(self.eth_peer_attempts), 16),
            ("recovery-hop-count", self.recovery_hop_count, 64),
            ("recovery-peer-attempts", u64::from(self.recovery_peer_attempts), 16),
            ("max-recoveries", u64::from(self.max_recoveries), 8),
            ("max-depth", self.max_depth, 8192),
            ("near-head-distance", self.near_head_distance, 8192),
            ("stale-distance", self.stale_distance, 8192),
        ] {
            if value == 0 || value > max {
                return Err(BlockSyncConfigError::Limit { name, value, max });
            }
        }
        if self.eth_fallback_delay >= self.head_timeout {
            return Err(BlockSyncConfigError::FallbackDelay);
        }
        if self.quick_timeout > self.head_timeout {
            return Err(BlockSyncConfigError::QuickTimeout);
        }
        if self.near_head_distance > self.max_depth {
            return Err(BlockSyncConfigError::HeadDistance);
        }
        Ok(Arc::new(BlockSyncConfig {
            recovery_gate: RecoveryGate::new(
                usize::from(self.max_recoveries),
                self.recovery_idle_timeout,
            ),
            args: self,
            downloader: Default::default(),
        }))
    }
}

/// Invalid block synchronization duration or cross-field configuration.
#[derive(Debug, thiserror::Error)]
pub enum BlockSyncConfigError {
    /// Duration syntax is invalid.
    #[error(transparent)]
    Duration(#[from] humantime::DurationError),
    /// Prevent zero timers and unsupported timer horizons.
    #[error("block-sync durations must be greater than zero and at most 24h")]
    DurationRange,
    /// Programmatic configuration must obey the same resource bounds as the CLI.
    #[error("{name} must be in 1..={max}, got {value}")]
    Limit { name: &'static str, value: u64, max: u64 },
    /// Fallback must have time to run.
    #[error("eth-fallback-delay must be less than head-timeout")]
    FallbackDelay,
    /// Quick requests must fit the total request lifetime.
    #[error("quick-timeout must not exceed head-timeout")]
    QuickTimeout,
    /// Keep near-head fetching inside the ancestor/pipeline handoff window.
    #[error("near-head-distance must not exceed max-depth")]
    HeadDistance,
}

fn parse_duration(value: &str) -> Result<Duration, BlockSyncConfigError> {
    let duration = humantime::parse_duration(value)?;
    validate_duration(duration)?;
    Ok(duration)
}

fn validate_duration(duration: Duration) -> Result<(), BlockSyncConfigError> {
    if duration.is_zero() || duration > Duration::from_secs(24 * 60 * 60) {
        return Err(BlockSyncConfigError::DurationRange);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        sync: BlockSyncArgs,
    }

    #[test]
    fn defaults_overrides_help_and_invalid_limits() {
        let defaults = Cli::try_parse_from(["test"]).unwrap().sync;
        assert_eq!(defaults, BlockSyncArgs::default());
        defaults.into_config().unwrap();
        let configured = Cli::try_parse_from([
            "test",
            "--bsc.block-sync.quick-timeout",
            "50ms",
            "--bsc.block-sync.eth-fallback-delay",
            "20ms",
            "--bsc.block-sync.head-timeout",
            "1s",
            "--bsc.block-sync.recovery-hop-count",
            "2",
            "--bsc.block-sync.max-depth",
            "64",
            "--bsc.block-sync.max-head-fetches",
            "64",
        ])
        .unwrap()
        .sync;
        assert_eq!(configured.quick_timeout, Duration::from_millis(50));
        assert_eq!(configured.eth_fallback_delay, Duration::from_millis(20));
        assert_eq!(configured.recovery_hop_count, 2);
        assert_eq!(configured.max_depth, 64);
        configured.into_config().unwrap();
        let help = Cli::command().render_long_help().to_string();
        for flag in [
            "quick-timeout",
            "eth-fallback-delay",
            "head-timeout",
            "max-head-fetches",
            "eth-peer-attempts",
            "recovery-hop-count",
            "recovery-timeout",
            "recovery-peer-attempts",
            "max-recoveries",
            "max-depth",
            "recovery-idle-timeout",
            "near-head-distance",
            "stale-distance",
        ] {
            assert!(help.contains(&format!("--bsc.block-sync.{flag}")));
            let invalid = Cli::try_parse_from(["test", &format!("--bsc.block-sync.{flag}"), "0"]);
            assert_eq!(invalid.err().unwrap().kind(), clap::error::ErrorKind::ValueValidation);
        }
        for (flag, value) in [
            ("recovery-hop-count", "65"),
            ("max-head-fetches", "65"),
            ("eth-peer-attempts", "17"),
            ("max-recoveries", "9"),
            ("quick-timeout", "25h"),
        ] {
            assert_eq!(
                Cli::try_parse_from(["test", &format!("--bsc.block-sync.{flag}"), value])
                    .err()
                    .unwrap()
                    .kind(),
                clap::error::ErrorKind::ValueValidation
            );
        }
        for (args, expected) in [
            (
                BlockSyncArgs { eth_fallback_delay: defaults.head_timeout, ..defaults },
                "eth-fallback-delay must be less than head-timeout",
            ),
            (
                BlockSyncArgs { quick_timeout: Duration::from_secs(6), ..defaults },
                "quick-timeout must not exceed head-timeout",
            ),
            (
                BlockSyncArgs { near_head_distance: 2049, ..defaults },
                "near-head-distance must not exceed max-depth",
            ),
        ] {
            assert_eq!(args.into_config().unwrap_err().to_string(), expected);
        }
    }

    #[test]
    fn every_override_reaches_runtime_and_programmatic_values_are_checked() {
        let parsed = Cli::try_parse_from([
            "test",
            "--bsc.block-sync.quick-timeout",
            "50ms",
            "--bsc.block-sync.eth-fallback-delay",
            "20ms",
            "--bsc.block-sync.head-timeout",
            "1s",
            "--bsc.block-sync.max-head-fetches",
            "48",
            "--bsc.block-sync.eth-peer-attempts",
            "2",
            "--bsc.block-sync.recovery-hop-count",
            "2",
            "--bsc.block-sync.recovery-timeout",
            "300ms",
            "--bsc.block-sync.recovery-fallback-delay",
            "75ms",
            "--bsc.block-sync.recovery-peer-attempts",
            "2",
            "--bsc.block-sync.max-recoveries",
            "1",
            "--bsc.block-sync.max-depth",
            "64",
            "--bsc.block-sync.recovery-idle-timeout",
            "7s",
            "--bsc.block-sync.near-head-distance",
            "16",
            "--bsc.block-sync.stale-distance",
            "3",
        ])
        .unwrap()
        .sync;
        let expected = BlockSyncArgs {
            quick_timeout: Duration::from_millis(50),
            eth_fallback_delay: Duration::from_millis(20),
            head_timeout: Duration::from_secs(1),
            max_head_fetches: 48,
            eth_peer_attempts: 2,
            recovery_hop_count: 2,
            recovery_timeout: Duration::from_millis(300),
            recovery_fallback_delay: Duration::from_millis(75),
            recovery_peer_attempts: 2,
            max_recoveries: 1,
            max_depth: 64,
            recovery_idle_timeout: Duration::from_secs(7),
            near_head_distance: 16,
            stale_distance: 3,
        };
        assert_eq!(parsed, expected);
        let config = parsed.into_config().unwrap();
        assert_eq!(config.args, expected);
        let permit = config.recovery_gate.try_acquire(1).unwrap();
        assert!(matches!(config.recovery_gate.try_acquire(2), Err(1)));
        drop(permit);
        assert!(config.recovery_gate.try_acquire(2).is_ok());
        let defaults = BlockSyncArgs::default();
        for args in [
            BlockSyncArgs { quick_timeout: Duration::ZERO, ..defaults },
            BlockSyncArgs { eth_fallback_delay: Duration::ZERO, ..defaults },
            BlockSyncArgs { head_timeout: Duration::ZERO, ..defaults },
            BlockSyncArgs { recovery_timeout: Duration::ZERO, ..defaults },
            BlockSyncArgs { recovery_fallback_delay: Duration::ZERO, ..defaults },
            BlockSyncArgs { recovery_idle_timeout: Duration::ZERO, ..defaults },
        ] {
            assert!(matches!(args.into_config(), Err(BlockSyncConfigError::DurationRange)));
        }
        for args in [
            BlockSyncArgs { max_head_fetches: 0, ..defaults },
            BlockSyncArgs { eth_peer_attempts: 0, ..defaults },
            BlockSyncArgs { recovery_hop_count: 0, ..defaults },
            BlockSyncArgs { recovery_peer_attempts: 0, ..defaults },
            BlockSyncArgs { max_recoveries: 0, ..defaults },
            BlockSyncArgs { max_depth: 0, ..defaults },
            BlockSyncArgs { near_head_distance: 0, ..defaults },
            BlockSyncArgs { stale_distance: 0, ..defaults },
            BlockSyncArgs { max_head_fetches: 65, ..defaults },
            BlockSyncArgs { recovery_hop_count: 65, ..defaults },
        ] {
            assert!(matches!(args.into_config(), Err(BlockSyncConfigError::Limit { .. })));
        }
    }
}
