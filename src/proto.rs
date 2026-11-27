//! Generated from proto/nekorisu/v1/core.proto, and cloud.proto for talking to a trusted source.
#![allow(clippy::all, clippy::pedantic)]

tonic::include_proto!("nekorisu.v1");

/// `nekorisu.cloud.v1`: a device and its trusted source (proto/nekorisu/cloud/v1/cloud.proto).
pub mod cloud {
    #![allow(clippy::all, clippy::pedantic)]
    // The server half is only used by the tests' stand-in source.
    #![allow(dead_code)]
    tonic::include_proto!("nekorisu.cloud.v1");
}

/// Bump when the gRPC interface changes incompatibly; clients compare it in Hello.
pub const PROTOCOL_VERSION: u32 = 1;
