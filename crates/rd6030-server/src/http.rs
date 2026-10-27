//! HTTP server: REST API + streamable-HTTP MCP endpoint.

use axum::Router;
use rd6030_core::config::Config;
use rd6030_core::Controller;

use crate::{api, mcp};

/// Build the combined HTTP application (REST API + MCP at `/mcp`).
pub fn app(controller: Controller) -> Router {
    api::router()
        .with_state(controller.clone())
        .merge(mcp::http_router(controller))
}

/// Run the HTTP server until it is shut down.
pub async fn serve_http(controller: Controller, config: &Config) -> anyhow::Result<()> {
    let app = app(controller);
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "HTTP server listening (REST API + MCP)");
    axum::serve(listener, app).await?;
    Ok(())
}
