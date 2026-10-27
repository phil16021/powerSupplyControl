//! `rd6030-server` — REST API and MCP server for the Riden RD6030.

pub mod api;
pub mod http;
pub mod mcp;

use rd6030_core::config::Config;
use rd6030_core::Controller;

/// Install a tracing subscriber writing to **stderr** (stdout carries the
/// MCP stdio protocol and CLI output). `RUST_LOG` overrides `default`.
pub fn init_tracing(default: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

/// Start a long-running controller: the driver reconnects in the
/// background, so a missing device is logged but not fatal.
pub async fn start_controller(config: &Config) -> Controller {
    let controller = Controller::start(config);
    if let Err(err) = controller.device().ensure_connected().await {
        tracing::warn!(error = %err, port = %config.port, "device not reachable yet; retrying in background");
    }
    controller
}

/// Honour `safety.output_off_on_exit` and stop the driver.
pub async fn finish(controller: &Controller, config: &Config) {
    controller.shutdown_safely(&config.safety).await;
}

/// Run the HTTP server (REST + MCP) with optional telemetry logging.
pub async fn run_serve(config: &Config) -> anyhow::Result<()> {
    let controller = start_controller(config).await;
    let logger = if config.logging.enabled {
        let logger = rd6030_core::TelemetryLogger::start(&controller, &config.logging).await?;
        tracing::info!(path = %config.logging.path, "telemetry logging enabled");
        Some(logger)
    } else {
        None
    };
    let result = http::serve_http(controller.clone(), config).await;
    if let Some(logger) = logger {
        logger.stop().await;
    }
    finish(&controller, config).await;
    result
}

/// Run the MCP server over stdio.
pub async fn run_mcp_stdio(config: &Config) -> anyhow::Result<()> {
    let controller = start_controller(config).await;
    let result = tokio::select! {
        r = mcp::serve_stdio(controller.clone()) => r,
        _ = http::shutdown_signal() => Ok(()),
    };
    finish(&controller, config).await;
    result
}
