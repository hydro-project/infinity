//! The Hydro dataflow for the Infinity Slack bot, plus the message types and
//! shared runtime state it operates on.
//!
//! This crate contains no I/O: Slack events and daemon events flow in as
//! streams, Slack actions and daemon commands flow out. The `infinity-slack-bot`
//! CLI crate compiles the dataflow with Hydro's embedded mode
//! (`generate_embedded`) and drives it around its own event loop.

#[cfg(stageleft_runtime)]
hydro_lang::setup!();

pub mod config;
pub mod daemon;
pub mod flow;
pub mod runtime;
pub mod session_store;
pub mod slack;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
