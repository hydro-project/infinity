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
//! let running = system.start_with_handles();
//! let mut thread = running.thread_handle("thread-1").await.expect("system running");
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
mod driver;
mod events;
mod handle;
mod model;
mod observer;
mod router;
mod sender;
mod thread;

pub use builder::{AgentSystem, AgentSystemBuilder, LocalAgentSystem, NoRapHttp};
pub use config::{StaticThreadConfig, ThreadConfig, ThreadConfigSource};
pub use defer::{DeferQueue, InMemoryDeferQueue, NoDeferral};
pub use driver::ActiveThreads;
pub use events::{AgentEvent, ReplaySnapshot, UserChoice};
pub use handle::{HandleObserver, HandleSubscribeRequest, ThreadHandle};
pub use model::{ModelSource, ResolvedModel, StaticModel};
pub use observer::{EventCollector, NullObserver, ThreadObserver};
pub use router::{RunningSystem, SubscribeHandle, SubscribeMessage};
pub use sender::{ChannelSendError, ChannelSender};
pub use thread::{StepOutcome, Thread, is_deferrable_synthetic_event, is_user_text_input};

#[cfg(test)]
mod tests;
