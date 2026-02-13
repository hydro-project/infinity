use lambda_runtime::{Error, run, service_fn, tracing};

mod conversation_history;
mod event_handler;
mod state_store;
mod tools;

use event_handler::function_handler;

#[tokio::main]
async fn main() -> Result<(), Error> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("Failed to install default CryptoProvider");
    tracing::init_default_subscriber();

    run(service_fn(function_handler)).await
}
