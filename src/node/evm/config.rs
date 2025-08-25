use super::{assembler::BscBlockAssembler, executor::BscBlockExecutor, factory::BscEvmFactory};
use crate::{
    chainspec::BscChainSpec,
    evm::transaction::BscTxEnv,
    hardforks::{bsc::BscHardfork, BscHardforks},
    node::engine_api::validator::BscExecutionData,
    system_contracts::SystemContract,
    BscPrimitives,
};
use alloy_consensus::{transaction::SignerRecoverable, BlockHeader, Header, TxReceipt};
use alloy_eips::eip7840::BlobParams;
use alloy_primitives::{Log, U256};
use reth_chainspec::{EthChainSpec, EthereumHardforks, Hardforks};
use reth_ethereum_forks::EthereumHardfork;
use reth_evm::{
    block::{BlockExecutorFactory, BlockExecutorFor},
    eth::{receipt_builder::ReceiptBuilder, EthBlockExecutionCtx},
    ConfigureEngineEvm, ConfigureEvm, EvmEnv, EvmFactory, ExecutableTxIterator, ExecutionCtxFor,
    FromRecoveredTx, FromTxWithEncoded, IntoTxEnv, NextBlockEnvAttributes,
};
use reth_evm_ethereum::RethReceiptBuilder;
use reth_primitives::{BlockTy, HeaderTy, SealedBlock, SealedHeader, TransactionSigned};
use reth_revm::State;
use revm::{
    context::{BlockEnv, CfgEnv},
    context_interface::block::BlobExcessGasAndPrice,
    primitives::hardfork::SpecId,
    Inspector,
};
use std::{borrow::Cow, convert::Infallible, sync::Arc};

/// Context for BSC block execution.
/// Contains all the fields from EthBlockExecutionCtx plus additional header field.
#[derive(Debug, Clone)]
pub struct BscBlockExecutionCtx<'a> {
    /// Base Ethereum execution context.
    pub base: EthBlockExecutionCtx<'a>,
    /// Block header (optional for BSC-specific logic).
    pub header: Option<Header>,
}

impl<'a> BscBlockExecutionCtx<'a> {
    /// Convert to EthBlockExecutionCtx for compatibility with existing BlockAssembler.
    pub fn as_eth_context(&self) -> &EthBlockExecutionCtx<'a> {
        &self.base
    }
}


/// Ethereum-related EVM configuration.
#[derive(Debug, Clone)]
pub struct BscEvmConfig {
    /// Inner [`BscBlockExecutorFactory`].
    pub executor_factory:
        BscBlockExecutorFactory<RethReceiptBuilder, Arc<BscChainSpec>, BscEvmFactory>,
    /// BSC block assembler.
    pub block_assembler: BscBlockAssembler<BscChainSpec>,
}

impl BscEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec.
    pub fn new(chain_spec: Arc<BscChainSpec>) -> Self {
        Self::bsc(chain_spec)
    }

    /// Creates a new Ethereum EVM configuration.
    pub fn bsc(chain_spec: Arc<BscChainSpec>) -> Self {
        Self::new_with_evm_factory(chain_spec, BscEvmFactory::default())
    }
}

impl BscEvmConfig {
    /// Creates a new Ethereum EVM configuration with the given chain spec and EVM factory.
    pub fn new_with_evm_factory(chain_spec: Arc<BscChainSpec>, evm_factory: BscEvmFactory) -> Self {
        Self {
            block_assembler: BscBlockAssembler::new(chain_spec.clone()),
            executor_factory: BscBlockExecutorFactory::new(
                RethReceiptBuilder::default(),
                chain_spec,
                evm_factory,
            ),
        }
    }

    /// Returns the chain spec associated with this configuration.
    pub const fn chain_spec(&self) -> &Arc<BscChainSpec> {
        self.executor_factory.spec()
    }
}

/// Ethereum block executor factory.
#[derive(Debug, Clone, Default, Copy)]
pub struct BscBlockExecutorFactory<
    R = RethReceiptBuilder,
    Spec = Arc<BscChainSpec>,
    EvmFactory = BscEvmFactory,
> {
    /// Receipt builder.
    receipt_builder: R,
    /// Chain specification.
    spec: Spec,
    /// EVM factory.
    evm_factory: EvmFactory,
}

impl<R, Spec, EvmFactory> BscBlockExecutorFactory<R, Spec, EvmFactory> {
    /// Creates a new [`BscBlockExecutorFactory`] with the given spec, [`EvmFactory`], and
    /// [`ReceiptBuilder`].
    pub const fn new(receipt_builder: R, spec: Spec, evm_factory: EvmFactory) -> Self {
        Self { receipt_builder, spec, evm_factory }
    }

    /// Exposes the receipt builder.
    pub const fn receipt_builder(&self) -> &R {
        &self.receipt_builder
    }

    /// Exposes the chain specification.
    pub const fn spec(&self) -> &Spec {
        &self.spec
    }
}

impl<R, Spec, EvmF> BlockExecutorFactory for BscBlockExecutorFactory<R, Spec, EvmF>
where
    R: ReceiptBuilder<Transaction = TransactionSigned, Receipt: TxReceipt<Log = Log>>,
    Spec: EthereumHardforks + BscHardforks + EthChainSpec + Hardforks + Clone,
    EvmF: EvmFactory<Tx: FromRecoveredTx<TransactionSigned> + FromTxWithEncoded<TransactionSigned>>,
    R::Transaction: From<TransactionSigned> + Clone,
    Self: 'static,
    BscTxEnv: IntoTxEnv<<EvmF as EvmFactory>::Tx>,
{
    type EvmFactory = EvmF;
    type ExecutionCtx<'a> = BscBlockExecutionCtx<'a>;
    type Transaction = TransactionSigned;
    type Receipt = R::Receipt;

    fn evm_factory(&self) -> &Self::EvmFactory {
        &self.evm_factory
    }

    fn create_executor<'a, DB, I>(
        &'a self,
        evm: <Self::EvmFactory as EvmFactory>::Evm<&'a mut State<DB>, I>,
        ctx: Self::ExecutionCtx<'a>,
    ) -> impl BlockExecutorFor<'a, Self, DB, I>
    where
        DB: alloy_evm::Database + 'a,
        I: Inspector<<Self::EvmFactory as EvmFactory>::Context<&'a mut State<DB>>> + 'a,
    {
        BscBlockExecutor::new(
            evm,
            ctx,
            self.spec().clone(),
            self.receipt_builder(),
            SystemContract::new(self.spec().clone()),
        )
    }
}

const EIP1559_INITIAL_BASE_FEE: u64 = 0;

impl ConfigureEvm for BscEvmConfig
where
    Self: Send + Sync + Unpin + Clone + 'static,
{
    type Primitives = BscPrimitives;
    type Error = Infallible;
    type NextBlockEnvCtx = NextBlockEnvAttributes;
    type BlockExecutorFactory = BscBlockExecutorFactory;
    type BlockAssembler = Self;

    fn block_executor_factory(&self) -> &Self::BlockExecutorFactory {
        &self.executor_factory
    }

    fn block_assembler(&self) -> &Self::BlockAssembler {
        self
    }

    fn evm_env(&self, header: &Header) -> EvmEnv<BscHardfork> {
        let blob_params = self.chain_spec().blob_params_at_timestamp(header.timestamp);
        let spec = revm_spec_by_timestamp_and_block_number(
            self.chain_spec().clone(),
            header.timestamp(),
            header.number(),
        );

        // configure evm env based on parent block
        let mut cfg_env =
            CfgEnv::new().with_chain_id(self.chain_spec().chain().id()).with_spec(spec);

        if let Some(blob_params) = &blob_params {
            cfg_env.set_max_blobs_per_tx(blob_params.max_blobs_per_tx);
        }

        // derive the EIP-4844 blob fees from the header's `excess_blob_gas` and the current
        // blobparams
        let blob_excess_gas_and_price =
            header.excess_blob_gas.zip(blob_params).map(|(excess_blob_gas, params)| {
                let blob_gasprice = params.calc_blob_fee(excess_blob_gas);
                BlobExcessGasAndPrice { excess_blob_gas, blob_gasprice }
            });

        let eth_spec = SpecId::from(spec);

        let block_env = BlockEnv {
            number: U256::from(header.number()),
            beneficiary: header.beneficiary(),
            timestamp: U256::from(header.timestamp()),
            difficulty: if eth_spec >= SpecId::MERGE { U256::ZERO } else { header.difficulty() },
            // BSC does not replace the DIFFICULTY output with prevrandao so here we are setting
            // this to the difficulty values to ensure correct opcode outputs
            prevrandao: if eth_spec >= SpecId::MERGE {
                Some(header.difficulty().into())
            } else {
                None
            },
            gas_limit: header.gas_limit(),
            basefee: header.base_fee_per_gas().unwrap_or_default(),
            blob_excess_gas_and_price,
        };

        EvmEnv { cfg_env, block_env }
    }

    fn next_evm_env(
        &self,
        parent: &Header,
        attributes: &Self::NextBlockEnvCtx,
    ) -> Result<EvmEnv<BscHardfork>, Self::Error> {
        // ensure we're not missing any timestamp based hardforks
        let spec_id = revm_spec_by_timestamp_and_block_number(
            self.chain_spec().clone(),
            attributes.timestamp,
            parent.number() + 1,
        );

        // configure evm env based on parent block
        let cfg_env =
            CfgEnv::new().with_chain_id(self.chain_spec().chain().id()).with_spec(spec_id);

        let blob_params = self.chain_spec().blob_params_at_timestamp(attributes.timestamp);

        // if the parent block did not have excess blob gas (i.e. it was pre-cancun), but it is
        // cancun now, we need to set the excess blob gas to the default value(0)
        let blob_excess_gas_and_price = parent
            .maybe_next_block_excess_blob_gas(blob_params)
            .or_else(|| (SpecId::from(spec_id).is_enabled_in(SpecId::CANCUN)).then_some(0))
            .map(|excess_blob_gas| {
                let blob_gasprice =
                    blob_params.unwrap_or_else(BlobParams::cancun).calc_blob_fee(excess_blob_gas);
                BlobExcessGasAndPrice { excess_blob_gas, blob_gasprice }
            });

        let mut basefee = parent.next_block_base_fee(
            self.chain_spec().base_fee_params_at_timestamp(attributes.timestamp),
        );

        let mut gas_limit = U256::from(parent.gas_limit);

        // If we are on the London fork boundary, we need to multiply the parent's gas limit by the
        // elasticity multiplier to get the new gas limit.
        if self
            .chain_spec()
            .inner
            .fork(EthereumHardfork::London)
            .transitions_at_block(parent.number + 1)
        {
            let elasticity_multiplier = self
                .chain_spec()
                .base_fee_params_at_timestamp(attributes.timestamp)
                .elasticity_multiplier;

            // multiply the gas limit by the elasticity multiplier
            gas_limit *= U256::from(elasticity_multiplier);

            // set the base fee to the initial base fee from the EIP-1559 spec
            basefee = Some(EIP1559_INITIAL_BASE_FEE)
        }

        let block_env = BlockEnv {
            number: U256::from(parent.number() + 1),
            beneficiary: attributes.suggested_fee_recipient,
            timestamp: U256::from(attributes.timestamp),
            difficulty: U256::ZERO,
            prevrandao: Some(attributes.prev_randao),
            gas_limit: attributes.gas_limit,
            // calculate basefee based on parent block's gas usage
            basefee: basefee.unwrap_or_default(),
            // calculate excess gas based on parent block's blob gas usage
            blob_excess_gas_and_price,
        };

        Ok(EvmEnv { cfg_env, block_env })
    }

    fn context_for_block<'a>(
        &self,
        block: &'a SealedBlock<BlockTy<Self::Primitives>>,
    ) -> ExecutionCtxFor<'a, Self> {
        BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                parent_hash: block.header().parent_hash,
                parent_beacon_block_root: block.header().parent_beacon_block_root,
                ommers: &block.body().ommers,
                withdrawals: block.body().withdrawals.as_ref().map(Cow::Borrowed),
            },
            header: Some(block.header().clone()),
        }
    }

    fn context_for_next_block(
        &self,
        parent: &SealedHeader<HeaderTy<Self::Primitives>>,
        attributes: Self::NextBlockEnvCtx,
    ) -> ExecutionCtxFor<'_, Self> {
        BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                parent_hash: parent.hash(),
                parent_beacon_block_root: attributes.parent_beacon_block_root,
                ommers: &[],
                withdrawals: attributes.withdrawals.map(Cow::Owned),
            },
            header: None, // No header available for next block context
        }
    }
}

impl ConfigureEngineEvm<BscExecutionData> for BscEvmConfig
where
    Self: Send + Sync + Unpin + Clone + 'static,
{
    fn evm_env_for_payload(&self, payload: &BscExecutionData) -> EvmEnv<BscHardfork> {
        self.evm_env(&payload.0.header)
    }

    fn context_for_payload<'a>(&self, payload: &'a BscExecutionData) -> BscBlockExecutionCtx<'a> {
        let block = &payload.0;
        BscBlockExecutionCtx {
            base: EthBlockExecutionCtx {
                parent_hash: block.header.parent_hash(),
                parent_beacon_block_root: block.header.parent_beacon_block_root,
                ommers: &block.body.inner.ommers,
                withdrawals: block.body.inner.withdrawals.as_ref().map(Cow::Borrowed),
            },
            header: Some(block.header.clone()),
        }
    }

    fn tx_iterator_for_payload(
        &self,
        payload: &BscExecutionData,
    ) -> impl ExecutableTxIterator<Self> {
        payload.0.body.inner.transactions.clone().into_iter().map(|tx| tx.try_into_recovered())
    }
}

/// Map the latest active hardfork at the given timestamp or block number to a [`BscHardfork`].
pub fn revm_spec_by_timestamp_and_block_number(
    chain_spec: impl BscHardforks,
    timestamp: u64,
    block_number: u64,
) -> BscHardfork {
    if chain_spec.is_maxwell_active_at_timestamp(timestamp) {
        BscHardfork::Maxwell
    } else if chain_spec.is_lorentz_active_at_timestamp(timestamp) {
        BscHardfork::Lorentz
    } else if chain_spec.is_pascal_active_at_timestamp(timestamp) {
        BscHardfork::Pascal
    } else if chain_spec.is_bohr_active_at_timestamp(timestamp) {
        BscHardfork::Bohr
    } else if chain_spec.is_haber_fix_active_at_timestamp(timestamp) {
        BscHardfork::HaberFix
    } else if chain_spec.is_haber_active_at_timestamp(timestamp) {
        BscHardfork::Haber
    } else if BscHardforks::is_cancun_active_at_timestamp(&chain_spec, timestamp) {
        BscHardfork::Cancun
    } else if chain_spec.is_feynman_fix_active_at_timestamp(timestamp) {
        BscHardfork::FeynmanFix
    } else if chain_spec.is_feynman_active_at_timestamp(timestamp) {
        BscHardfork::Feynman
    } else if chain_spec.is_kepler_active_at_timestamp(timestamp) {
        BscHardfork::Kepler
    } else if chain_spec.is_hertz_fix_active_at_block(block_number) {
        BscHardfork::HertzFix
    } else if chain_spec.is_hertz_active_at_block(block_number) {
        BscHardfork::Hertz
    } else if chain_spec.is_plato_active_at_block(block_number) {
        BscHardfork::Plato
    } else if chain_spec.is_luban_active_at_block(block_number) {
        BscHardfork::Luban
    } else if chain_spec.is_planck_active_at_block(block_number) {
        BscHardfork::Planck
    } else {
        // Dynamically determine the order for Moran, Nano, Gibbs for the current chain
        fn get_activation_block(fc: &reth_chainspec::ForkCondition) -> Option<u64> {
            match fc {
                reth_chainspec::ForkCondition::Block(b) => Some(*b),
                _ => None,
            }
        }
        let gibbs_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Gibbs));
        let moran_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Moran));
        let nano_block = get_activation_block(&chain_spec.bsc_fork_activation(BscHardfork::Nano));
        // Sort by activation block descending (newest first)
        let mut forks = vec![
            (gibbs_block, BscHardfork::Gibbs),
            (moran_block, BscHardfork::Moran),
            (nano_block, BscHardfork::Nano),
        ];
        forks.sort_by(|a, b| b.0.cmp(&a.0));
        for &(_, fork) in &forks {
            if chain_spec.bsc_fork_activation(fork).active_at_block(block_number) {
                return fork;
            }
        }
        if chain_spec.is_euler_active_at_block(block_number) {
            BscHardfork::Euler
        } else if chain_spec.is_bruno_active_at_block(block_number) {
            BscHardfork::Bruno
        } else if chain_spec.is_mirror_sync_active_at_block(block_number) {
            BscHardfork::MirrorSync
        } else if chain_spec.is_niels_active_at_block(block_number) {
            BscHardfork::Niels
        } else if chain_spec.is_ramanujan_active_at_block(block_number) {
            BscHardfork::Ramanujan
        } else {
            BscHardfork::Frontier
        }
    }
}
