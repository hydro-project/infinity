//! The high-level agent API: build a system of agents with tools, then run it
//! either as **steps** driven by an external queue (serverless) or as a
//! self-contained **local system** with an internal queue and per-thread
//! drivers (resident processes).
//!
//! Local systems run with a [`ThreadObserver`] supplied by their embedding;
//! step-mode systems expose [`AgentSystem::step`] for external transports.
//! Both build on [`event_processor`](crate::event_processor).

mod builder;
mod config;
mod defer;
mod events;
pub mod local;
mod model;
mod observer;
mod thread;

pub use builder::{AgentSystem, AgentSystemBuilder, LocalAgentSystem, NoRapHttp};
pub use config::{ThreadConfig, ThreadConfigSource};
pub use defer::{DeferQueue, InMemoryDeferQueue, NoDeferral};
pub use events::{AgentEvent, ReplaySnapshot, UserChoice};
pub use model::{ModelSource, ResolvedModel, StaticModel};
pub use observer::{EventCollector, ThreadObserver};
pub use thread::StepOutcome;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
