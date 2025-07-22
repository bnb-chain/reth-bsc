use crate::{BscBlock, BscBlockBody, BscPrimitives};
use reth_chainspec::EthereumHardforks;
use reth_db::transaction::{DbTx, DbTxMut};
use reth_provider::{
    providers::{ChainStorage, NodeTypesForProvider},
    BlockBodyReader, BlockBodyWriter, ChainSpecProvider, ChainStorageReader, ChainStorageWriter,
    DBProvider, DatabaseProvider, EthStorage, ProviderResult, ReadBodyInput, StorageLocation,
};

#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct BscStorage(EthStorage);

impl<Provider> BlockBodyWriter<Provider, BscBlockBody> for BscStorage
where
    Provider: DBProvider<Tx: DbTxMut>,
{
    fn write_block_bodies(
        &self,
        provider: &Provider,
        bodies: Vec<(u64, Option<BscBlockBody>)>,
        write_to: StorageLocation,
    ) -> ProviderResult<()> {
        let (eth_bodies, _sidecars) = bodies
            .into_iter()
            .map(|(block_number, body)| {
                if let Some(BscBlockBody { inner, sidecars }) = body {
                    ((block_number, Some(inner)), (block_number, Some(sidecars)))
                } else {
                    ((block_number, None), (block_number, None))
                }
            })
            .unzip::<_, _, Vec<_>, Vec<_>>();
        self.0.write_block_bodies(provider, eth_bodies, write_to)?;

        // TODO: Write sidecars

        Ok(())
    }

    fn remove_block_bodies_above(
        &self,
        provider: &Provider,
        block: u64,
        remove_from: StorageLocation,
    ) -> ProviderResult<()> {
        self.0.remove_block_bodies_above(provider, block, remove_from)?;

        // TODO: Remove sidecars

        Ok(())
    }
}

impl<Provider> BlockBodyReader<Provider> for BscStorage
where
    Provider: DBProvider + ChainSpecProvider<ChainSpec: EthereumHardforks>,
{
    type Block = BscBlock;

    fn read_block_bodies(
        &self,
        provider: &Provider,
        inputs: Vec<ReadBodyInput<'_, Self::Block>>,
    ) -> ProviderResult<Vec<BscBlockBody>> {
        let eth_bodies = self.0.read_block_bodies(provider, inputs)?;

        // TODO: Read sidecars

        Ok(eth_bodies.into_iter().map(|inner| BscBlockBody { inner, sidecars: None }).collect())
    }
}

impl ChainStorage<BscPrimitives> for BscStorage {
    fn reader<TX, Types>(
        &self,
    ) -> impl ChainStorageReader<DatabaseProvider<TX, Types>, BscPrimitives>
    where
        TX: DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = BscPrimitives>,
    {
        self
    }

    fn writer<TX, Types>(
        &self,
    ) -> impl ChainStorageWriter<DatabaseProvider<TX, Types>, BscPrimitives>
    where
        TX: DbTxMut + DbTx + 'static,
        Types: NodeTypesForProvider<Primitives = BscPrimitives>,
    {
        self
    }
}
