//! The resident (local) runtime behind [`LocalAgentSystem`]: an in-process
//! queue, a router, and one driver task per active thread. Step-mode systems
//! use none of this; the shared slice pipeline lives in the parent module.
//!
//! [`LocalAgentSystem`]: super::LocalAgentSystem

mod driver;
mod handle;
mod launch;
mod router;
mod sender;

pub use driver::{ActiveThreads, ThreadLifecycleEvent, ThreadLifecycleState};
pub use handle::ThreadHandle;
pub use launch::{LaunchingSystem, ThreadBuilder};
pub use router::{RunningSystem, StopHandle, SubscribeHandle};
pub use sender::{ChannelSendError, ChannelSender};

pub(crate) use launch::{LaunchRegistry, UnionConfigSource, UnionModelSource};
