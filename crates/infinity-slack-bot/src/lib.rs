#[cfg(stageleft_runtime)]
hydro_lang::setup!();

pub mod config;
pub mod daemon_client;
pub mod daemon_sidecar;
pub mod flow;
pub mod runtime;
pub mod session_store;
pub mod sidecar;
pub mod slack_client;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
