//! Shared models for execution spec tests

use alloy_consensus::Header as RethHeader;
use alloy_eips::eip4895::Withdrawals;
use alloy_genesis::GenesisAccount;
use alloy_primitives::{keccak256, Address, Bloom, Bytes, B256, B64, U256};
use reth_bsc::{
    chainspec::BscChainSpec,
    hardforks::bsc::BscHardfork,
};
use reth_chainspec::{BaseFeeParams, BaseFeeParamsKind, Chain, ChainSpec, ForkCondition, Hardfork};
use reth_ethereum_forks::{ChainHardforks, EthereumHardfork};
use reth_db_api::{cursor::DbDupCursorRO, tables, transaction::DbTx};
use reth_primitives_traits::SealedHeader;
use serde::Deserialize;
use std::{collections::BTreeMap, ops::Deref, sync::Arc};

use crate::Error;

/// The definition of a blockchain test.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BlockchainTest {
    /// Genesis block header.
    pub genesis_block_header: Header,
    /// RLP encoded genesis block.
    #[serde(rename = "genesisRLP")]
    pub genesis_rlp: Option<Bytes>,
    /// Block data.
    pub blocks: Vec<Block>,
    /// The expected post state.
    pub post_state: Option<BTreeMap<Address, Account>>,
    /// The test pre-state.
    pub pre: State,
    /// Hash of the best block.
    pub lastblockhash: B256,
    /// Network spec.
    pub network: ForkSpec,
    #[serde(default)]
    /// Engine spec.
    pub seal_engine: SealEngine,
}

/// A block header in an Ethereum blockchain test.
#[derive(Debug, PartialEq, Eq, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    /// Bloom filter.
    pub bloom: Bloom,
    /// Coinbase.
    pub coinbase: Address,
    /// Difficulty.
    pub difficulty: U256,
    /// Extra data.
    pub extra_data: Bytes,
    /// Gas limit.
    pub gas_limit: U256,
    /// Gas used.
    pub gas_used: U256,
    /// Block Hash.
    pub hash: B256,
    /// Mix hash.
    pub mix_hash: B256,
    /// Seal nonce.
    pub nonce: B64,
    /// Block number.
    pub number: U256,
    /// Parent hash.
    pub parent_hash: B256,
    /// Receipt trie.
    pub receipt_trie: B256,
    /// State root.
    pub state_root: B256,
    /// Timestamp.
    pub timestamp: U256,
    /// Transactions trie.
    pub transactions_trie: B256,
    /// Uncle hash.
    pub uncle_hash: B256,
    /// Base fee per gas.
    pub base_fee_per_gas: Option<U256>,
    /// Withdrawals root.
    pub withdrawals_root: Option<B256>,
    /// Blob gas used.
    pub blob_gas_used: Option<U256>,
    /// Excess blob gas.
    pub excess_blob_gas: Option<U256>,
    /// Parent beacon block root.
    pub parent_beacon_block_root: Option<B256>,
    /// Requests root.
    pub requests_hash: Option<B256>,
    /// Target blobs per block.
    pub target_blobs_per_block: Option<U256>,
}

impl From<Header> for SealedHeader {
    fn from(value: Header) -> Self {
        let header = RethHeader {
            base_fee_per_gas: value.base_fee_per_gas.map(|v| v.to::<u64>()),
            beneficiary: value.coinbase,
            difficulty: value.difficulty,
            extra_data: value.extra_data,
            gas_limit: value.gas_limit.to::<u64>(),
            gas_used: value.gas_used.to::<u64>(),
            mix_hash: value.mix_hash,
            nonce: u64::from_be_bytes(value.nonce.0).into(),
            number: value.number.to::<u64>(),
            timestamp: value.timestamp.to::<u64>(),
            transactions_root: value.transactions_trie,
            receipts_root: value.receipt_trie,
            ommers_hash: value.uncle_hash,
            state_root: value.state_root,
            parent_hash: value.parent_hash,
            logs_bloom: value.bloom,
            withdrawals_root: value.withdrawals_root,
            blob_gas_used: value.blob_gas_used.map(|v| v.to::<u64>()),
            excess_blob_gas: value.excess_blob_gas.map(|v| v.to::<u64>()),
            parent_beacon_block_root: value.parent_beacon_block_root,
            requests_hash: value.requests_hash,
        };
        Self::new(header, value.hash)
    }
}

/// A block in an Ethereum blockchain test.
#[derive(Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    /// Block header.
    pub block_header: Option<Header>,
    /// RLP encoded block bytes
    pub rlp: Bytes,
    /// If the execution of the block should fail,
    /// `expect_exception` is `Some`.
    /// Its contents detail the reason for the failure.
    pub expect_exception: Option<String>,
    /// Transactions
    pub transactions: Option<Vec<Transaction>>,
    /// Uncle/ommer headers
    pub uncle_headers: Option<Vec<Header>>,
    /// Transaction Sequence
    pub transaction_sequence: Option<Vec<TransactionSequence>>,
    /// Withdrawals
    pub withdrawals: Option<Withdrawals>,
}

/// Transaction sequence in block
#[derive(Debug, PartialEq, Eq, Deserialize, Default)]
#[serde(deny_unknown_fields)]
#[serde(rename_all = "camelCase")]
pub struct TransactionSequence {
    exception: String,
    raw_bytes: Bytes,
    valid: String,
}

/// Ethereum blockchain test data state.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Default)]
pub struct State(BTreeMap<Address, Account>);

impl State {
    /// Return state as genesis state.
    pub fn into_genesis_state(self) -> BTreeMap<Address, GenesisAccount> {
        self.0
            .into_iter()
            .map(|(address, account)| {
                let storage = account
                    .storage
                    .iter()
                    .filter(|(_, v)| !v.is_zero())
                    .map(|(k, v)| {
                        (
                            B256::from_slice(&k.to_be_bytes::<32>()),
                            B256::from_slice(&v.to_be_bytes::<32>()),
                        )
                    })
                    .collect();
                let account = GenesisAccount {
                    balance: account.balance,
                    nonce: Some(account.nonce.try_into().unwrap()),
                    code: Some(account.code).filter(|c| !c.is_empty()),
                    storage: Some(storage),
                    private_key: None,
                };
                (address, account)
            })
            .collect::<BTreeMap<_, _>>()
    }
}

impl Deref for State {
    type Target = BTreeMap<Address, Account>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// An account.
#[derive(Debug, PartialEq, Eq, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Account {
    /// Balance.
    pub balance: U256,
    /// Code.
    pub code: Bytes,
    /// Nonce.
    pub nonce: U256,
    /// Storage.
    pub storage: BTreeMap<U256, U256>,
}

impl Account {
    /// Check that the account matches what is in the database.
    ///
    /// In case of a mismatch, `Err(Error::Assertion)` is returned.
    pub fn assert_db(&self, address: Address, tx: &impl DbTx) -> Result<(), Error> {
        let account =
            tx.get_by_encoded_key::<tables::PlainAccountState>(&address)?.ok_or_else(|| {
                Error::Assertion(format!(
                    "Expected account ({address}) is missing from DB: {self:?}"
                ))
            })?;

        assert_equal(self.balance, account.balance, "Balance does not match")?;
        assert_equal(self.nonce.to(), account.nonce, "Nonce does not match")?;

        if let Some(bytecode_hash) = account.bytecode_hash {
            assert_equal(keccak256(&self.code), bytecode_hash, "Bytecode does not match")?;
        } else {
            assert_equal(
                self.code.is_empty(),
                true,
                "Expected empty bytecode, got bytecode in db.",
            )?;
        }

        let mut storage_cursor = tx.cursor_dup_read::<tables::PlainStorageState>()?;
        for (slot, value) in &self.storage {
            if let Some(entry) =
                storage_cursor.seek_by_key_subkey(address, B256::new(slot.to_be_bytes()))?
            {
                if U256::from_be_bytes(entry.key.0) == *slot {
                    assert_equal(
                        *value,
                        entry.value,
                        &format!("Storage for slot {slot:?} does not match"),
                    )?;
                } else {
                    return Err(Error::Assertion(format!(
                        "Slot {slot:?} is missing from the database. Expected {value:?}"
                    )));
                }
            } else {
                return Err(Error::Assertion(format!(
                    "Slot {slot:?} is missing from the database. Expected {value:?}"
                )));
            }
        }

        Ok(())
    }
}

fn assert_equal<T: std::cmp::PartialEq + std::fmt::Debug>(
    got: T,
    expected: T,
    msg: &str,
) -> Result<(), Error> {
    if got != expected {
        Err(Error::Assertion(format!("{msg}: got {got:?}, expected {expected:?}")))
    } else {
        Ok(())
    }
}

/// Fork specification.
/// Includes both Ethereum and BSC hardforks.
#[derive(Debug, PartialEq, Eq, PartialOrd, Hash, Ord, Clone, Copy, Deserialize)]
pub enum ForkSpec {
    // Ethereum forks
    /// Frontier
    Frontier,
    /// Frontier to Homestead
    FrontierToHomesteadAt5,
    /// Homestead
    Homestead,
    /// Homestead to DAO
    HomesteadToDaoAt5,
    /// Homestead to Tangerine
    HomesteadToEIP150At5,
    /// Tangerine
    EIP150,
    /// Spurious Dragon
    EIP158,
    /// Spurious Dragon to Byzantium
    EIP158ToByzantiumAt5,
    /// Byzantium
    Byzantium,
    /// Byzantium to Constantinople
    ByzantiumToConstantinopleAt5,
    /// Byzantium to Constantinople Fix
    ByzantiumToConstantinopleFixAt5,
    /// Constantinople
    Constantinople,
    /// Constantinople fix
    ConstantinopleFix,
    /// Istanbul
    Istanbul,
    /// Berlin
    Berlin,
    /// Berlin to London
    BerlinToLondonAt5,
    /// London
    London,
    /// Paris aka The Merge
    #[serde(alias = "Paris")]
    Merge,
    /// Paris to Shanghai at time 15k
    ParisToShanghaiAtTime15k,
    /// Shanghai
    Shanghai,
    /// Shanghai to Cancun at time 15k
    ShanghaiToCancunAtTime15k,
    /// Merge EOF test
    #[serde(alias = "Merge+3540+3670")]
    MergeEOF,
    /// After Merge Init Code test
    #[serde(alias = "Merge+3860")]
    MergeMeterInitCode,
    /// After Merge plus new PUSH0 opcode
    #[serde(alias = "Merge+3855")]
    MergePush0,
    /// Cancun
    Cancun,
    /// Cancun to Prague at time 15k
    CancunToPragueAtTime15k,
    /// Prague
    Prague,
    /// Osaka
    Osaka,

    // BSC-specific forks
    /// BSC Ramanujan
    Ramanujan,
    /// BSC Niels
    Niels,
    /// BSC MirrorSync
    MirrorSync,
    /// BSC Bruno
    Bruno,
    /// BSC Euler
    Euler,
    /// BSC Nano
    Nano,
    /// BSC Moran
    Moran,
    /// BSC Gibbs
    Gibbs,
    /// BSC Planck
    Planck,
    /// BSC Luban
    Luban,
    /// BSC Plato
    Plato,
    /// BSC Hertz
    Hertz,
    /// BSC HertzFix
    HertzFix,
    /// BSC Kepler
    Kepler,
    /// BSC Feynman
    Feynman,
    /// BSC FeynmanFix
    FeynmanFix,
    /// BSC Haber
    Haber,
    /// BSC HaberFix
    HaberFix,
    /// BSC Bohr
    Bohr,
    /// BSC Pascal
    Pascal,
    /// BSC Lorentz
    Lorentz,
    /// BSC Maxwell
    Maxwell,
    /// BSC Fermi
    Fermi,
    /// BSC Mendel
    Mendel,
}

impl ForkSpec {
    /// Returns `true` if this is a BSC-specific fork.
    pub const fn is_bsc_fork(&self) -> bool {
        matches!(
            self,
            Self::Ramanujan
                | Self::Niels
                | Self::MirrorSync
                | Self::Bruno
                | Self::Euler
                | Self::Nano
                | Self::Moran
                | Self::Gibbs
                | Self::Planck
                | Self::Luban
                | Self::Plato
                | Self::Hertz
                | Self::HertzFix
                | Self::Kepler
                | Self::Feynman
                | Self::FeynmanFix
                | Self::Haber
                | Self::HaberFix
                | Self::Bohr
                | Self::Pascal
                | Self::Lorentz
                | Self::Maxwell
                | Self::Fermi
                | Self::Mendel
        )
    }
}

impl From<ForkSpec> for Arc<BscChainSpec> {
    fn from(fork_spec: ForkSpec) -> Self {
        // Create hardforks list based on the fork spec
        let hardforks = create_hardforks_for_spec(fork_spec);

        let inner = ChainSpec {
            chain: Chain::from_id(1), // Use Ethereum mainnet chain ID for EF tests
            genesis: Default::default(),
            hardforks,
            deposit_contract: None,
            base_fee_params: BaseFeeParamsKind::Constant(BaseFeeParams::new(1, 1)),
            prune_delete_limit: 3500,
            ..Default::default()
        };

        Arc::new(BscChainSpec { inner })
    }
}

/// Create the hardforks configuration for a given fork spec.
/// This ensures all prerequisite forks are enabled from genesis (block 0 / timestamp 0).
fn create_hardforks_for_spec(fork_spec: ForkSpec) -> ChainHardforks {
    // Build fork-specific hardfork lists to ensure correct precompile gas costs
    let mut forks: Vec<(Box<dyn reth_ethereum_forks::Hardfork>, ForkCondition)> = Vec::new();

    // Add forks based on the target spec
    match fork_spec {
        // Frontier
        ForkSpec::Frontier => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::FrontierToHomesteadAt5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(5)));
        }

        // Homestead
        ForkSpec::Homestead => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::HomesteadToDaoAt5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Dao.boxed(), ForkCondition::Block(5)));
        }
        ForkSpec::HomesteadToEIP150At5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(5)));
        }

        // Tangerine Whistle (EIP-150)
        ForkSpec::EIP150 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
        }

        // Spurious Dragon (EIP-158)
        ForkSpec::EIP158 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::EIP158ToByzantiumAt5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(5)));
        }

        // Byzantium
        ForkSpec::Byzantium => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::ByzantiumToConstantinopleAt5 | ForkSpec::ByzantiumToConstantinopleFixAt5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(5)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(5)));
        }

        // Constantinople
        ForkSpec::Constantinople | ForkSpec::ConstantinopleFix => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
        }

        // Istanbul
        ForkSpec::Istanbul => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
        }

        // Berlin/London era
        ForkSpec::Berlin => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::BerlinToLondonAt5 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(5)));
        }
        ForkSpec::London => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
        }

        // Post-merge (Paris)
        ForkSpec::Merge | ForkSpec::MergeEOF | ForkSpec::MergeMeterInitCode | ForkSpec::MergePush0 => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
        }

        // Shanghai
        ForkSpec::ParisToShanghaiAtTime15k => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(15_000)));
        }
        ForkSpec::Shanghai => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
        }

        // Cancun
        ForkSpec::ShanghaiToCancunAtTime15k => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(15_000)));
        }
        ForkSpec::Cancun => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
        }

        ForkSpec::CancunToPragueAtTime15k => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(15_000)));
        }

        ForkSpec::Prague => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)));
        }
        ForkSpec::Osaka => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Osaka.boxed(), ForkCondition::Timestamp(0)));
        }

        // BSC-specific forks - add all prerequisites including base Ethereum forks
        ForkSpec::Ramanujan | ForkSpec::Niels => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::MirrorSync => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Bruno => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Euler => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Nano | ForkSpec::Moran | ForkSpec::Gibbs => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Planck => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Luban => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Plato => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Plato.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Hertz | ForkSpec::HertzFix => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Plato.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Hertz.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::HertzFix.boxed(), ForkCondition::Block(0)));
        }
        ForkSpec::Kepler | ForkSpec::Feynman | ForkSpec::FeynmanFix => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Plato.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Hertz.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::HertzFix.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Kepler.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Feynman.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::FeynmanFix.boxed(), ForkCondition::Timestamp(0)));
        }
        ForkSpec::Haber | ForkSpec::HaberFix | ForkSpec::Bohr => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Plato.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Hertz.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::HertzFix.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Kepler.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Feynman.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::FeynmanFix.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Haber.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::HaberFix.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Bohr.boxed(), ForkCondition::Timestamp(0)));
        }
        ForkSpec::Pascal | ForkSpec::Lorentz | ForkSpec::Maxwell | ForkSpec::Fermi | ForkSpec::Mendel => {
            forks.push((EthereumHardfork::Frontier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Homestead.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Tangerine.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::SpuriousDragon.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Byzantium.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Constantinople.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Petersburg.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Istanbul.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::MuirGlacier.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Berlin.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::London.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Paris.boxed(), ForkCondition::Block(0)));
            forks.push((EthereumHardfork::Shanghai.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((EthereumHardfork::Prague.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Ramanujan.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Niels.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::MirrorSync.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Bruno.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Euler.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Nano.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Moran.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Gibbs.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Planck.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Luban.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Plato.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Hertz.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::HertzFix.boxed(), ForkCondition::Block(0)));
            forks.push((BscHardfork::Kepler.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Feynman.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::FeynmanFix.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Cancun.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Haber.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::HaberFix.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Bohr.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Pascal.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Lorentz.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Maxwell.boxed(), ForkCondition::Timestamp(0)));
            forks.push((BscHardfork::Fermi.boxed(), ForkCondition::Timestamp(0)));
            if matches!(fork_spec, ForkSpec::Mendel) {
                forks.push((BscHardfork::Mendel.boxed(), ForkCondition::Timestamp(0)));
            }
        }
    }

    ChainHardforks::new(forks)
}

/// Possible seal engines.
#[derive(Debug, PartialEq, Eq, Default, Deserialize)]
pub enum SealEngine {
    /// No consensus checks.
    #[default]
    NoProof,
}

/// Ethereum blockchain test transaction data.
#[derive(Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Transaction {
    /// Transaction type
    #[serde(rename = "type")]
    pub transaction_type: Option<U256>,
    /// Data.
    pub data: Bytes,
    /// Gas limit.
    pub gas_limit: U256,
    /// Gas price.
    pub gas_price: Option<U256>,
    /// Nonce.
    pub nonce: U256,
    /// Signature r part.
    pub r: U256,
    /// Signature s part.
    pub s: U256,
    /// Parity bit.
    pub v: U256,
    /// Transaction value.
    pub value: U256,
    /// Chain ID.
    pub chain_id: Option<U256>,
    /// Access list.
    pub access_list: Option<AccessList>,
    /// Max fee per gas.
    pub max_fee_per_gas: Option<U256>,
    /// Max priority fee per gas
    pub max_priority_fee_per_gas: Option<U256>,
    /// Transaction hash.
    pub hash: Option<B256>,
}

/// Access list item
#[derive(Debug, PartialEq, Eq, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct AccessListItem {
    /// Account address
    pub address: Address,
    /// Storage key.
    pub storage_keys: Vec<B256>,
}

/// Access list.
pub type AccessList = Vec<AccessListItem>;
