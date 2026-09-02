//! BSC-specific gRPC services.

pub mod mev;

/// Protobuf definitions shared with go-bsc's BEP-675 gRPC endpoint.
pub mod mev_proto {
    tonic::include_proto!("mev.v1");
}
