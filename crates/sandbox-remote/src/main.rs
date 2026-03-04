use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use lambda_extension::{Extension, NextEvent};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

use sandbox_core::server::{TaskTracker, build_router};
use sandbox_remote::backend::EfsBackend;
use sandbox_remote::metadata::DynamoMetadataStore;
use sandbox_remote::sigv4_client::SigV4CallbackClient;

/// Internal Lambda extension that drains pending background tasks after each invocation.
struct DrainExtension {
    request_done_receiver: Mutex<UnboundedReceiver<()>>,
    tracker: TaskTracker,
}

impl DrainExtension {
    fn new(receiver: UnboundedReceiver<()>, tracker: TaskTracker) -> Self {
        Self {
            request_done_receiver: Mutex::new(receiver),
            tracker,
        }
    }

    async fn invoke(
        &self,
        event: lambda_extension::LambdaEvent,
    ) -> Result<(), lambda_extension::Error> {
        match event.next {
            NextEvent::Shutdown(_) => {
                return Err(anyhow!("extension received unexpected SHUTDOWN event").into());
            }
            NextEvent::Invoke(_) => {}
        }

        // Wait for the HTTP handler to signal it's done
        self.request_done_receiver
            .lock()
            .await
            .recv()
            .await
            .ok_or_else(|| anyhow!("channel closed"))?;

        tracing::info!("Draining background tasks");

        // Drain all spawned background tasks before Lambda freezes
        self.tracker.drain().await;

        tracing::info!("Drained background tasks");

        Ok(())
    }
}

#[tokio::main]
async fn main() -> Result<(), lambda_extension::Error> {
    lambda_http::lambda_runtime::tracing::init_default_subscriber();

    let efs_mount =
        std::env::var("EFS_MOUNT_PATH").unwrap_or_else(|_| "/mnt/efs/sandbox-repos".to_string());

    let table_name =
        std::env::var("DYNAMODB_TABLE").unwrap_or_else(|_| "sandbox-metadata".to_string());

    let aws_config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
    let ddb_client = aws_sdk_dynamodb::Client::new(&aws_config);

    let backend = EfsBackend::new(PathBuf::from(efs_mount));
    let metadata = DynamoMetadataStore::new(ddb_client, table_name);
    let callback_client = SigV4CallbackClient::new(&aws_config);

    let (app, tracker) = build_router(backend, metadata, callback_client);

    let (request_done_sender, request_done_receiver) = unbounded_channel::<()>();

    let drain_ext = Arc::new(DrainExtension::new(request_done_receiver, tracker));
    let extension = Extension::new()
        .with_events(&["INVOKE"])
        .with_events_processor(lambda_extension::service_fn(|event| {
            let drain_ext = drain_ext.clone();
            async move { drain_ext.invoke(event).await }
        }))
        .with_extension_name("drain-tasks")
        .register()
        .await?;

    // Wrap the router to signal the extension after each response
    let sender = request_done_sender;
    let handler = tower::service_fn(move |event: lambda_http::Request| {
        let app = app.clone();
        let sender = sender.clone();
        async move {
            use tower::ServiceExt;
            let resp = app.oneshot(event).await?;
            // Signal the extension that the response has been sent
            let _ = sender.send(());
            Ok::<_, std::convert::Infallible>(resp)
        }
    });

    tokio::try_join!(
        // Poll the handler first (biased) for smaller future + tiny latency win
        biased;
        lambda_http::run_with_streaming_response(handler),
        extension.run(),
    )?;

    Ok(())
}
