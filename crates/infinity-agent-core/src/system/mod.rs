//! The high-level agent API: build a system of agents with tools, then run it
//! either as **steps** driven by an external queue (serverless) or as a
//! self-contained **local system** with an internal queue and per-thread
//! drivers (resident processes).
//!
//! ```ignore
//! use infinity_agent_core::system::{AgentSystemBuilder, StaticModel};
//! use infinity_agent_core::stores::{InMemoryConversationStore, InMemoryStateStore};
//!
//! let model = StaticModel::new(provider, "my-model").await?;
//! let system = AgentSystemBuilder::new_local(
//!     InMemoryConversationStore::new(),
//!     InMemoryStateStore::new(),
//!     model,
//! )
//! .tools(my_tools)
//! .build_local();
//!
//! let running = system.start();
//! let mut thread = running.thread_builder().launch().await;
//! thread.send_user_text("hello!").await?;
//! while let Some(event) = thread.recv().await { /* ... */ }
//! ```
//!
//! See the crate-level docs and the "Agent System API" section of the
//! Infinity Runtime documentation for the full picture, including how this
//! layers over the low-level API in
//! [`event_processor`](crate::event_processor).

mod builder;
mod config;
mod defer;
mod events;
pub mod local;
mod model;
mod observer;
mod thread;

pub use builder::{AgentSystem, AgentSystemBuilder, LocalAgentSystem, NoRapHttp};
pub use config::{StaticThreadConfig, ThreadConfig, ThreadConfigSource};
pub use defer::{DeferQueue, InMemoryDeferQueue, NoDeferral};
pub use events::{AgentEvent, ReplaySnapshot, UserChoice};
pub use model::{ModelSource, ResolvedModel, StaticModel};
pub use observer::{EventCollector, ThreadObserver};
pub use thread::StepOutcome;

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;
