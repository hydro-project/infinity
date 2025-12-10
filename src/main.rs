use lambda_runtime::{Error, run, service_fn, tracing};

mod conversation_history;
mod event_handler;
mod tools;
use event_handler::function_handler;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing::init_default_subscriber();

    run(service_fn(function_handler)).await
}
