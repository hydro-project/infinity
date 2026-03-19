type BoxError = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), BoxError> {
    let local = tokio::task::LocalSet::new();
    local.run_until(infinity_daemon::run_daemon()).await
}
