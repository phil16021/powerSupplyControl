//! REST API endpoints for the RD6030.
//!
//! All routes live under `/api`. When a bearer token is configured every
//! request must carry `Authorization: Bearer <token>` (see [`require_token`]).

use std::convert::Infallible;
use std::time::Duration;

use axum::{
    extract::{Path, Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{
        sse::{Event, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{get, post},
    Json, Router,
};
use rd6030_core::{
    BatteryStatus, Controller, DeviceError, DeviceInfo, LinkStatus, Preset, PresetBank, Protection,
    RtcTime, Setpoint, SystemOptions, TelemetrySample,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio_stream::{wrappers::WatchStream, Stream, StreamExt};

/// Shared state of the REST API.
#[derive(Clone, Debug)]
pub struct ApiState {
    /// Device controller.
    pub controller: Controller,
    /// Whether the calibration (NVRAM) endpoints are enabled.
    pub allow_calibration: bool,
}

/// Build the REST API router.
pub fn router() -> Router<ApiState> {
    Router::new()
        .route("/api/health", get(get_health))
        .route("/api/info", get(get_info))
        .route("/api/telemetry", get(get_telemetry))
        .route("/api/telemetry/stream", get(stream_telemetry))
        .route("/api/output", post(set_output))
        .route("/api/set", post(set_setpoint))
        .route("/api/setpoint", get(get_setpoint))
        .route("/api/ramp", post(ramp))
        .route("/api/limits", get(get_limits))
        .route("/api/protection", get(get_protection).put(set_protection))
        .route("/api/lock", get(get_lock).put(set_lock))
        .route("/api/options", get(get_options))
        .route("/api/presets", get(get_preset))
        .route("/api/presets/select", post(select_preset))
        .route("/api/presets/bank", get(export_presets).put(import_presets))
        .route("/api/presets/{slot}", get(read_preset).put(write_preset))
        .route("/api/battery", get(get_battery))
        .route("/api/rtc", get(get_rtc).put(set_rtc))
        .route("/api/rtc/sync", post(sync_rtc))
        .route("/api/calibrate/commit", post(commit_calibration))
}

/// Middleware rejecting requests without the configured bearer token.
pub async fn require_token(
    State(token): State<Option<String>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(expected) = token else {
        return next.run(request).await;
    };
    let provided = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    match provided {
        Some(p) if constant_time_eq(p.as_bytes(), expected.as_bytes()) => next.run(request).await,
        _ => ApiError {
            status: StatusCode::UNAUTHORIZED,
            message: "missing or invalid bearer token".to_string(),
        }
        .into_response(),
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[derive(Serialize)]
struct Health {
    link: LinkStatus,
    telemetry_age_ms: Option<u128>,
}

async fn get_health(State(s): State<ApiState>) -> Json<Health> {
    Json(Health {
        link: s.controller.link(),
        telemetry_age_ms: s.controller.latest().map(|t| t.age().as_millis()),
    })
}

async fn get_info(State(s): State<ApiState>) -> Result<Json<DeviceInfo>, ApiError> {
    Ok(Json(s.controller.info().await?))
}

async fn get_telemetry(State(s): State<ApiState>) -> Result<Json<TelemetrySample>, ApiError> {
    Ok(Json(s.controller.telemetry_sample().await?))
}

/// Server-sent events: one `telemetry` event per new sample.
async fn stream_telemetry(
    State(s): State<ApiState>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = WatchStream::new(s.controller.subscribe()).filter_map(|sample| {
        let sample = sample?;
        let event = Event::default().event("telemetry").json_data(sample).ok()?;
        Some(Ok(event))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[derive(Deserialize)]
struct OutputRequest {
    enabled: bool,
}

async fn set_output(
    State(s): State<ApiState>,
    Json(req): Json<OutputRequest>,
) -> Result<Json<Value>, ApiError> {
    s.controller.set_output(req.enabled).await?;
    Ok(Json(json!({ "ok": true, "output_enabled": req.enabled })))
}

#[derive(Deserialize)]
struct SetRequest {
    voltage: Option<f32>,
    current: Option<f32>,
}

async fn set_setpoint(
    State(s): State<ApiState>,
    Json(req): Json<SetRequest>,
) -> Result<Json<Value>, ApiError> {
    let c = &s.controller;
    match (req.voltage, req.current) {
        (None, None) => return Err(ApiError::bad_request("specify voltage and/or current")),
        (Some(voltage), Some(current)) => c.set_setpoint(Setpoint { voltage, current }).await?,
        (Some(v), None) => c.set_voltage(v).await?,
        (None, Some(a)) => c.set_current(a).await?,
    }
    let sp = c.setpoint().await?;
    Ok(Json(json!({ "ok": true, "setpoint": sp })))
}

async fn get_setpoint(State(s): State<ApiState>) -> Result<Json<Setpoint>, ApiError> {
    Ok(Json(s.controller.setpoint().await?))
}

#[derive(Deserialize)]
struct RampRequest {
    voltage: Option<f32>,
    current: Option<f32>,
    step: f32,
    #[serde(default = "default_ramp_interval")]
    interval_ms: u64,
}

fn default_ramp_interval() -> u64 {
    100
}

async fn ramp(
    State(s): State<ApiState>,
    Json(req): Json<RampRequest>,
) -> Result<Json<Value>, ApiError> {
    let interval = Duration::from_millis(req.interval_ms);
    match (req.voltage, req.current) {
        (Some(v), None) => s.controller.ramp_voltage(v, req.step, interval).await?,
        (None, Some(a)) => s.controller.ramp_current(a, req.step, interval).await?,
        _ => {
            return Err(ApiError::bad_request(
                "specify exactly one of voltage or current",
            ))
        }
    }
    Ok(Json(
        json!({ "ok": true, "setpoint": s.controller.setpoint().await? }),
    ))
}

async fn get_limits(State(s): State<ApiState>) -> Json<Value> {
    let l = s.controller.limits();
    Json(json!({ "max_voltage": l.max_voltage, "max_current": l.max_current }))
}

async fn get_protection(State(s): State<ApiState>) -> Result<Json<Protection>, ApiError> {
    Ok(Json(s.controller.protection().await?))
}

async fn set_protection(
    State(s): State<ApiState>,
    Json(p): Json<Protection>,
) -> Result<Json<Protection>, ApiError> {
    s.controller.set_protection(p).await?;
    Ok(Json(s.controller.protection().await?))
}

#[derive(Deserialize, Serialize)]
struct LockState {
    locked: bool,
}

async fn get_lock(State(s): State<ApiState>) -> Result<Json<LockState>, ApiError> {
    Ok(Json(LockState {
        locked: s.controller.keypad_locked().await?,
    }))
}

async fn set_lock(
    State(s): State<ApiState>,
    Json(req): Json<LockState>,
) -> Result<Json<LockState>, ApiError> {
    s.controller.set_keypad_lock(req.locked).await?;
    Ok(Json(req))
}

async fn get_options(State(s): State<ApiState>) -> Result<Json<SystemOptions>, ApiError> {
    Ok(Json(s.controller.system_options().await?))
}

async fn get_preset(State(s): State<ApiState>) -> Result<Json<Value>, ApiError> {
    let slot = s.controller.active_preset().await?;
    let setpoint = s.controller.setpoint().await?;
    Ok(Json(json!({ "slot": slot, "setpoint": setpoint })))
}

#[derive(Deserialize)]
struct SelectPresetRequest {
    slot: u8,
}

async fn select_preset(
    State(s): State<ApiState>,
    Json(req): Json<SelectPresetRequest>,
) -> Result<Json<Value>, ApiError> {
    s.controller.select_preset(req.slot).await?;
    Ok(Json(json!({ "ok": true, "slot": req.slot })))
}

async fn export_presets(State(s): State<ApiState>) -> Result<Json<PresetBank>, ApiError> {
    Ok(Json(s.controller.export_presets().await?))
}

async fn import_presets(
    State(s): State<ApiState>,
    Json(bank): Json<PresetBank>,
) -> Result<Json<Value>, ApiError> {
    let written = s.controller.import_presets(&bank).await?;
    Ok(Json(json!({ "ok": true, "written": written })))
}

async fn read_preset(
    State(s): State<ApiState>,
    Path(slot): Path<u8>,
) -> Result<Json<Preset>, ApiError> {
    Ok(Json(s.controller.read_preset(slot).await?))
}

async fn write_preset(
    State(s): State<ApiState>,
    Path(slot): Path<u8>,
    Json(preset): Json<Preset>,
) -> Result<Json<Value>, ApiError> {
    let written = s.controller.write_preset(slot, preset).await?;
    Ok(Json(json!({ "ok": true, "written": written })))
}

async fn get_battery(State(s): State<ApiState>) -> Result<Json<BatteryStatus>, ApiError> {
    Ok(Json(s.controller.battery_status().await?))
}

async fn get_rtc(State(s): State<ApiState>) -> Result<Json<RtcTime>, ApiError> {
    Ok(Json(s.controller.read_rtc().await?))
}

async fn set_rtc(
    State(s): State<ApiState>,
    Json(time): Json<RtcTime>,
) -> Result<Json<RtcTime>, ApiError> {
    s.controller.set_rtc(time).await?;
    Ok(Json(time))
}

async fn sync_rtc(State(s): State<ApiState>) -> Result<Json<RtcTime>, ApiError> {
    Ok(Json(s.controller.sync_rtc().await?))
}

#[derive(Deserialize)]
struct CommitRequest {
    confirm: bool,
}

async fn commit_calibration(
    State(s): State<ApiState>,
    Json(req): Json<CommitRequest>,
) -> Result<Json<Value>, ApiError> {
    if !s.allow_calibration {
        return Err(ApiError {
            status: StatusCode::FORBIDDEN,
            message: "calibration endpoints are disabled (server.allow_calibration = false)"
                .to_string(),
        });
    }
    if !req.confirm {
        return Err(ApiError::bad_request(
            "calibration commit requires \"confirm\": true",
        ));
    }
    s.controller.commit_calibration().await?;
    Ok(Json(json!({ "ok": true, "committed": true })))
}

/// JSON error response carrying an HTTP status code.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }
}

/// HTTP status for a device error.
pub fn status_for(err: &DeviceError) -> StatusCode {
    match err {
        DeviceError::OutOfRange(_) => StatusCode::UNPROCESSABLE_ENTITY,
        DeviceError::Forbidden(_) => StatusCode::FORBIDDEN,
        DeviceError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
        DeviceError::NotConnected(_) | DeviceError::Disconnected => StatusCode::SERVICE_UNAVAILABLE,
        DeviceError::Exception(_)
        | DeviceError::Modbus(_)
        | DeviceError::InvalidData
        | DeviceError::UnsupportedModel(_) => StatusCode::BAD_GATEWAY,
        DeviceError::Io(_) | DeviceError::Serial(_) => StatusCode::SERVICE_UNAVAILABLE,
    }
}

impl From<DeviceError> for ApiError {
    fn from(err: DeviceError) -> Self {
        Self {
            status: status_for(&err),
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_compare() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secreT"));
        assert!(!constant_time_eq(b"secret", b"secre"));
    }

    #[test]
    fn error_status_mapping() {
        assert_eq!(
            status_for(&DeviceError::OutOfRange(String::new())),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status_for(&DeviceError::Timeout(Duration::from_millis(1))),
            StatusCode::GATEWAY_TIMEOUT
        );
        assert_eq!(
            status_for(&DeviceError::NotConnected(String::new())),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_for(&DeviceError::Forbidden(String::new())),
            StatusCode::FORBIDDEN
        );
    }
}
