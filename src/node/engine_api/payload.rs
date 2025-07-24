use crate::node::{engine::BscBuiltPayload, engine_api::validator::BscExecutionData};
use reth::{
    payload::EthPayloadBuilderAttributes,
    primitives::{NodePrimitives, SealedBlock},
};
use reth_node_ethereum::engine::EthPayloadAttributes;
use reth_payload_primitives::{BuiltPayload, PayloadTypes};

/// A default payload type for [`BscPayloadTypes`]
#[derive(Debug, Default, Clone, serde::Deserialize, serde::Serialize)]
#[non_exhaustive]
pub struct BscPayloadTypes;

impl PayloadTypes for BscPayloadTypes {
    type BuiltPayload = BscBuiltPayload;
    type PayloadAttributes = EthPayloadAttributes;
    type PayloadBuilderAttributes = EthPayloadBuilderAttributes;
    type ExecutionData = BscExecutionData;

    fn block_to_payload(
        block: SealedBlock<
            <<Self::BuiltPayload as BuiltPayload>::Primitives as NodePrimitives>::Block,
        >,
    ) -> Self::ExecutionData {
        BscExecutionData(block.into_block())
    }
}
