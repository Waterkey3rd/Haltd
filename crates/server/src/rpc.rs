//! JSON-RPC methods of the data channel (DESIGN §6.2).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

use rtt::Rtt;
use sampler::{BatchSignal, Mode, Sampler};
use serde_json::{json, Value};
use tsdb::{export_recording_csv, export_recording_csv_bytes, CsvOptions, Recorder, Tsdb};

use crate::control::{Control, ControlBus};
use crate::diagnose::Diagnostic;
use crate::presets::{self, TimelinePreset};
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

pub(crate) struct RpcError {
    pub code: i64,
    pub message: String,
    pub data: Option<Box<Diagnostic>>,
}

impl RpcError {
    pub(crate) fn new(code: i64, message: String, data: Option<Diagnostic>) -> Self {
        Self {
            code,
            message,
            data: data.map(Box::new),
        }
    }

    pub(crate) fn from_diagnostic(diag: Diagnostic) -> Self {
        let message = diag.message.clone();
        Self::new(SERVER_ERROR, message, Some(diag))
    }

    pub(crate) fn method_not_found(message: String) -> Self {
        Self::new(METHOD_NOT_FOUND, message, None)
    }
}

type RpcResult = Result<Reply, RpcError>;

fn server_error(message: impl std::fmt::Display) -> RpcError {
    RpcError::from_diagnostic(Diagnostic::from_message(&message.to_string()))
}

fn invalid(message: &str) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        message.to_owned(),
        Some(Diagnostic::from_message(message)),
    )
}

fn ok(value: Value) -> RpcResult {
    Ok(Reply::Json(value))
}

fn string_param<'a>(params: &'a Value, name: &str) -> Result<&'a str, RpcError> {
    params[name]
        .as_str()
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

fn u32_param(params: &Value, name: &str) -> Result<u32, RpcError> {
    params[name]
        .as_u64()
        .map(|value| value as u32)
        .ok_or_else(|| invalid(&format!("{name} must be a number")))
}

fn i64_param(params: &Value, name: &str) -> Result<i64, RpcError> {
    params[name]
        .as_i64()
        .or_else(|| params[name].as_f64().map(|value| value as i64))
        .ok_or_else(|| invalid(&format!("{name} must be a number")))
}

fn series_ids(params: &Value) -> Result<Vec<u32>, RpcError> {
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

fn default_output(extension: &str) -> Result<PathBuf, RpcError> {
    let directory = symbols::paths::cache_root()
        .ok_or_else(|| server_error("no cache directory: set HALTD_CACHE_DIR or give a path"))?
        .join("recordings");
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
    last_recording: Mutex<Option<PathBuf>>,
    origin_ns: i64,
    clients: Arc<AtomicUsize>,
}

pub(crate) struct CsvExportRequest {
    pub series_ids: Vec<u32>,
    pub t0: i64,
    pub t1: i64,
    pub zoh: bool,
    pub relative_time: bool,
    pub from_recording: bool,
    pub hlrec_path: Option<PathBuf>,
    pub series_names: Option<Vec<String>>,
}

/// Counts a connected data client for as long as it lives.
pub(crate) struct ClientGuard(Arc<AtomicUsize>);

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn encode_query_frame(
    tsdb: &Tsdb,
    origin_ns: i64,
    request: u32,
    ids: &[u32],
    t0: i64,
    t1: i64,
    width: usize,
) -> Vec<u8> {
    let (start, end) = (t0 + origin_ns, t1 + origin_ns);
    let (mut min, mut max) = (vec![0.0f64; width], vec![0.0f64; width]);
    let mut frame = Vec::with_capacity(32 + ids.len() * (8 + width * 16));
    frame.extend_from_slice(&[KIND_QUERY, VERSION]);
    frame.extend_from_slice(&(ids.len() as u16).to_le_bytes());
    frame.extend_from_slice(&request.to_le_bytes());
    frame.extend_from_slice(&(width as u32).to_le_bytes());
    frame.extend_from_slice(&0u32.to_le_bytes());
    frame.extend_from_slice(&t0.to_le_bytes());
    frame.extend_from_slice(&t1.to_le_bytes());
    for &id in ids {
        match tsdb.get(id) {
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
    frame
}

impl App {
    pub fn new(sampler: Arc<Sampler>, rtt: Arc<Rtt>, control: Arc<ControlBus>) -> Arc<Self> {
        let origin_ns = sampler.tsdb().now_ns();
        Arc::new(Self {
            sampler,
            rtt,
            control,
            recorder: Mutex::new(None),
            last_recording: Mutex::new(None),
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
            Ok(Err(diag)) => Err(RpcError::from_diagnostic(diag)),
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
        Ok(Reply::Binary(encode_query_frame(
            self.tsdb(),
            self.origin_ns,
            request,
            &ids,
            t0,
            t1,
            width,
        )))
    }

    fn record_stop(&self) -> RpcResult {
        let recorder = self
            .recorder
            .lock()
            .expect("recorder lock")
            .take()
            .ok_or_else(|| server_error("not recording"))?;
        let path = recorder.path().to_owned();
        let started_ns = recorder.started_ns() - self.origin_ns;
        let stats = recorder.stop().map_err(server_error)?;
        *self.last_recording.lock().expect("last recording lock") = Some(path.clone());
        ok(json!({
            "path": path,
            "startedNs": started_ns,
            "samples": stats.samples,
            "dropped": stats.dropped,
        }))
    }

    fn record_start(&self, params: &Value) -> RpcResult {
        let mut slot = self.recorder.lock().expect("recorder lock");
        if slot.is_some() {
            return Err(server_error("already recording"));
        }
        let path = match params["path"].as_str() {
            Some(path) => PathBuf::from(path),
            None => default_output("hlrec")?,
        };
        let recorder =
            Recorder::start(self.tsdb().clone(), Path::new(&path)).map_err(server_error)?;
        *self.last_recording.lock().expect("last recording lock") = Some(path.clone());
        *slot = Some(recorder);
        ok(json!({"path": path}))
    }

    fn csv_options(&self, req: &CsvExportRequest) -> CsvOptions {
        CsvOptions {
            zero_order_hold: req.zoh,
            relative_time: req.relative_time,
            session_origin_ns: self.origin_ns,
        }
    }

    fn resolve_hlrec_path(&self, given: Option<PathBuf>) -> Result<PathBuf, RpcError> {
        given
            .or_else(|| {
                self.last_recording
                    .lock()
                    .expect("last recording lock")
                    .clone()
            })
            .ok_or_else(|| server_error("no recording available to export"))
    }

    pub(crate) fn export_csv_bytes(
        &self,
        req: &CsvExportRequest,
    ) -> Result<(Vec<u8>, u64), RpcError> {
        let options = self.csv_options(req);
        if req.from_recording {
            let hlrec = self.resolve_hlrec_path(req.hlrec_path.clone())?;
            return export_recording_csv_bytes(&hlrec, req.series_names.as_deref(), options)
                .map_err(server_error);
        }
        let t0 = if req.t0 == i64::MIN {
            i64::MIN
        } else {
            req.t0 + self.origin_ns
        };
        let t1 = if req.t1 == i64::MAX {
            i64::MAX
        } else {
            req.t1 + self.origin_ns
        };
        self.tsdb()
            .export_csv_bytes(&req.series_ids, t0, t1, options)
            .map_err(server_error)
    }

    pub(crate) fn read_recording_bytes(
        &self,
        given: Option<PathBuf>,
    ) -> Result<(PathBuf, Vec<u8>), RpcError> {
        let path = self.resolve_hlrec_path(given)?;
        let bytes = std::fs::read(&path).map_err(server_error)?;
        Ok((path, bytes))
    }

    fn record_export(&self, params: &Value) -> RpcResult {
        if params["format"].as_str().unwrap_or("csv") != "csv" {
            return Err(invalid("only csv export is supported"));
        }
        let from_recording = params["fromRecording"].as_bool().unwrap_or(false);
        let ids = if from_recording && !params["seriesIds"].is_array() {
            Vec::new()
        } else {
            series_ids(params)?
        };
        let t0 = params["t0"].as_i64().unwrap_or(i64::MIN);
        let t1 = params["t1"].as_i64().unwrap_or(i64::MAX);
        let path = match params["path"].as_str() {
            Some(path) => PathBuf::from(path),
            None => default_output("csv")?,
        };
        let series_names = params["seriesNames"].as_array().map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect::<Vec<_>>()
        });
        let req = CsvExportRequest {
            series_ids: ids,
            t0,
            t1,
            zoh: params["zoh"].as_bool().unwrap_or(false),
            relative_time: params["relativeTime"].as_bool().unwrap_or(true),
            from_recording,
            hlrec_path: params["hlrecPath"].as_str().map(PathBuf::from),
            series_names,
        };
        let options = self.csv_options(&req);
        let rows = if req.from_recording {
            let hlrec = self.resolve_hlrec_path(req.hlrec_path)?;
            export_recording_csv(&hlrec, &path, req.series_names.as_deref(), options)
                .map_err(server_error)?
        } else {
            let abs_t0 = if req.t0 == i64::MIN {
                i64::MIN
            } else {
                req.t0 + self.origin_ns
            };
            let abs_t1 = if req.t1 == i64::MAX {
                i64::MAX
            } else {
                req.t1 + self.origin_ns
            };
            self.tsdb()
                .export_csv_with(&req.series_ids, abs_t0, abs_t1, options, &path)
                .map_err(server_error)?
        };
        ok(json!({"path": path, "rows": rows}))
    }

    fn record_save_hlrec(&self, params: &Value) -> RpcResult {
        let dest = PathBuf::from(string_param(params, "path")?);
        let src = self.resolve_hlrec_path(params["hlrecPath"].as_str().map(PathBuf::from))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(server_error)?;
        }
        let bytes = std::fs::copy(&src, &dest).map_err(server_error)?;
        ok(json!({"path": dest, "bytes": bytes}))
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
                let rec_guard = self.recorder.lock().expect("recorder lock");
                let rec_path = rec_guard
                    .as_ref()
                    .map(|recorder| recorder.path().to_owned());
                let rec_info = rec_guard.as_ref().map(|recorder| {
                    let stats = recorder.stats();
                    json!({
                        "active": true,
                        "path": recorder.path(),
                        "startedNs": recorder.started_ns() - self.origin_ns,
                        "samples": stats.samples,
                        "dropped": stats.dropped,
                    })
                });
                drop(rec_guard);
                status["recording"] = json!(rec_path);
                status["recordingInfo"] = json!(rec_info);
                status["lastRecordingPath"] = json!(self
                    .last_recording
                    .lock()
                    .expect("last recording lock")
                    .clone());
                status["controller"] = json!(self.control.has_controller());
                ok(status)
            }
            "session.stats" => ok(sampler.stats_json()),
            "symbols.complete" => {
                let query = params["query"].as_str().unwrap_or("");
                let limit = params["limit"].as_u64().unwrap_or(40) as usize;
                let items: Vec<Value> = sampler
                    .complete_globals(query, limit)
                    .into_iter()
                    .map(|item| {
                        json!({
                            "name": item.name,
                            "type": item.type_name,
                            "address": format!("{:#010x}", item.addr),
                        })
                    })
                    .collect();
                ok(json!({"items": items}))
            }

            "watch.add" => {
                let rate = params["rateHz"].as_u64().map(|rate| rate as u32);
                let info = sampler
                    .watch_add(string_param(params, "expr")?, rate)
                    .map_err(server_error)?;
                ok(json!(info))
            }
            "watch.modify" => {
                let info = sampler
                    .watch_modify(u32_param(params, "id")?, string_param(params, "expr")?)
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
            "timeline.clear" => {
                self.rtt.clear_plotted();
                sampler.timeline_clear();
                ok(json!({}))
            }
            "timeline.applyBatch" => {
                let replace = params["replace"].as_bool().unwrap_or(true);
                let rate_hz = params["rateHz"].as_u64().map(|rate| rate as u32);
                let signals = params["signals"]
                    .as_array()
                    .ok_or_else(|| invalid("signals must be an array"))?;
                if replace {
                    self.rtt.clear_plotted();
                }
                let mut expr_items = Vec::new();
                let mut rtt_items = Vec::new();
                for item in signals {
                    let kind = item["kind"].as_str().unwrap_or("expr");
                    let expr = item["expr"]
                        .as_str()
                        .ok_or_else(|| invalid("signal expr must be a string"))?
                        .to_owned();
                    let group = item["group"].as_str().map(str::to_owned);
                    let color = item["color"].as_str().map(str::to_owned);
                    let mode_str = item["mode"].as_str();
                    if kind == "rtt" || mode_str == Some("rtt") {
                        rtt_items.push((expr, group, color));
                    } else {
                        let mode = Mode::parse(mode_str).map_err(|e| invalid(&e))?;
                        let priority = item["priority"].as_i64().unwrap_or(0) as i32;
                        expr_items.push(BatchSignal {
                            expr,
                            mode,
                            group,
                            color,
                            priority,
                        });
                    }
                }
                let mut added: Vec<Value> = sampler
                    .timeline_apply_batch(replace, rate_hz, &expr_items)
                    .map_err(server_error)?
                    .into_iter()
                    .map(|status| json!(status))
                    .collect();
                for (name, group, color) in rtt_items {
                    match self.rtt.plot_signal(&name, group.clone()) {
                        Ok(id) => {
                            if color.is_some() || group.is_some() {
                                self.tsdb().update_meta(id, group.clone(), color.clone());
                            }
                            added.push(json!({
                                "seriesId": id,
                                "name": name,
                                "root": name,
                                "mode": "rtt",
                                "actualMode": "rtt",
                                "rateHz": 0,
                                "error": null,
                            }));
                        }
                        Err(err) => {
                            added.push(json!({
                                "seriesId": 0,
                                "name": name,
                                "root": name,
                                "mode": "rtt",
                                "actualMode": "none",
                                "rateHz": 0,
                                "error": err,
                            }));
                        }
                    }
                }
                self.rtt.set_paused(sampler.rate().rate_hz == 0);
                ok(json!({"series": added}))
            }
            "timeline.presets.list" => {
                let ws = sampler.workspace();
                let data = presets::load(ws.as_deref()).map_err(server_error)?;
                ok(json!(data))
            }
            "timeline.presets.save" => {
                let ws = sampler.workspace();
                let preset: TimelinePreset = serde_json::from_value(params["preset"].clone())
                    .map_err(|e| invalid(&format!("invalid preset: {e}")))?;
                let overwrite = params["overwriteName"].as_str();
                let data =
                    presets::save_preset(ws.as_deref(), preset, overwrite).map_err(server_error)?;
                ok(json!(data))
            }
            "timeline.presets.delete" => {
                let ws = sampler.workspace();
                let data = presets::delete_preset(ws.as_deref(), string_param(params, "name")?)
                    .map_err(server_error)?;
                ok(json!(data))
            }
            "timeline.presets.rename" => {
                let ws = sampler.workspace();
                let data = presets::rename_preset(
                    ws.as_deref(),
                    string_param(params, "from")?,
                    string_param(params, "to")?,
                )
                .map_err(server_error)?;
                ok(json!(data))
            }
            "timeline.presets.setActive" => {
                let ws = sampler.workspace();
                let data = presets::set_active(ws.as_deref(), params["name"].as_str())
                    .map_err(server_error)?;
                ok(json!(data))
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
                        "mode": "rtt",
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
            "record.saveHlrec" => self.record_save_hlrec(params),

            "target.halt" => self.control(Control::Halt),
            "target.go" => self.control(Control::Go),
            "target.reset" => self.control(Control::Reset),
            "flash.reload" => self.control(Control::Reload {
                elf: params["elf"].as_str().map(PathBuf::from),
            }),

            other => Err(RpcError::method_not_found(format!(
                "unknown method {other:?}"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_frame_matches_preallocated_capacity_and_wire_layout() {
        let db = Tsdb::default();
        let s1 = db.create_series("a".to_owned(), None, None);
        let s2 = db.create_series("b".to_owned(), None, None);
        let origin = 1_000_000;
        s1.push(origin + 100, 1.25);
        s2.push(origin + 200, -2.5);

        let width = 16;
        let ids = [s1.id(), s2.id(), 999];
        let frame = encode_query_frame(&db, origin, 42, &ids, 0, 1000, width);
        let expected_len = 32 + ids.len() * (8 + width * 16);
        assert_eq!(frame.len(), expected_len);
        assert_eq!(frame.capacity(), expected_len);
        assert_eq!(frame[0], KIND_QUERY);
        assert_eq!(frame[1], VERSION);
        assert_eq!(u16::from_le_bytes(frame[2..4].try_into().unwrap()), 3);
        assert_eq!(u32::from_le_bytes(frame[4..8].try_into().unwrap()), 42);
        assert_eq!(u32::from_le_bytes(frame[8..12].try_into().unwrap()), 16);
        assert_eq!(i64::from_le_bytes(frame[16..24].try_into().unwrap()), 0);
        assert_eq!(i64::from_le_bytes(frame[24..32].try_into().unwrap()), 1000);
    }
}
