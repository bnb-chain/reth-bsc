//! BEP-675 BidBlock system-transaction validation.
//!
//! A builder-proposed block carries a trailing region of *unsigned* system transactions that the
//! in-turn validator blind-signs. This module ports the validation rules from bnb-chain/bsc
//! `consensus/parlia/bid_block.go`: which trailing txs are signable, and the exact selector/order
//! the trailing region must have for the given header.
//!
//! These functions are pure (no consensus state): they classify and verify transactions and so are
//! parameterized on `(txs, header, parent)` rather than the not-yet-ported `DecodedBidBlock`. The
//! stateful Parlia methods (`prepare_for_bid_block`, assembly, `block_time_upper_check`, the
//! `sign_system_tx` wrapper) are a separate slice.

use crate::{
    consensus::parlia::util::is_breathe_block,
    system_contracts::{is_invoke_system_contract, VALIDATOR_CONTRACT},
};
use alloy_consensus::{Header, Transaction};
use alloy_primitives::U256;
use reth_ethereum_primitives::TransactionSigned;
use std::{fmt, vec::Vec};

/// `deposit(...)` on the Validator contract.
pub const DEPOSIT_SELECTOR: [u8; 4] = [0xf3, 0x40, 0xfa, 0x01];
/// `distributeFinalityReward(...)` on the Validator contract.
pub const DISTRIBUTE_FINALITY_REWARD_SELECTOR: [u8; 4] = [0x30, 0x0c, 0x35, 0x67];
/// `updateValidatorSetV2(...)` on the Validator contract.
pub const UPDATE_VALIDATOR_SET_V2_SELECTOR: [u8; 4] = [0x1e, 0x4c, 0x15, 0x24];

/// The only system-tx methods a validator will blind-sign for a BidBlock.
const SIGNABLE_SELECTORS: [[u8; 4]; 3] =
    [DEPOSIT_SELECTOR, DISTRIBUTE_FINALITY_REWARD_SELECTOR, UPDATE_VALIDATOR_SET_V2_SELECTOR];

/// Validator-set rewards are distributed every `finalityRewardInterval` blocks.
const FINALITY_REWARD_INTERVAL: u64 = 200;

/// One expected entry in the trailing system-tx region.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedSystemTxEntry {
    /// Method name (for diagnostics).
    pub method: &'static str,
    /// 4-byte function selector.
    pub selector: [u8; 4],
}

/// Error describing why a BidBlock's trailing system-tx region is invalid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BidBlockSystemTxError {
    /// A trailing unsigned tx (at `index`) is not on the signable whitelist.
    NotSignable { index: usize },
    /// A required system tx is missing.
    MissingSystemTx { method: &'static str },
    /// An unexpected extra system tx is present at `position`.
    UnexpectedExtraTx { position: usize },
    /// The system tx at `position` has the wrong selector (expected `method`).
    WrongSelector { method: &'static str, position: usize },
}

impl fmt::Display for BidBlockSystemTxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotSignable { index } => {
                write!(f, "unsigned system tx at position {index} is not on the signable whitelist")
            }
            Self::MissingSystemTx { method } => write!(f, "missing required system tx {method:?}"),
            Self::UnexpectedExtraTx { position } => {
                write!(f, "unexpected extra system tx at position {position}")
            }
            Self::WrongSelector { method, position } => {
                write!(f, "expected system tx {method:?} at position {position}")
            }
        }
    }
}

impl std::error::Error for BidBlockSystemTxError {}

/// Whether the signature is the all-zero "unsigned" placeholder.
fn is_zero_sig(tx: &TransactionSigned) -> bool {
    let sig = tx.signature();
    sig.r().is_zero() && sig.s().is_zero() && !sig.v()
}

/// Whether `data` begins with one of the signable method selectors.
fn has_signable_selector(data: &[u8]) -> bool {
    data.len() >= 4 && SIGNABLE_SELECTORS.iter().any(|sel| data.starts_with(sel))
}

/// Reports whether `tx` looks like an unsigned BidBlock system tx (does not recover the sender):
/// it targets a system contract, has zero gas price, and carries the all-zero signature.
pub fn is_unsigned_system_tx_candidate(tx: &TransactionSigned) -> bool {
    let Some(to) = tx.to() else { return false };
    if !is_invoke_system_contract(&to) {
        return false;
    }
    if tx.max_fee_per_gas() != 0 {
        return false;
    }
    is_zero_sig(tx)
}

/// Reports whether `tx` may be blind-signed for a BidBlock: an unsigned candidate targeting the
/// Validator contract with a signable selector.
pub fn is_signable_system_tx(tx: &TransactionSigned) -> bool {
    is_unsigned_system_tx_candidate(tx)
        && tx.to() == Some(VALIDATOR_CONTRACT)
        && has_signable_selector(tx.input())
}

/// The expected trailing system-tx order for an accepted BidBlock:
///
/// `deposit -> distributeFinalityReward (every 200 blocks) -> updateValidatorSetV2 (breathe block)`
pub fn expected_system_tx_shape(header: &Header, parent: &Header) -> Vec<ExpectedSystemTxEntry> {
    let mut shape = Vec::with_capacity(3);
    shape.push(ExpectedSystemTxEntry { method: "deposit", selector: DEPOSIT_SELECTOR });

    if header.number.is_multiple_of(FINALITY_REWARD_INTERVAL) {
        shape.push(ExpectedSystemTxEntry {
            method: "distributeFinalityReward",
            selector: DISTRIBUTE_FINALITY_REWARD_SELECTOR,
        });
    }

    if is_breathe_block(parent.timestamp, header.timestamp) {
        shape.push(ExpectedSystemTxEntry {
            method: "updateValidatorSetV2",
            selector: UPDATE_VALIDATOR_SET_V2_SELECTOR,
        });
    }

    shape
}

/// Verify that `txs` match `shape` exactly in count, selector and order.
pub fn verify_system_tx_shape(
    txs: &[TransactionSigned],
    shape: &[ExpectedSystemTxEntry],
) -> Result<(), BidBlockSystemTxError> {
    if txs.len() < shape.len() {
        return Err(BidBlockSystemTxError::MissingSystemTx { method: shape[txs.len()].method });
    }
    if txs.len() > shape.len() {
        return Err(BidBlockSystemTxError::UnexpectedExtraTx { position: shape.len() });
    }
    for (i, exp) in shape.iter().enumerate() {
        if !txs[i].input().starts_with(&exp.selector) {
            return Err(BidBlockSystemTxError::WrongSelector { method: exp.method, position: i });
        }
    }
    Ok(())
}

/// Locate the trailing unsigned system-tx region and return its start index plus the value of the
/// `deposit` tx (zero if absent).
///
/// `start == 0` means there are no preceding user txs to collect fees from, which is invalid by
/// design — the zero value makes admission reject it.
pub fn extract_bid_block_deposit_value(txs: &[TransactionSigned]) -> (usize, U256) {
    let mut system_tx_start = txs.len();
    for i in (0..txs.len()).rev() {
        if !is_unsigned_system_tx_candidate(&txs[i]) {
            break;
        }
        system_tx_start = i;
    }

    if system_tx_start > 0
        && system_tx_start < txs.len()
        && txs[system_tx_start].input().starts_with(&DEPOSIT_SELECTOR)
    {
        return (system_tx_start, txs[system_tx_start].value());
    }
    (system_tx_start, U256::ZERO)
}

/// Validate the trailing unsigned system-tx region starting at `system_tx_start`:
///
/// 1. every trailing unsigned tx must be on the signable whitelist, and
/// 2. selectors & order must match [`expected_system_tx_shape`] for this header.
pub fn verify_bid_block_system_txs(
    txs: &[TransactionSigned],
    header: &Header,
    parent: &Header,
    system_tx_start: usize,
) -> Result<(), BidBlockSystemTxError> {
    for (offset, tx) in txs[system_tx_start..].iter().enumerate() {
        if !is_signable_system_tx(tx) {
            return Err(BidBlockSystemTxError::NotSignable { index: system_tx_start + offset });
        }
    }
    let shape = expected_system_tx_shape(header, parent);
    verify_system_tx_shape(&txs[system_tx_start..], &shape)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_consensus::TxLegacy;
    use alloy_primitives::{Address, Bytes, Signature, TxKind};

    fn mk_header(number: u64, timestamp: u64) -> Header {
        Header { number, timestamp, ..Default::default() }
    }

    fn build_tx(
        to: Address,
        gas_price: u128,
        input: Vec<u8>,
        value: u64,
        sig: Signature,
    ) -> TransactionSigned {
        let tx = TxLegacy {
            chain_id: None,
            nonce: 0,
            gas_price,
            gas_limit: 1_000_000,
            to: TxKind::Call(to),
            value: U256::from(value),
            input: Bytes::from(input),
        };
        TransactionSigned::new_unhashed(tx.into(), sig)
    }

    /// Build a tx with the all-zero "unsigned" signature.
    fn unsigned_tx(to: Address, gas_price: u128, input: Vec<u8>, value: u64) -> TransactionSigned {
        build_tx(to, gas_price, input, value, Signature::new(U256::ZERO, U256::ZERO, false))
    }

    fn system_tx(selector: [u8; 4], value: u64) -> TransactionSigned {
        unsigned_tx(VALIDATOR_CONTRACT, 0, selector.to_vec(), value)
    }

    #[test]
    fn classifies_unsigned_system_tx_candidates() {
        // Unsigned, zero gas price, to a system contract.
        assert!(is_unsigned_system_tx_candidate(&system_tx(DEPOSIT_SELECTOR, 0)));
        // Non-zero gas price disqualifies.
        assert!(!is_unsigned_system_tx_candidate(&unsigned_tx(
            VALIDATOR_CONTRACT,
            1,
            DEPOSIT_SELECTOR.to_vec(),
            0
        )));
        // Non-system-contract target disqualifies.
        assert!(!is_unsigned_system_tx_candidate(&unsigned_tx(
            Address::repeat_byte(0x42),
            0,
            DEPOSIT_SELECTOR.to_vec(),
            0
        )));
        // A real signature disqualifies.
        let signed = build_tx(
            VALIDATOR_CONTRACT,
            0,
            DEPOSIT_SELECTOR.to_vec(),
            0,
            Signature::new(U256::from(1), U256::from(2), false),
        );
        assert!(!is_unsigned_system_tx_candidate(&signed));
    }

    #[test]
    fn signable_requires_validator_contract_and_whitelisted_selector() {
        assert!(is_signable_system_tx(&system_tx(DEPOSIT_SELECTOR, 0)));
        assert!(is_signable_system_tx(&system_tx(UPDATE_VALIDATOR_SET_V2_SELECTOR, 0)));
        // Whitelisted selector but wrong (non-Validator) system contract.
        assert!(!is_signable_system_tx(&unsigned_tx(
            crate::system_contracts::SLASH_CONTRACT,
            0,
            DEPOSIT_SELECTOR.to_vec(),
            0
        )));
        // Validator contract but an unknown selector.
        assert!(!is_signable_system_tx(&system_tx([0xde, 0xad, 0xbe, 0xef], 0)));
    }

    #[test]
    fn expected_shape_depends_on_header() {
        // Plain block: just the deposit.
        let shape = expected_system_tx_shape(&mk_header(101, 1_000), &mk_header(100, 1_000));
        assert_eq!(shape.len(), 1);
        assert_eq!(shape[0].selector, DEPOSIT_SELECTOR);

        // Finality-reward block (number % 200 == 0): deposit + distributeFinalityReward.
        let shape = expected_system_tx_shape(&mk_header(200, 1_000), &mk_header(199, 1_000));
        assert_eq!(shape.len(), 2);
        assert_eq!(shape[1].selector, DISTRIBUTE_FINALITY_REWARD_SELECTOR);

        // Breathe block (UTC day rollover): deposit + updateValidatorSetV2.
        // 86_400 = 1 day; crossing it changes the UTC day.
        let shape = expected_system_tx_shape(&mk_header(101, 86_401), &mk_header(100, 86_399));
        assert_eq!(shape.len(), 2);
        assert_eq!(shape[1].selector, UPDATE_VALIDATOR_SET_V2_SELECTOR);
    }

    #[test]
    fn verify_shape_checks_count_and_order() {
        let shape = vec![
            ExpectedSystemTxEntry { method: "deposit", selector: DEPOSIT_SELECTOR },
            ExpectedSystemTxEntry {
                method: "distributeFinalityReward",
                selector: DISTRIBUTE_FINALITY_REWARD_SELECTOR,
            },
        ];

        let ok = [system_tx(DEPOSIT_SELECTOR, 0), system_tx(DISTRIBUTE_FINALITY_REWARD_SELECTOR, 0)];
        assert_eq!(verify_system_tx_shape(&ok, &shape), Ok(()));

        // Missing the trailing tx.
        assert_eq!(
            verify_system_tx_shape(&ok[..1], &shape),
            Err(BidBlockSystemTxError::MissingSystemTx { method: "distributeFinalityReward" })
        );

        // Extra trailing tx.
        let extra = [
            system_tx(DEPOSIT_SELECTOR, 0),
            system_tx(DISTRIBUTE_FINALITY_REWARD_SELECTOR, 0),
            system_tx(UPDATE_VALIDATOR_SET_V2_SELECTOR, 0),
        ];
        assert_eq!(
            verify_system_tx_shape(&extra, &shape),
            Err(BidBlockSystemTxError::UnexpectedExtraTx { position: 2 })
        );

        // Wrong selector / order.
        let swapped =
            [system_tx(DISTRIBUTE_FINALITY_REWARD_SELECTOR, 0), system_tx(DEPOSIT_SELECTOR, 0)];
        assert_eq!(
            verify_system_tx_shape(&swapped, &shape),
            Err(BidBlockSystemTxError::WrongSelector { method: "deposit", position: 0 })
        );
    }

    #[test]
    fn extract_deposit_value_finds_trailing_region() {
        let user_tx = unsigned_tx(Address::repeat_byte(0x11), 1, vec![], 0); // not a candidate
        let txs = vec![user_tx, system_tx(DEPOSIT_SELECTOR, 5_000)];
        let (start, value) = extract_bid_block_deposit_value(&txs);
        assert_eq!(start, 1);
        assert_eq!(value, U256::from(5_000));
    }

    #[test]
    fn extract_deposit_value_zero_when_no_user_txs() {
        // All-system-tx block: start == 0 is invalid by design, value reported as zero.
        let txs = vec![system_tx(DEPOSIT_SELECTOR, 5_000)];
        let (start, value) = extract_bid_block_deposit_value(&txs);
        assert_eq!(start, 0);
        assert_eq!(value, U256::ZERO);
    }

    #[test]
    fn verify_bid_block_system_txs_end_to_end() {
        let user_tx = unsigned_tx(Address::repeat_byte(0x11), 1, vec![], 0);
        // number 200 => finality-reward block; same-day timestamps => not a breathe block.
        let header = mk_header(200, 1_000);
        let parent = mk_header(199, 1_000);

        let good = vec![
            user_tx.clone(),
            system_tx(DEPOSIT_SELECTOR, 1),
            system_tx(DISTRIBUTE_FINALITY_REWARD_SELECTOR, 0),
        ];
        assert_eq!(verify_bid_block_system_txs(&good, &header, &parent, 1), Ok(()));

        // A trailing tx with a non-signable selector is rejected.
        let bad = vec![user_tx, system_tx(DEPOSIT_SELECTOR, 1), system_tx([0xaa, 0xbb, 0xcc, 0xdd], 0)];
        assert_eq!(
            verify_bid_block_system_txs(&bad, &header, &parent, 1),
            Err(BidBlockSystemTxError::NotSignable { index: 2 })
        );
    }
}
