//! HTTP front: the embedded web UI and the WebSocket upgrade (DESIGN §6.2).

use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;

use crate::rpc::App;
use crate::ws;

#[derive(RustEmbed)]
#[folder = "../../webui/dist"]
struct Assets;

fn content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("svg") => "image/svg+xml",
        Some("map") | Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

fn asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => (
            [(header::CONTENT_TYPE, content_type(path))],
            file.data.into_owned(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn page() -> Response {
    asset("index.html")
}

async fn rtt_page() -> Response {
    asset("rtt.html")
}

async fn static_file(uri: Uri) -> Response {
    asset(uri.path().trim_start_matches('/'))
}

/// A web page must not be able to reach the debugger just because the browser
/// runs on this machine. Native clients send no Origin.
pub(crate) fn origin_allowed(origin: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let Some(rest) = origin.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split(':').next().unwrap_or(rest);
    host == "127.0.0.1" || host == "localhost"
}

async fn upgrade(
    socket: WebSocketUpgrade,
    headers: HeaderMap,
    State(app): State<Arc<App>>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !origin_allowed(origin) {
        tracing::warn!("refused a WebSocket from origin {origin:?}");
        return StatusCode::FORBIDDEN.into_response();
    }
    socket.on_upgrade(move |socket| ws::run(socket, app))
}

pub fn run(app: Arc<App>, port: u16) -> std::io::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let router = Router::new()
            .route("/", get(page))
            .route("/timeline", get(page))
            .route("/rtt", get(rtt_page))
            .route("/ws", get(upgrade))
            .fallback(static_file)
            .with_state(app);
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        tracing::info!("serving the web UI on http://127.0.0.1:{port}");
        axum::serve(listener, router).await
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_local_origins_may_connect() {
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("http://127.0.0.1:47110")));
        assert!(origin_allowed(Some("http://localhost:5173")));
        assert!(!origin_allowed(Some("https://evil.example")));
        assert!(!origin_allowed(Some("http://127.0.0.1.evil.example")));
        assert!(!origin_allowed(Some("null")));
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert!(content_type("a/b.js").starts_with("text/javascript"));
        assert_eq!(content_type("x.bin"), "application/octet-stream");
    }
}
