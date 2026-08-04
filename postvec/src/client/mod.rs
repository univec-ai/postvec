//! Transport-agnostic inference client.
//!
//! Everything above the transport (queue, worker, search, migrate) depends
//! only on [`InferenceClient`]. gRPC mode implements it over gRPC/HTTP;
//! embedded mode talks to the in-process engine behind the same trait.

pub mod discovery;
#[cfg(feature = "embedded")]
pub mod embedded;
pub mod grpc;

pub use postvec_core::client::*;
