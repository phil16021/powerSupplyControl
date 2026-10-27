//! REST + MCP integration tests against the behavioural simulator.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rd6030_core::config::{LimitsConfig, ServerConfig};
use rd6030_core::register::reg;
use rd6030_core::{Controller, Device, DriverOptions};
use rd6030_sim::SimDevice;
use serde_json::{json, Value};
use tower::ServiceExt;

fn controller(sim: &Arc<Mutex<SimDevice>>) -> Controller {
    let options = DriverOptions {
        poll_interval: Some(std::time::Duration::from_millis(50)),
        ..DriverOptions::default()
    };
    Controller::from_device(Device::start(
        rd6030_sim::connector(Arc::clone(sim)),
        options,
    ))
    .with_limits(LimitsConfig {
        max_voltage: 30.0,
        max_current: 10.0,
    })
}

fn sim() -> Arc<Mutex<SimDevice>> {
    let mut dev = SimDevice::new();
    dev.load_ohms = Some(10.0);
    Arc::new(Mutex::new(dev))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let req = match body {
        Some(b) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(b.to_string())),
        None => req.body(Body::empty()),
    }
    .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn rest_controls_simulated_device() {
    let sim = sim();
    let c = controller(&sim);
    let app = rd6030_server::http::app(c.clone(), &ServerConfig::default());

    let (s, v) = call(&app, "GET", "/api/info", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["model"], 60301);

    let (s, v) = call(
        &app,
        "POST",
        "/api/set",
        Some(json!({"voltage": 12.0, "current": 2.0})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["setpoint"]["voltage"], 12.0);

    let (s, _) = call(
        &app,
        "POST",
        "/api/output",
        Some(json!({"enabled": true})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(sim.lock().unwrap().register(reg::OUTPUT), 1);

    // Fresh telemetry reflects the simulated 10 ohm load: 12 V / 1.2 A.
    c.device().read_telemetry().await.unwrap();
    let (s, v) = call(&app, "GET", "/api/telemetry", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["voltage_out"], 12.0);
    assert_eq!(v["current_out"], 1.2);
    assert!(v["seq"].as_u64().unwrap() >= 1);

    // Software limit -> 422, nothing written.
    let (s, v) = call(
        &app,
        "POST",
        "/api/set",
        Some(json!({"voltage": 40.0})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    assert_eq!(sim.lock().unwrap().register(reg::VSET), 1200);

    // Protection + presets.
    let (s, v) = call(
        &app,
        "PUT",
        "/api/protection",
        Some(json!({"ovp": 13.0, "ocp": 3.0})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["ovp"], 13.0);
    let (s, v) = call(&app, "GET", "/api/presets/bank", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["slots"].as_array().unwrap().len(), 10);
    let (s, v) = call(
        &app,
        "PUT",
        "/api/presets/4",
        Some(json!({"voltage": 3.3, "current": 0.5, "ovp": 4.0, "ocp": 1.0})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["written"], true);
    let (s, _) = call(&app, "GET", "/api/presets/12", None, None).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Invalid RTC -> 422.
    let (s, _) = call(
        &app,
        "PUT",
        "/api/rtc",
        Some(json!({"year": 2026, "month": 13, "day": 1, "hour": 0, "minute": 0, "second": 0})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // Calibration is disabled by default.
    let (s, _) = call(
        &app,
        "POST",
        "/api/calibrate/commit",
        Some(json!({"confirm": true})),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    let (s, v) = call(&app, "GET", "/api/health", None, None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["link"]["state"], "connected");
    c.shutdown().await;
}

#[tokio::test]
async fn bearer_token_is_enforced() {
    let sim = sim();
    let c = controller(&sim);
    let server = ServerConfig {
        token: Some("s3cret".to_string()),
        ..ServerConfig::default()
    };
    let app = rd6030_server::http::app(c.clone(), &server);
    let (s, _) = call(&app, "GET", "/api/health", None, None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, "GET", "/api/health", None, Some("wrong")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, _) = call(&app, "GET", "/api/health", None, Some("s3cret")).await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = call(&app, "POST", "/mcp", Some(json!({})), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    c.shutdown().await;
}

#[tokio::test]
async fn device_offline_maps_to_503() {
    let c = Controller::from_device(Device::start(
        rd6030_core::FnConnector(|| -> Result<rd6030_core::RtuBackend, _> {
            Err(rd6030_core::DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no such port",
            )))
        }),
        DriverOptions::default().without_polling(),
    ));
    let app = rd6030_server::http::app(c.clone(), &ServerConfig::default());
    let (s, v) = call(&app, "GET", "/api/info", None, None).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE, "{v}");
    let (_, v) = call(&app, "GET", "/api/health", None, None).await;
    assert_eq!(v["link"]["state"], "degraded");
    c.shutdown().await;
}

#[tokio::test]
async fn mcp_tools_over_stdio_transport() {
    use rmcp::model::CallToolRequestParams;
    use rmcp::ServiceExt;

    let sim = sim();
    let c = controller(&sim);
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let server = rd6030_server::mcp::McpServer::new(c.clone());
    tokio::spawn(async move {
        let running = server.serve(server_io).await.unwrap();
        let _ = running.waiting().await;
    });
    let client = ().serve(client_io).await.unwrap();

    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.to_string()).collect();
    for expected in [
        "get_telemetry",
        "get_device_info",
        "set_voltage",
        "set_protection",
        "get_status",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "missing {expected}: {names:?}"
        );
    }

    let args = json!({"voltage": 5.0}).as_object().unwrap().clone();
    let ok = client
        .call_tool(CallToolRequestParams::new("set_voltage").with_arguments(args))
        .await
        .unwrap();
    assert_ne!(ok.is_error, Some(true));
    assert_eq!(sim.lock().unwrap().register(reg::VSET), 500);

    // Above the software limit: a tool error the model can read.
    let args = json!({"voltage": 45.0}).as_object().unwrap().clone();
    let err = client
        .call_tool(CallToolRequestParams::new("set_voltage").with_arguments(args))
        .await
        .unwrap();
    assert_eq!(err.is_error, Some(true));
    let text = err.content[0].as_text().unwrap().text.clone();
    assert!(text.contains("out of range"), "{text}");
    assert_eq!(sim.lock().unwrap().register(reg::VSET), 500);

    client.cancel().await.unwrap();
    c.shutdown().await;
}
