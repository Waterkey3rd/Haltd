//! One WebSocket client: JSON-RPC requests in, replies and live data out.
//!
//! Binary frames are little-endian. All times on the wire, JSON or binary, are
//! nanoseconds since the backend started, which keep full precision in a
//! JavaScript number for months.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tsdb::Event;

use crate::rpc::{App, Reply};

const PUSH_INTERVAL: Duration = Duration::from_millis(33);
const OUTBOUND_QUEUE: usize = 256;
const MAX_SAMPLES_PER_FRAME: usize = 8192;

const KIND_SAMPLES: u8 = 1;
const KIND_RTT: u8 = 2;
const KIND_EVENT: u8 = 3;
const VERSION: u8 = 1;

#[derive(Default)]
pub(crate) struct Subscribed {
    /// Series id to next sample index.
    pub series: BTreeMap<u32, u64>,
    /// RTT channel to next byte.
    pub rtt: BTreeMap<u32, u64>,
}

pub(crate) type Subscriptions = Arc<Mutex<Subscribed>>;

/// Encode an event as a kind=3 frame.
fn event_frame(event: &Event, origin: i64) -> Vec<u8> {
    let detail = serde_json::to_vec(&event.detail).unwrap_or_default();
    let mut frame = Vec::with_capacity(16 + detail.len());
    frame.extend_from_slice(&[KIND_EVENT, VERSION]);
    frame.extend_from_slice(&1u16.to_le_bytes());
    frame.extend_from_slice(&(event.t - origin).to_le_bytes());
    frame.extend_from_slice(&(event.kind as u16).to_le_bytes());
    frame.extend_from_slice(&(detail.len() as u16).to_le_bytes());
    frame.extend_from_slice(&detail);
    frame
}

/// Encode new samples as a kind=1 frame, advancing the cursors.
fn samples_frame(app: &App, subscriptions: &Subscriptions) -> Option<Vec<u8>> {
    let mut subscribed = subscriptions.lock().expect("subscription lock");
    let cursors = &mut subscribed.series;
    let origin = app.origin_ns();
    let mut blocks = 0u16;
    let mut frame = vec![KIND_SAMPLES, VERSION, 0, 0];
    for (id, cursor) in cursors.iter_mut() {
        let Some(series) = app.tsdb().get(*id) else {
            continue;
        };
        let chunk = series.read_since(*cursor, MAX_SAMPLES_PER_FRAME);
        *cursor = chunk.next;
        let Some(first) = chunk.t.first() else {
            continue;
        };
        frame.extend_from_slice(&id.to_le_bytes());
        frame.extend_from_slice(&(chunk.t.len() as u32).to_le_bytes());
        frame.extend_from_slice(&(first - origin).to_le_bytes());
        // Timestamps are irregular at the microsecond level: always explicit.
        frame.extend_from_slice(&0u32.to_le_bytes());
        for t in &chunk.t {
            frame.extend_from_slice(&(t - origin).to_le_bytes());
        }
        for v in &chunk.v {
            frame.extend_from_slice(&v.to_le_bytes());
        }
        blocks += 1;
    }
    if blocks == 0 {
        return None;
    }
    frame[2..4].copy_from_slice(&blocks.to_le_bytes());
    Some(frame)
}

/// Encode new terminal bytes as kind=2 frames, advancing the cursors.
fn rtt_frames(app: &App, subscriptions: &Subscriptions) -> Vec<Vec<u8>> {
    let mut subscribed = subscriptions.lock().expect("subscription lock");
    let mut frames = Vec::new();
    for (channel, cursor) in subscribed.rtt.iter_mut() {
        if let Some((data, next)) = app.rtt().terminal_read(*channel, *cursor) {
            *cursor = next;
            let mut frame = Vec::with_capacity(9 + data.len());
            frame.extend_from_slice(&[KIND_RTT, VERSION, 1, 0, *channel as u8]);
            frame.extend_from_slice(&(data.len() as u32).to_le_bytes());
            frame.extend_from_slice(&data);
            frames.push(frame);
        }
    }
    frames
}

fn rpc_error(id: &Value, code: i64, message: String) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Parse and serve one request, off the async threads since it may wait on
/// the probe. Returns None for a notification.
async fn serve_request(
    app: Arc<App>,
    subscriptions: Subscriptions,
    text: String,
) -> Option<Message> {
    let request: Value = match serde_json::from_str(&text) {
        Ok(request) => request,
        Err(error) => {
            return Some(Message::text(
                rpc_error(&Value::Null, -32700, error.to_string()).to_string(),
            ))
        }
    };
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or("").to_owned();
    let params = request["params"].clone();
    let served = tokio::task::spawn_blocking({
        let id = id.clone();
        move || app.dispatch(&method, &params, &id, &subscriptions)
    })
    .await;
    Some(match served {
        Ok(Ok(Reply::Json(result))) => {
            Message::text(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
        }
        Ok(Ok(Reply::Binary(frame))) => Message::binary(frame),
        Ok(Err((code, message))) => Message::text(rpc_error(&id, code, message).to_string()),
        Err(error) => Message::text(rpc_error(&id, -32603, error.to_string()).to_string()),
    })
}

pub(crate) async fn run(socket: WebSocket, app: Arc<App>) {
    let _client = app.client_connected();
    let (mut sink, mut stream) = socket.split();
    let (outbound, mut queue) = mpsc::channel::<Message>(OUTBOUND_QUEUE);
    let subscriptions: Subscriptions = Arc::default();

    let writer = tokio::spawn(async move {
        while let Some(message) = queue.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    let pusher = tokio::spawn({
        let app = app.clone();
        let subscriptions = subscriptions.clone();
        let outbound = outbound.clone();
        async move {
            let mut watches = app.sampler().subscribe_watches();
            let mut ticker = tokio::time::interval(PUSH_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // New clients get events from now on; history comes from a query.
            let mut last_event = app.tsdb().events_since(0).last().map_or(0, |e| e.seq);
            loop {
                tokio::select! {
                    update = watches.recv() => match update {
                        Ok(values) => {
                            let note = json!({
                                "jsonrpc": "2.0",
                                "method": "watch.update",
                                "params": {"values": values.as_ref()},
                            });
                            if outbound.send(Message::text(note.to_string())).await.is_err() {
                                return;
                            }
                        }
                        // Falling behind only costs some intermediate values.
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => return,
                    },
                    _ = ticker.tick() => {
                        let origin = app.origin_ns();
                        for event in app.tsdb().events_since(last_event) {
                            last_event = event.seq;
                            let frame = event_frame(&event, origin);
                            if outbound.send(Message::binary(frame)).await.is_err() {
                                return;
                            }
                        }
                        for frame in rtt_frames(&app, &subscriptions) {
                            if outbound.send(Message::binary(frame)).await.is_err() {
                                return;
                            }
                        }
                        if let Some(frame) = samples_frame(&app, &subscriptions) {
                            if outbound.send(Message::binary(frame)).await.is_err() {
                                return;
                            }
                        }
                    }
                }
            }
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        let text = match message {
            Message::Text(text) => text.to_string(),
            Message::Close(_) => break,
            _ => continue,
        };
        let app = app.clone();
        let subscriptions = subscriptions.clone();
        let outbound = outbound.clone();
        // Concurrent, so a slow request cannot delay a query.
        tokio::spawn(async move {
            if let Some(reply) = serve_request(app, subscriptions, text).await {
                let _ = outbound.send(reply).await;
            }
        });
    }
    pusher.abort();
    drop(outbound);
    let _ = writer.await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tsdb::EventKind;

    #[test]
    fn event_frames_carry_relative_time_and_detail() {
        let event = Event {
            seq: 1,
            t: 1_000_500,
            kind: EventKind::Halted,
            detail: json!({"a": 1}),
        };
        let frame = event_frame(&event, 1_000_000);
        assert_eq!(&frame[..4], &[KIND_EVENT, VERSION, 1, 0]);
        assert_eq!(i64::from_le_bytes(frame[4..12].try_into().unwrap()), 500);
        assert_eq!(u16::from_le_bytes(frame[12..14].try_into().unwrap()), 1);
        let length = u16::from_le_bytes(frame[14..16].try_into().unwrap()) as usize;
        assert_eq!(&frame[16..16 + length], br#"{"a":1}"#);
    }
}
