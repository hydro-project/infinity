//! Compiles and runs the code examples in the documentation as doctests.
//!
//! Each fenced `rust` block in the matched markdown files becomes a doctest,
//! so `cargo test --doc -p infinity-mdtests` fails whenever a documented
//! example stops compiling. Examples whose model provider is constructed in
//! hidden setup lines swap it for [`mock_provider`] and are driven to
//! completion with [`run`], so they also execute. Blocks that need real model
//! providers, network services, or subprocesses stay `rust,no_run`.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use infinity_provider_protocol::{
    CompletionError, CompletionRequest, FinalResponse, ModelEntry, ModelProvider, ModelStream,
    StreamChunk,
};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// The model ids the documentation examples select from the catalog. The
/// first entry is the provider's default model.
pub const MODEL_IDS: &[&str] = &[
    "global.anthropic.claude-sonnet-4-6",
    "global.anthropic.claude-opus-4-8",
];

/// The text every completion from [`mock_provider`] produces.
pub const MOCK_REPLY: &str = "Mock reply from the documentation test provider.";

/// How long [`run`] lets an example execute before failing it.
const EXAMPLE_TIMEOUT: Duration = Duration::from_secs(30);

/// A provider whose catalog contains every model in [`MODEL_IDS`], and whose
/// models answer every request with [`MOCK_REPLY`] and no tool calls, so each
/// completion round finishes immediately.
struct MockProvider;

#[async_trait]
impl ModelProvider for MockProvider {
    async fn list_models(&self) -> Result<Vec<ModelEntry>, BoxError> {
        Ok(MODEL_IDS
            .iter()
            .map(|model_id| ModelEntry {
                model_id: (*model_id).to_owned(),
                display_name: format!("Mock {model_id}"),
                context_window: 200_000,
                max_output_tokens: None,
                supports_image_input: false,
            })
            .collect())
    }

    async fn invoke_model(
        &self,
        _model_id: &str,
        _request: CompletionRequest,
    ) -> Result<ModelStream, CompletionError> {
        Ok(Box::pin(futures_util::stream::iter([
            Ok(StreamChunk::Text(MOCK_REPLY.to_owned())),
            Ok(StreamChunk::Final(FinalResponse { usage: None })),
        ])))
    }
}

/// A mock provider that stands in for `BedrockProvider::from_env()` in hidden
/// doctest setup; see [`MODEL_IDS`] and [`MOCK_REPLY`].
pub fn mock_provider() -> Arc<dyn ModelProvider> {
    Arc::new(MockProvider)
}

/// Run a documentation example to completion on a current-thread runtime
/// inside a `LocalSet` (local agent systems are not `Send`), failing the test
/// if it returns an error or does not finish within the timeout.
pub fn run<F, E>(example: F)
where
    F: Future<Output = Result<(), E>>,
    E: std::fmt::Debug,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("failed to build the doctest runtime");
    let local = tokio::task::LocalSet::new();
    let result = local.block_on(&runtime, async {
        tokio::time::timeout(EXAMPLE_TIMEOUT, example).await
    });
    match result {
        Ok(Ok(())) => {}
        Ok(Err(error)) => panic!("documentation example failed: {error:?}"),
        Err(_) => panic!("documentation example did not finish within {EXAMPLE_TIMEOUT:?}"),
    }
}

#[doc(hidden)]
#[cfg(doctest)]
mod docs {
    include_mdtests::include_mdtests!("docs/docs/infinity-runtime/**/*.md");
}
