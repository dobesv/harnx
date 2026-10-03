//! NATS plumbing shared by the toolset/hookset servers and the runtime.

pub mod cas;
pub mod connect;
pub mod leader_reads;
pub mod recovery;
pub mod registry;
pub mod rpc;
pub mod shutdown;
