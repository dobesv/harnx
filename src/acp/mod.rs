mod client;
mod config;

#[derive(Debug, Clone, Default)]
pub struct AcpManager;

pub use client::AcpClient;
pub use config::AcpServerConfig;
