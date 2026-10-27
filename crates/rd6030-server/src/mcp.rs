//! Model Context Protocol (MCP) server exposing the RD6030 as a set of tools.
//!
//! The server can run over two transports:
//! * **stdio** — for Claude Desktop / IDEs ([`serve_stdio`]).
//! * **streamable HTTP** — mounted on an axum router ([`http_router`]).

use std::sync::Arc;

use rd6030_core::{Controller, DeviceError};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_router, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};

/// MCP server state: the controller plus the generated tool router.
#[derive(Debug, Clone)]
pub struct McpServer {
    #[allow(dead_code)] // held for the generated `#[tool_router]` dispatch
    tool_router: ToolRouter<Self>,
    controller: Controller,
}

impl McpServer {
    /// Create a server for the given controller.
    pub fn new(controller: Controller) -> Self {
        Self {
            tool_router: Self::tool_router(),
            controller,
        }
    }
}

#[tool_router]
impl McpServer {
    #[tool(
        description = "Read live output measurements: voltage, current, power, input voltage, CV/CC regulation mode, protection status and output state."
    )]
    async fn get_telemetry(&self) -> String {
        json_result(self.controller.telemetry().await)
    }

    #[tool(
        description = "Read device identity: model id, serial number, firmware version and internal temperatures (C and F)."
    )]
    async fn get_info(&self) -> String {
        json_result(self.controller.info().await)
    }

    #[tool(description = "Enable or disable the power supply output.")]
    async fn set_output(&self, Parameters(req): Parameters<SetOutputRequest>) -> String {
        json_result(self.controller.set_output(req.enabled).await.map(|_| "ok"))
    }

    #[tool(description = "Set the output voltage setpoint in volts (0.0 to 60.0).")]
    async fn set_voltage(&self, Parameters(req): Parameters<SetVoltageRequest>) -> String {
        json_result(
            self.controller
                .set_voltage(req.voltage as f32)
                .await
                .map(|_| "ok"),
        )
    }

    #[tool(description = "Set the output current setpoint in amperes (0.0 to 30.0).")]
    async fn set_current(&self, Parameters(req): Parameters<SetCurrentRequest>) -> String {
        json_result(
            self.controller
                .set_current(req.current as f32)
                .await
                .map(|_| "ok"),
        )
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls a Riden RD6030 programmable DC power supply. \
                 Use get_telemetry/get_info to read state and \
                 set_voltage/set_current/set_output to change it.",
        )
    }
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve_stdio(controller: Controller) -> anyhow::Result<()> {
    let server = McpServer::new(controller);
    let running = server.serve(rmcp::transport::stdio()).await?;
    running.waiting().await?;
    Ok(())
}

/// Build an axum router with the streamable-HTTP MCP endpoint mounted at `/mcp`.
pub fn http_router(controller: Controller) -> axum::Router {
    let service = StreamableHttpService::new(
        move || Ok::<McpServer, std::io::Error>(McpServer::new(controller.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new().nest_service("/mcp", service)
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetOutputRequest {
    #[schemars(description = "Whether the output should be enabled.")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetVoltageRequest {
    #[schemars(description = "Target voltage in volts (0.0..=60.0).")]
    pub voltage: f64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetCurrentRequest {
    #[schemars(description = "Target current in amperes (0.0..=30.0).")]
    pub current: f64,
}

/// Serialize a `Result` as a JSON object (or `{"error": ...}`) for a tool reply.
fn json_result<T: Serialize>(result: Result<T, DeviceError>) -> String {
    match result {
        Ok(value) => serde_json::to_string_pretty(&value)
            .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}")),
        Err(err) => format!("{{\"error\":\"{err}\"}}"),
    }
}
