//! The resident (local) runtime behind [`LocalAgentSystem`]: an in-process
//! queue, a router, and one driver task per active thread. Step-mode systems
//! use none of this; the shared slice pipeline lives in the parent module.
//!
//! [`LocalAgentSystem`]: super::LocalAgentSystem

mod driver;
mod router;
mod sender;

pub use driver::ThreadLifecycleEvent;
pub use router::{RunningSystem, SubscribeHandle};
pub use sender::{ChannelSendError, ChannelSender};
