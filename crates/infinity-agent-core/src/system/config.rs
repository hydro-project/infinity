//! Per-thread agent configuration: tools, system prompt, and tool-server
//! notifier, resolved when a thread is loaded.

use std::rc::Rc;

use async_trait::async_trait;

use crate::tools::Tool;
use crate::traits::InputSender;
use rap_client::http::HttpClient;
use rap_client::notifier::RapNotifier;

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The configuration a thread runs with. Resolved by a
/// [`ThreadConfigSource`] each time a thread is loaded, so
/// different threads of one system can see different toolsets (e.g. one
/// daemon session per working directory, each with its own RAP servers).
///
/// Tools are reference-counted so an embedding can hand the same tool
/// instances to every thread of a session while retaining its own handles
/// (e.g. to shut down the server behind a tool when the session idles).
pub struct ThreadConfig<M: InputSender, H: HttpClient> {
    pub tools: Vec<Rc<dyn Tool<M>>>,
    /// Appended to the built-in system prompt.
    pub extra_system_prompt: Option<String>,
    /// Best-effort lifecycle notifications (tool cancellation, thread
    /// closure) to this thread's RAP tool servers.
    pub rap_notifier: Option<RapNotifier<H>>,
}

/// Resolves the configuration for each thread as it is loaded.
///
/// Like [`ModelSource`](super::ModelSource), resolution happens per thread
/// load rather than once at system construction, which is what lets a single
/// long-lived system serve many differently-configured conversations.
#[async_trait(?Send)]
pub trait ThreadConfigSource<M: InputSender, H: HttpClient> {
    async fn resolve(&self, thread_id: &str) -> Result<ThreadConfig<M, H>, BoxError>;
}

/// Empty fixed configuration used when an embedding does not provide a
/// [`ThreadConfigSource`].
pub(crate) struct StaticThreadConfig<M: InputSender, H: HttpClient> {
    pub(crate) tools: Vec<Rc<dyn Tool<M>>>,
    pub(crate) extra_system_prompt: Option<String>,
    pub(crate) rap_notifier: Option<RapNotifier<H>>,
}

impl<M: InputSender, H: HttpClient> Default for StaticThreadConfig<M, H> {
    fn default() -> Self {
        Self {
            tools: Vec::new(),
            extra_system_prompt: None,
            rap_notifier: None,
        }
    }
}

#[async_trait(?Send)]
impl<M: InputSender + 'static, H: HttpClient + 'static> ThreadConfigSource<M, H>
    for StaticThreadConfig<M, H>
{
    async fn resolve(&self, _thread_id: &str) -> Result<ThreadConfig<M, H>, BoxError> {
        Ok(ThreadConfig {
            tools: self.tools.clone(),
            extra_system_prompt: self.extra_system_prompt.clone(),
            rap_notifier: self.rap_notifier.clone(),
        })
    }
}
