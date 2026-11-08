//! JSON-RPC methods of the data channel (DESIGN §6.2).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use rtt::Rtt;
use sampler::{Mode, Sampler};
use serde_json::{json, Value};
use tsdb::{Recorder, Tsdb};

use crate::control::{Control, ControlBus};
use crate::ws::Subscriptions;

const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_ERROR: i64 = -32000;
const MAX_COLUMNS: usize = 8192;
const KIND_QUERY: u8 = 4;
const VERSION: u8 = 1;

pub(crate) enum Reply {
    Json(Value),
    Binary(Vec<u8>),
}

type RpcResult = Result<Reply, (i64, String)>;

fn server_error(message: impl std::fmt::Display) -> (i64, String) {
    (SERVER_ERROR, message.to_string())
}

fn invalid(message: &str) -> (i64, String) {
    (INVALID_PARAMS, message.to_owned())
}

fn ok(value: Value) -> RpcResult {
    Ok(Reply::Json(value))
}

fn string_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, (i64, String)> {
    params[name]
        .as_str()
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

fn u32_param(params: &Value, name: &str) -> Result<u32, (i64, String)> {
    params[name]
        .as_u64()
        .map(|value| value as u32)
        .ok_or_else(|| invalid(&format!("{name} must be a number")))
}

fn i64_param(params: &Value, name: &str) -> Result<i64, (i64, String)> {
    params[name]
        .as_i64()
        .or_else(|| params[name].as_f64().map(|value| value as i64))
        .ok_or_else(|| invalid(&format!("{name} must be a number")))
}

fn series_ids(params: &Value) -> Result<Vec<u32>, (i64, String)> {
    params["seriesIds"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_u64().map(|id| id as u32))
                .collect()
        })
        .ok_or_else(|| invalid("seriesIds must be an array"))
}

fn default_output(extension: &str) -> Result<PathBuf, (i64, String)> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .ok_or_else(|| server_error("no cache directory"))?;
    let directory = base.join("jlinkd").join("recordings");
    std::fs::create_dir_all(&directory).map_err(server_error)?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    Ok(directory.join(format!("{stamp}.{extension}")))
}

pub struct App {
    sampler: Arc<Sampler>,
    rtt: Arc<Rtt>,
    control: Arc<ControlBus>,
    recorder: Mutex<Option<Recorder>>,
    origin_ns: i64,
    clients: Arc<AtomicUsize>,
}

/// Counts a connected data client for as long as it lives.
pub(crate) struct ClientGuard(Arc<AtomicUsize>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl App {
    pub fn new(sampler: Arc<Sampler>, rtt: Arc<Rtt>, control: Arc<ControlBus>) -> Arc<Self> {
        let origin_ns = sampler.tsdb().now_ns();
        Arc::new(Self {
            sampler,
            rtt,
            control,
            recorder: Mutex::new(None),
            origin_ns,
            clients: Arc::default(),
        })
    }

    pub(crate) fn client_count(&self) -> usize {
        self.clients.load(Ordering::SeqCst)
    }

    pub(crate) fn client_connected(&self) -> ClientGuard {
        self.clients.fetch_add(1, Ordering::SeqCst);
        ClientGuard(self.clients.clone())
    }

    pub(crate) fn sampler(&self) -> &Arc<Sampler> {
        &self.sampler
    }

    pub(crate) fn rtt(&self) -> &Arc<Rtt> {
        &self.rtt
    }

    pub(crate) fn tsdb(&self) -> &Arc<Tsdb> {
        self.sampler.tsdb()
    }

    pub(crate) fn origin_ns(&self) -> i64 {
        self.origin_ns
    }

    fn control(&self, command: Control) -> RpcResult {
        let (reply, result) = mpsc::channel();
        self.control.send(command, reply);
        match result.recv() {
            Ok(Ok(value)) => ok(value),
            Ok(Err(message)) => Err(server_error(message)),
            Err(_) => Err(server_error("the debug session ended")),
        }
    }

    fn query(&self, params: &Value, id: &Value) -> RpcResult {
        let request =
            id.as_u64()
                .ok_or_else(|| invalid("timeline.query needs a numeric id"))? as u32;
        let ids = series_ids(params)?;
        let t0 = i64_param(params, "t0")?;
        let t1 = i64_param(params, "t1")?;
        let width = u32_param(params, "widthPx")? as usize;
        if t1 <= t0 || width == 0 || width > MAX_COLUMNS {
            return Err(invalid("need t1 > t0 and a sensible widthPx"));
        }
        let (start, end) = (t0 + self.origin_ns, t1 + self.origin_ns);
        let (mut min, mut max) = (vec![0.0f64; width], vec![0.0f64; width]);
        let mut frame = Vec::with_capacity(24 + ids.len() * (8 + width * 16));
        frame.extend_from_slice(&[KIND_QUERY, VERSION]);
        frame.extend_from_slice(&(ids.len() as u16).to_le_bytes());
        frame.extend_from_slice(&request.to_le_bytes());
        frame.extend_from_slice(&(width as u32).to_le_bytes());
        frame.extend_from_slice(&0u32.to_le_bytes());
        frame.extend_from_slice(&t0.to_le_bytes());
        frame.extend_from_slice(&t1.to_le_bytes());
        for id in ids {
            match self.tsdb().get(id) {
                Some(series) => series.columns(start, end, &mut min, &mut max),
                None => {
                    min.fill(f64::NAN);
                    max.fill(f64::NAN);
                }
            }
            frame.extend_from_slice(&id.to_le_bytes());
            frame.extend_from_slice(&0u32.to_le_bytes());
            for value in &min {
                frame.extend_from_slice(&value.to_le_bytes());
            }
            for value in &max {
                frame.extend_from_slice(&value.to_le_bytes());
            }
        }
        Ok(Reply::Binary(frame))
    }

    fn record_stop(&self) -> RpcResult {
        let recorder = self
            .recorder
            .lock()
            .expect("recorder lock")
            .take()
            .ok_or_else(|| server_error("not recording"))?;
        let path = recorder.path().to_owned();
        let stats = recorder.stop().map_err(server_error)?;
        ok(json!({"path": path, "samples": stats.samples, "dropped": stats.dropped}))
    }

    fn record_start(&self, params: &Value) -> RpcResult {
        let mut slot = self.recorder.lock().expect("recorder lock");
        if slot.is_some() {
            return Err(server_error("already recording"));
        }
        let path = match params["path"].as_str() {
            Some(path) => PathBuf::from(path),
            None => default_output("jlrec")?,
        };
        let recorder =
            Recorder::start(self.tsdb().clone(), Path::new(&path)).map_err(server_error)?;
        *slot = Some(recorder);
        ok(json!({"path": path}))
    }

    fn record_export(&self, params: &Value) -> RpcResult {
        if params["format"].as_str().unwrap_or("csv") != "csv" {
            return Err(invalid("only csv export is supported"));
        }
        let ids = series_ids(params)?;
        let t0 = params["t0"]
            .as_i64()
            .map_or(i64::MIN, |t| t + self.origin_ns);
        let t1 = params["t1"]
            .as_i64()
            .map_or(i64::MAX, |t| t + self.origin_ns);
        let path = match params["path"].as_str() {
            Some(path) => PathBuf::from(path),
            None => default_output("csv")?,
        };
        let rows = self
            .tsdb()
            .export_csv(&ids, t0, t1, &path)
            .map_err(server_error)?;
        ok(json!({"path": path, "rows": rows}))
    }

    fn events(&self, params: &Value) -> RpcResult {
        let t0 = params["t0"].as_i64().unwrap_or(0) + self.origin_ns;
        let t1 = params["t1"]
            .as_i64()
            .map_or(i64::MAX, |t| t + self.origin_ns);
        let events: Vec<Value> = self
            .tsdb()
            .events_in(t0, t1)
            .into_iter()
            .map(|event| {
                json!({
                    "t": event.t - self.origin_ns,
                    "kind": event.kind as u16,
                    "detail": event.detail,
                })
            })
            .collect();
        ok(json!({"events": events}))
    }

    pub(crate) fn dispatch(
        &self,
        method: &str,
        params: &Value,
        id: &Value,
        subscriptions: &Subscriptions,
    ) -> RpcResult {
        let sampler = &self.sampler;
        match method {
            "session.status" => {
                let mut status = sampler.status_json();
                status["nowNs"] = json!(self.tsdb().now_ns() - self.origin_ns);
                status["recording"] = json!(self
                    .recorder
                    .lock()
                    .expect("recorder lock")
                    .as_ref()
                    .map(|recorder| recorder.path().to_owned()));
                status["controller"] = json!(self.control.has_controller());
                ok(status)
            }
            "session.stats" => ok(sampler.stats_json()),

            "watch.add" => {
                let rate = params["rateHz"].as_u64().map(|rate| rate as u32);
                let info = sampler
                    .watch_add(string_param(params, "expr")?, rate)
                    .map_err(server_error)?;
                ok(json!(info))
            }
            "watch.remove" => {
                sampler
                    .watch_remove(u32_param(params, "id")?)
                    .map_err(server_error)?;
                ok(json!({}))
            }
            "watch.list" => ok(json!(sampler.watch_list())),
            "watch.write" => {
                sampler
                    .watch_write(u32_param(params, "id")?, string_param(params, "value")?)
                    .map_err(server_error)?;
                ok(json!({}))
            }
            "watch.expand" => ok(json!(sampler
                .watch_expand(u32_param(params, "id")?)
                .map_err(server_error)?)),
            "watch.collapse" => {
                sampler
                    .watch_collapse(u32_param(params, "id")?)
                    .map_err(server_error)?;
                ok(json!({}))
            }
            "watch.setRate" => {
                sampler
                    .watch_set_rate(u32_param(params, "id")?, u32_param(params, "rateHz")?)
                    .map_err(server_error)?;
                ok(json!({}))
            }

            "timeline.add" if params["rttSignal"].is_string() => {
                let name = string_param(params, "rttSignal")?;
                let group = params["group"].as_str().map(str::to_owned);
                let id = self.rtt.plot_signal(name, group).map_err(server_error)?;
                ok(json!({"series": [{
                    "seriesId": id,
                    "name": name,
                    "actualMode": "rtt",
                    "error": null,
                }]}))
            }
            "timeline.add" => {
                let mode = Mode::parse(params["mode"].as_str()).map_err(|e| invalid(&e))?;
                let group = params["group"].as_str().map(str::to_owned);
                let color = params["color"].as_str().map(str::to_owned);
                let priority = params["priority"].as_i64().unwrap_or(0) as i32;
                let series = sampler
                    .timeline_add(string_param(params, "expr")?, mode, group, color, priority)
                    .map_err(server_error)?;
                ok(json!({"series": series}))
            }
            "timeline.remove" => {
                let id = u32_param(params, "seriesId")?;
                if !self.rtt.remove_series(id) {
                    sampler.timeline_remove(id).map_err(server_error)?;
                }
                ok(json!({}))
            }
            "timeline.list" => {
                let decorate = |mut item: Value, id: u32| {
                    let meta = self.tsdb().get(id).map(|series| series.meta());
                    item["group"] = json!(meta.as_ref().and_then(|m| m.group.clone()));
                    item["color"] = json!(meta.as_ref().and_then(|m| m.color.clone()));
                    item
                };
                let mut series: Vec<Value> = sampler
                    .timeline_list()
                    .iter()
                    .map(|status| decorate(json!(status), status.series_id))
                    .collect();
                for (id, name) in self.rtt.plotted() {
                    let item = json!({
                        "seriesId": id,
                        "name": name,
                        "root": name,
                        "actualMode": "rtt",
                        "rateHz": 0,
                        "error": null,
                    });
                    series.push(decorate(item, id));
                }
                ok(json!({"series": series}))
            }
            "rtt.status" => ok(json!(self.rtt.status())),
            "rtt.channels" => ok(json!({"channels": self.rtt.channels()})),
            "rtt.signals" => ok(json!({"signals": self.rtt.signals()})),
            "rtt.subscribe" => {
                let channel = u32_param(params, "ch")?;
                let cursor = self.rtt.terminal_cursor(channel);
                subscriptions
                    .lock()
                    .expect("subscription lock")
                    .rtt
                    .insert(channel, cursor);
                ok(json!({}))
            }
            "rtt.unsubscribe" => {
                subscriptions
                    .lock()
                    .expect("subscription lock")
                    .rtt
                    .remove(&u32_param(params, "ch")?);
                ok(json!({}))
            }
            "rtt.write" => {
                let written = self
                    .rtt
                    .write(
                        u32_param(params, "ch")?,
                        string_param(params, "data")?.as_bytes().to_vec(),
                    )
                    .map_err(server_error)?;
                ok(json!({"written": written}))
            }
            "timeline.update" => {
                let group = params["group"].as_str().map(str::to_owned);
                let color = params["color"].as_str().map(str::to_owned);
                if !self
                    .tsdb()
                    .update_meta(u32_param(params, "seriesId")?, group, color)
                {
                    return Err(server_error("no such series"));
                }
                ok(json!({}))
            }
            "timeline.rate" => ok(json!(sampler.rate())),
            "timeline.setRate" => {
                let rate = sampler
                    .set_rate(u32_param(params, "rateHz")?)
                    .map_err(server_error)?;
                self.rtt.set_paused(rate.rate_hz == 0);
                ok(json!(rate))
            }
            "timeline.query" => self.query(params, id),
            "timeline.subscribe" => {
                let mut subscribed = subscriptions.lock().expect("subscription lock");
                for series in series_ids(params)? {
                    if let Some(item) = self.tsdb().get(series) {
                        subscribed.series.insert(series, item.total());
                    }
                }
                ok(json!({}))
            }
            "timeline.unsubscribe" => {
                let mut subscribed = subscriptions.lock().expect("subscription lock");
                for series in series_ids(params)? {
                    subscribed.series.remove(&series);
                }
                ok(json!({}))
            }
            "events.query" => self.events(params),

            "record.start" => self.record_start(params),
            "record.stop" => self.record_stop(),
            "record.export" => self.record_export(params),

            "target.halt" => self.control(Control::Halt),
            "target.go" => self.control(Control::Go),
            "target.reset" => self.control(Control::Reset),
            "flash.reload" => self.control(Control::Reload {
                elf: params["elf"].as_str().map(PathBuf::from),
            }),

            other => Err((METHOD_NOT_FOUND, format!("unknown method {other:?}"))),
        }
    }
}
