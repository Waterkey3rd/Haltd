//! HTTP front: the embedded web UI and the WebSocket upgrade (DESIGN §6.2).

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use rust_embed::RustEmbed;
use serde::Deserialize;

use crate::rpc::{App, CsvExportRequest};
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

fn sanitize_filename(raw: Option<&str>, default_name: &str, ext: &str) -> String {
    let cleaned: String = raw
        .unwrap_or(default_name)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('.');
    let base = if trimmed.is_empty() {
        default_name
    } else {
        trimmed
    };
    if base.to_ascii_lowercase().ends_with(&format!(".{ext}")) {
        base.to_owned()
    } else {
        format!("{base}.{ext}")
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExportCsvQuery {
    series_ids: Option<String>,
    t0: Option<i64>,
    t1: Option<i64>,
    zoh: Option<bool>,
    relative_time: Option<bool>,
    from_recording: Option<bool>,
    hlrec_path: Option<String>,
    series_names: Option<String>,
    filename: Option<String>,
}

async fn export_csv(
    headers: HeaderMap,
    Query(query): Query<ExportCsvQuery>,
    State(app): State<Arc<App>>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !origin_allowed(origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let ids = query
        .series_ids
        .as_deref()
        .unwrap_or("")
        .split(',')
        .filter_map(|s| s.trim().parse::<u32>().ok())
        .collect();
    let series_names = query.series_names.as_deref().map(|raw| {
        raw.split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
    });
    let req = CsvExportRequest {
        series_ids: ids,
        t0: query.t0.unwrap_or(i64::MIN),
        t1: query.t1.unwrap_or(i64::MAX),
        zoh: query.zoh.unwrap_or(false),
        relative_time: query.relative_time.unwrap_or(true),
        from_recording: query.from_recording.unwrap_or(false),
        hlrec_path: query.hlrec_path.map(PathBuf::from),
        series_names,
    };
    let filename = sanitize_filename(query.filename.as_deref(), "haltd-export", "csv");
    let result = tokio::task::spawn_blocking(move || app.export_csv_bytes(&req)).await;
    match result {
        Ok(Ok((bytes, _rows))) => (
            [
                (header::CONTENT_TYPE, "text/csv; charset=utf-8".to_owned()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{filename}\""),
                ),
            ],
            bytes,
        )
            .into_response(),
        Ok(Err(error)) => (StatusCode::BAD_REQUEST, error.message).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RecordingQuery {
    hlrec_path: Option<String>,
    filename: Option<String>,
}

async fn download_recording(
    headers: HeaderMap,
    Query(query): Query<RecordingQuery>,
    State(app): State<Arc<App>>,
) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok());
    if !origin_allowed(origin) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let given = query.hlrec_path.map(PathBuf::from);
    let requested_name = query.filename;
    let result = tokio::task::spawn_blocking(move || app.read_recording_bytes(given)).await;
    match result {
        Ok(Ok((path, bytes))) => {
            let fallback = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("recording.hlrec");
            let filename = sanitize_filename(requested_name.as_deref(), fallback, "hlrec");
            (
                [
                    (header::CONTENT_TYPE, "application/octet-stream".to_owned()),
                    (
                        header::CONTENT_DISPOSITION,
                        format!("attachment; filename=\"{filename}\""),
                    ),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(error)) => (StatusCode::NOT_FOUND, error.message).into_response(),
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response(),
    }
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
            .route("/api/export.csv", get(export_csv))
            .route("/api/recording.hlrec", get(download_recording))
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
