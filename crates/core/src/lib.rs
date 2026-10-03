pub mod ai;
pub mod api;
pub mod collect;
pub mod config;
pub mod gpu;
pub mod metrics;
pub mod runner;
pub mod sidecar;
pub mod storage;

pub use config::Config;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
