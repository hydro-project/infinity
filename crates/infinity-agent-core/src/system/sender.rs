//! The in-process [`InputSender`] used by the local driver.

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::message::InputMessage;
use crate::traits::InputSender;

/// Error returned if the local system has been shut down (daemon exit).
#[derive(Debug)]
pub struct ChannelSendError;

impl std::fmt::Display for ChannelSendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "agent system input channel closed")
    }
}

impl std::error::Error for ChannelSendError {}

/// An [`InputSender`] that delivers messages to a running local agent system.
/// This is the loopback path for everything the runtime schedules for later:
/// tool results, child-thread seed messages, reports to parents, timer
/// wake-ups.
///
/// A local system runs for the lifetime of its process (threads idle out
/// individually; see [`RunningSystem`](super::RunningSystem)), so sends only
/// fail after an explicit whole-system shutdown.
#[derive(Clone)]
pub struct ChannelSender {
    tx: mpsc::UnboundedSender<(InputMessage, String)>,
}

impl ChannelSender {
    pub(crate) fn new_pair() -> (Self, mpsc::UnboundedReceiver<(InputMessage, String)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

#[async_trait]
impl InputSender for ChannelSender {
    type Error = ChannelSendError;

    async fn send_to_input_queue(
        &self,
        message: InputMessage,
        _group_id: &str,
        dedup_id: &str,
    ) -> Result<(), ChannelSendError> {
        self.tx
            .send((message, dedup_id.to_owned()))
            .map_err(|_| ChannelSendError)
    }
}
