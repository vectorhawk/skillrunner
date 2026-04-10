pub mod aggregator;
pub mod backends_config;
pub mod migration;
pub mod protocol;
pub mod sampling;
pub mod setup;
pub mod stdio_process;

#[cfg(feature = "registry")]
pub mod server;
#[cfg(feature = "registry")]
pub mod tools;

#[cfg(not(feature = "registry"))]
#[path = "server_local.rs"]
pub mod server;
#[cfg(not(feature = "registry"))]
#[path = "tools_local.rs"]
pub mod tools;
