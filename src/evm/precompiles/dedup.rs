//! Helpers for rejecting duplicate validator identities and signer keys.
//!
//! Ported from bnb-chain/bsc `core/vm/lightclient/v2` (`validatorDuplicateTracker`,
//! `isZeroBytes`). The Pasteur bridge precompiles (cometBFT light-block validation and
//! BLS signature verification) use these to enforce uniqueness at the precompile
//! boundary instead of relying on callers to sanitize inputs.

use alloy_primitives::hex;
use revm::precompile::PrecompileHalt;
use std::{borrow::Cow, collections::HashMap};

/// Tracks previously-seen values for a single validator field and reports the first
/// collision as a descriptive [`PrecompileHalt`].
///
/// When `ignore_zero` is set, empty or all-zero values are treated as "unset" and are
/// not considered duplicates — optional bridge fields (relayer address, BLS key) may be
/// omitted in the source validators or zero-filled by fixed-width decoding, and both
/// forms must be allowed to repeat.
pub(crate) struct DuplicateTracker {
    field: &'static str,
    seen: HashMap<Vec<u8>, usize>,
    ignore_zero: bool,
}

impl DuplicateTracker {
    /// Create a tracker for `field`, pre-sizing for `size` entries. `ignore_zero`
    /// controls whether unset (empty / all-zero) values are exempt from the check.
    pub(crate) fn new(field: &'static str, size: usize, ignore_zero: bool) -> Self {
        Self { field, seen: HashMap::with_capacity(size), ignore_zero }
    }

    /// Record `value` at index `idx`, returning an error if the same value was already
    /// seen at an earlier index.
    pub(crate) fn check(&mut self, idx: usize, value: &[u8]) -> Result<(), PrecompileHalt> {
        if self.ignore_zero && (value.is_empty() || is_zero_bytes(value)) {
            return Ok(());
        }

        if let Some(&first_idx) = self.seen.get(value) {
            return Err(PrecompileHalt::Other(Cow::Owned(format!(
                "duplicate {} #{} and #{}: {}",
                self.field,
                first_idx,
                idx,
                hex::encode(value),
            ))));
        }
        self.seen.insert(value.to_vec(), idx);
        Ok(())
    }
}

/// Returns `true` only for a non-empty, all-zero byte slice.
///
/// Empty input returns `false`: it represents "no value" rather than a zero value. This
/// mirrors the upstream `isZeroBytes` semantics that the duplicate checks rely on.
pub(crate) fn is_zero_bytes(value: &[u8]) -> bool {
    !value.is_empty() && value.iter().all(|&b| b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_zero_bytes_requires_nonempty_input() {
        assert!(!is_zero_bytes(&[]));
        assert!(is_zero_bytes(&[0x00, 0x00]));
        assert!(!is_zero_bytes(&[0x00, 0x01]));
    }

    #[test]
    fn tracker_rejects_repeated_values() {
        let mut tracker = DuplicateTracker::new("validator address", 4, false);
        assert!(tracker.check(0, &[1, 2, 3]).is_ok());
        assert!(tracker.check(1, &[4, 5, 6]).is_ok());
        match tracker.check(2, &[1, 2, 3]).unwrap_err() {
            PrecompileHalt::Other(msg) => assert!(msg.contains("duplicate validator address")),
            other => panic!("unexpected halt variant: {other:?}"),
        }
    }

    #[test]
    fn tracker_ignore_zero_skips_unset_fields() {
        let mut tracker = DuplicateTracker::new("validator bls key", 4, true);
        // Repeated unset (empty / all-zero) values are not duplicates.
        assert!(tracker.check(0, &[]).is_ok());
        assert!(tracker.check(1, &[0, 0, 0]).is_ok());
        assert!(tracker.check(2, &[0, 0, 0]).is_ok());
        // Real repeats are still rejected.
        assert!(tracker.check(3, &[7, 7]).is_ok());
        assert!(tracker.check(4, &[7, 7]).is_err());
    }

    #[test]
    fn tracker_without_ignore_zero_treats_zero_as_value() {
        let mut tracker = DuplicateTracker::new("validator pubkey", 2, false);
        assert!(tracker.check(0, &[0, 0]).is_ok());
        assert!(tracker.check(1, &[0, 0]).is_err());
    }
}
