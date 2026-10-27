//! REST API endpoints for the RD6030.

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rd6030_core::{
    BatteryStatus, Controller, DeviceError, DeviceInfo, DeviceTelemetry, RtcTime, Setpoint,
};
use serde::Deserialize;
use serde_json::{json, Value};

/// Build the REST API router (state = [`Controller`]).
pub fn router() -> Router<Controller> {
    Router::new()
        .route("/api/info", get(get_info))
        .route("/api/telemetry", get(get_telemetry))
        .route("/api/output", post(set_output))
        .route("/api/set", post(set_setpoint))
        .route("/api/presets", get(get_preset))
        .route("/api/presets/select", post(select_preset))
        .route("/api/battery", get(get_battery))
        .route("/api/rtc", get(get_rtc).put(set_rtc))
        .route("/api/rtc/sync", post(sync_rtc))
        .route("/api/calibrate/commit", post(commit_calibration))
}

async fn get_info(State(c): State<Controller>) -> Result<Json<DeviceInfo>, ApiError> {
    Ok(Json(c.info().await?))
}

async fn get_telemetry(State(c): State<Controller>) -> Result<Json<DeviceTelemetry>, ApiError> {
    Ok(Json(c.telemetry().await?))
}

async fn set_output(
    State(c): State<Controller>,
    Json(req): Json<OutputRequest>,
) -> Result<Json<Value>, ApiError> {
    c.set_output(req.enabled).await?;
    Ok(Json(json!({ "ok": true, "output_enabled": req.enabled })))
}

async fn set_setpoint(
    State(c): State<Controller>,
    Json(req): Json<SetRequest>,
) -> Result<Json<Value>, ApiError> {
    match (req.voltage, req.current) {
        (None, None) => return Err(ApiError::bad_request("specify voltage and/or current")),
        (Some(v), Some(a)) => {
            c.set_setpoint(Setpoint {
                voltage: v,
                current: a,
            })
            .await?
        }
        (Some(v), None) => c.set_voltage(v).await?,
        (None, Some(a)) => c.set_current(a).await?,
    }
    let sp = c.setpoint().await?;
    Ok(Json(json!({ "ok": true, "setpoint": sp })))
}

async fn get_preset(State(c): State<Controller>) -> Result<Json<Value>, ApiError> {
    let slot = c.active_preset().await?;
    let setpoint = c.setpoint().await?;
    Ok(Json(json!({ "slot": slot, "setpoint": setpoint })))
}

async fn select_preset(
    State(c): State<Controller>,
    Json(req): Json<SelectPresetRequest>,
) -> Result<Json<Value>, ApiError> {
    c.select_preset(req.slot).await?;
    Ok(Json(json!({ "ok": true, "slot": req.slot })))
}

async fn get_battery(State(c): State<Controller>) -> Result<Json<BatteryStatus>, ApiError> {
    Ok(Json(c.battery_status().await?))
}

async fn get_rtc(State(c): State<Controller>) -> Result<Json<RtcTime>, ApiError> {
    Ok(Json(c.read_rtc().await?))
}

async fn set_rtc(
    State(c): State<Controller>,
    Json(time): Json<RtcTime>,
) -> Result<Json<RtcTime>, ApiError> {
    c.set_rtc(time).await?;
    Ok(Json(time))
}

async fn sync_rtc(State(c): State<Controller>) -> Result<Json<RtcTime>, ApiError> {
    Ok(Json(c.sync_rtc().await?))
}

async fn commit_calibration(
    State(c): State<Controller>,
    Json(req): Json<CommitRequest>,
) -> Result<Json<Value>, ApiError> {
    if !req.confirm {
        return Err(ApiError::bad_request(
            "calibration commit requires \"confirm\": true",
        ));
    }
    c.commit_calibration().await?;
    Ok(Json(json!({ "ok": true, "committed": true })))
}

#[derive(Deserialize)]
struct OutputRequest {
    enabled: bool,
}

#[derive(Deserialize)]
struct SetRequest {
    voltage: Option<f32>,
    current: Option<f32>,
}

#[derive(Deserialize)]
struct SelectPresetRequest {
    slot: u8,
}

#[derive(Deserialize)]
struct CommitRequest {
    confirm: bool,
}

/// JSON error response carrying an HTTP status code.
struct ApiError {
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

impl From<DeviceError> for ApiError {
    fn from(err: DeviceError) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: err.to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}
