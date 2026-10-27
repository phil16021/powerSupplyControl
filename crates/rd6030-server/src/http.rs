//! HTTP server: REST API + streamable-HTTP MCP endpoint.

use std::net::{IpAddr, ToSocketAddrs};

use anyhow::{bail, Context as _};
use axum::{middleware, Router};
use rd6030_core::config::{Config, ServerConfig};
use rd6030_core::Controller;

use crate::api::{self, ApiState};
use crate::mcp;

/// Build the combined HTTP application (REST API + MCP at `/mcp`), with the
/// bearer-token check applied to every route when a token is configured.
pub fn app(controller: Controller, server: &ServerConfig) -> Router {
    let state = ApiState {
        controller: controller.clone(),
        allow_calibration: server.allow_calibration,
    };
    api::router()
        .with_state(state)
        .merge(mcp::http_router(controller))
        .layer(middleware::from_fn_with_state(
            server.token.clone(),
            api::require_token,
        ))
}

/// Refuse to expose the power supply on a non-loopback address without a
/// bearer token.
pub fn check_bind_policy(server: &ServerConfig) -> anyhow::Result<()> {
    if server.token.is_some() {
        return Ok(());
    }
    let addrs = (server.host.as_str(), server.port)
        .to_socket_addrs()
        .with_context(|| format!("resolve bind host {}", server.host))?;
    for addr in addrs {
        if !is_loopback(addr.ip()) {
            bail!(
                "refusing to bind to non-loopback address {addr} without server.token; \
                 set a bearer token in the configuration"
            );
        }
    }
    Ok(())
}

fn is_loopback(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => v4.is_loopback(),
        IpAddr::V6(v6) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
    }
}

/// Resolve when the process receives Ctrl+C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutdown requested");
}

/// Run the HTTP server until Ctrl+C / SIGTERM.
pub async fn serve_http(controller: Controller, config: &Config) -> anyhow::Result<()> {
    check_bind_policy(&config.server)?;
    let app = app(controller, &config.server);
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr}"))?;
    tracing::info!(%addr, auth = config.server.token.is_some(), "HTTP server listening (REST API + MCP)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_loopback_requires_token() {
        let mut s = ServerConfig::default();
        assert!(check_bind_policy(&s).is_ok());
        s.host = "0.0.0.0".to_string();
        assert!(check_bind_policy(&s).is_err());
        s.token = Some("t".to_string());
        assert!(check_bind_policy(&s).is_ok());
        s.token = None;
        s.host = "::1".to_string();
        assert!(check_bind_policy(&s).is_ok());
    }
}
