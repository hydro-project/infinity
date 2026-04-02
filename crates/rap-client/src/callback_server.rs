/// Local HTTP callback server for receiving async RAP tool results.
use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::server::conn::http1;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use rap_protocol::RapCallback;
use std::convert::Infallible;
use std::future::Future;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::mpsc;

/// Bind a TCP listener on localhost with an OS-assigned port.
/// Returns the listener and the base URL (e.g. `http://127.0.0.1:{port}`).
pub async fn bind_callback_listener()
-> Result<(TcpListener, String), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let base_url = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    Ok((listener, base_url))
}

/// Start the accept loop on a pre-bound listener, dispatching each
/// [`RapCallback`] to the given handler.
pub fn start_callback_server_on<F, Fut>(listener: TcpListener, handler: F)
where
    F: Fn(RapCallback) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let handler = Arc::new(handler);
    tracing::info!(
        "RAP callback server listening on {:?}",
        listener.local_addr()
    );
    tokio::spawn(rap_protocol::log_panic(
        "callback_accept_loop",
        async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!("Callback accept error: {}", e);
                        continue;
                    }
                };
                let handler = handler.clone();
                tokio::spawn(rap_protocol::log_panic("callback_connection", async move {
                    let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                        let handler = handler.clone();
                        async move { Ok::<_, Infallible>(handle(req, handler).await) }
                    });
                    if let Err(e) = http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), svc)
                        .await
                    {
                        tracing::warn!("Callback connection error: {}", e);
                    }
                }));
            }
        },
    ));
}

/// Start a callback server that passes each [`RapCallback`] to the given handler.
/// Returns the base URL that RAP tools should POST results to.
pub async fn start_callback_server<F, Fut>(
    handler: F,
) -> Result<String, Box<dyn std::error::Error + Send + Sync>>
where
    F: Fn(RapCallback) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (listener, base_url) = bind_callback_listener().await?;
    start_callback_server_on(listener, handler);
    Ok(base_url)
}

/// Start a callback server that collects [`RapCallback`] values into a channel.
/// Returns `(base_url, receiver)`.
pub async fn start_callback_channel()
-> Result<(String, mpsc::UnboundedReceiver<RapCallback>), Box<dyn std::error::Error + Send + Sync>>
{
    let (tx, rx) = mpsc::unbounded_channel();
    let base_url = start_callback_server(move |cb| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(cb);
        }
    })
    .await?;
    Ok((base_url, rx))
}

fn ok_response(status: StatusCode, body: &str) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .body(Full::new(Bytes::from(body.to_string())))
        .expect("bug: failed to build HTTP response")
}

async fn handle<F, Fut>(req: Request<Incoming>, handler: Arc<F>) -> Response<Full<Bytes>>
where
    F: Fn(RapCallback) -> Fut + Send + Sync,
    Fut: Future<Output = ()> + Send,
{
    if req.method() != hyper::Method::POST {
        return ok_response(StatusCode::METHOD_NOT_ALLOWED, "POST only");
    }

    let body = match req.into_body().collect().await {
        Ok(b) => b.to_bytes(),
        Err(e) => {
            tracing::warn!("Failed to read callback body: {}", e);
            return ok_response(StatusCode::BAD_REQUEST, "Failed to read body");
        }
    };

    let cb: RapCallback = match serde_json::from_slice(&body) {
        Ok(c) => c,
        Err(e) => {
            let raw = String::from_utf8_lossy(&body);
            tracing::error!("Invalid callback payload: {e}\nRaw body: {raw}");
            return ok_response(StatusCode::BAD_REQUEST, &format!("Bad request: {}", e));
        }
    };

    handler(cb).await;
    ok_response(StatusCode::OK, "OK")
}
