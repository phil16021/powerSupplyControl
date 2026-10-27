//! Model Context Protocol (MCP) server exposing the RD6030 as a set of tools.
//!
//! The server can run over two transports:
//! * **stdio** — for Claude Desktop / IDEs ([`serve_stdio`]).
//! * **streamable HTTP** — mounted on an axum router ([`http_router`]).
//!
//! All setters go through [`Controller`], so the configured software limits
//! apply. Calibration and raw register access are deliberately not exposed,
//! and preset-bank writes (EEPROM wear) are not offered either.

use std::sync::Arc;
use std::time::Duration;

use rd6030_core::{Controller, DeviceError, Protection, Setpoint};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{ServerCapabilities, ServerConfig};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::{schemars, tool, tool_handler, tool_router, ServerHandler, ServiceExt};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// Tool result: JSON text on success, an error message (`is_error = true`)
/// on failure.
type ToolResult = Result<String, String>;

/// MCP server state: the controller plus the generated tool router.
#[derive(Debug, Clone)]
pub struct McpServer {
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
        description = "Read live output measurements: voltage, current, power, input voltage, CV/CC regulation mode, protection status, output state, sample timestamp and sequence number."
    )]
    async fn get_telemetry(&self) -> ToolResult {
        json_result(self.controller.telemetry_sample().await)
    }

    #[tool(
        description = "Read device identity: model id, serial number, firmware version and internal temperatures (C and F)."
    )]
    async fn get_device_info(&self) -> ToolResult {
        json_result(self.controller.info().await)
    }

    #[tool(
        description = "Read the serial link state (connected/degraded/disconnected) and driver statistics."
    )]
    async fn get_status(&self) -> ToolResult {
        to_json(&self.controller.link())
    }

    #[tool(description = "Read the configured software limits for voltage and current.")]
    async fn get_limits(&self) -> ToolResult {
        let l = self.controller.limits();
        to_json(&json!({ "max_voltage": l.max_voltage, "max_current": l.max_current }))
    }

    #[tool(description = "Enable or disable the power supply output.")]
    async fn set_output(&self, Parameters(req): Parameters<SetOutputRequest>) -> ToolResult {
        json_result(
            self.controller
                .set_output(req.enabled)
                .await
                .map(|()| json!({ "output_enabled": req.enabled })),
        )
    }

    #[tool(description = "Read the active voltage and current setpoints.")]
    async fn get_setpoint(&self) -> ToolResult {
        json_result(self.controller.setpoint().await)
    }

    #[tool(
        description = "Set the output voltage setpoint in volts (0 up to the configured limit, at most 60)."
    )]
    async fn set_voltage(&self, Parameters(req): Parameters<SetVoltageRequest>) -> ToolResult {
        json_result(
            self.controller
                .set_voltage(req.voltage as f32)
                .await
                .map(|()| json!({ "voltage": req.voltage })),
        )
    }

    #[tool(
        description = "Set the output current setpoint in amperes (0 up to the configured limit, at most 30)."
    )]
    async fn set_current(&self, Parameters(req): Parameters<SetCurrentRequest>) -> ToolResult {
        json_result(
            self.controller
                .set_current(req.current as f32)
                .await
                .map(|()| json!({ "current": req.current })),
        )
    }

    #[tool(description = "Set voltage and current setpoints together in one atomic write.")]
    async fn set_setpoint(&self, Parameters(req): Parameters<SetSetpointRequest>) -> ToolResult {
        let sp = Setpoint {
            voltage: req.voltage as f32,
            current: req.current as f32,
        };
        json_result(self.controller.set_setpoint(sp).await.map(|()| sp))
    }

    #[tool(
        description = "Ramp the voltage setpoint gradually to a target (step in volts, interval_ms >= 50 between steps)."
    )]
    async fn ramp_voltage(&self, Parameters(req): Parameters<RampRequest>) -> ToolResult {
        json_result(
            self.controller
                .ramp_voltage(
                    req.target as f32,
                    req.step as f32,
                    Duration::from_millis(req.interval_ms),
                )
                .await
                .map(|()| json!({ "voltage": req.target })),
        )
    }

    #[tool(description = "Read the active over-voltage / over-current protection thresholds.")]
    async fn get_protection(&self) -> ToolResult {
        json_result(self.controller.protection().await)
    }

    #[tool(
        description = "Set the active over-voltage (V) and over-current (A) protection thresholds. Persists to device EEPROM: do not call repeatedly."
    )]
    async fn set_protection(
        &self,
        Parameters(req): Parameters<SetProtectionRequest>,
    ) -> ToolResult {
        let p = Protection {
            ovp: req.ovp as f32,
            ocp: req.ocp as f32,
        };
        json_result(self.controller.set_protection(p).await.map(|()| p))
    }

    #[tool(description = "Read one stored preset slot (0..=9): voltage, current, OVP, OCP.")]
    async fn read_preset(&self, Parameters(req): Parameters<SlotRequest>) -> ToolResult {
        json_result(self.controller.read_preset(req.slot).await)
    }

    #[tool(description = "Recall a stored preset slot (0..=9, i.e. M0..M9).")]
    async fn select_preset(&self, Parameters(req): Parameters<SlotRequest>) -> ToolResult {
        json_result(
            self.controller
                .select_preset(req.slot)
                .await
                .map(|()| json!({ "slot": req.slot })),
        )
    }

    #[tool(description = "Read battery charging mode status, capacity and energy.")]
    async fn get_battery(&self) -> ToolResult {
        json_result(self.controller.battery_status().await)
    }

    #[tool(description = "Read the device real-time clock.")]
    async fn get_rtc(&self) -> ToolResult {
        json_result(self.controller.read_rtc().await)
    }

    #[tool(description = "Synchronize the device real-time clock to the host clock.")]
    async fn sync_rtc(&self) -> ToolResult {
        json_result(self.controller.sync_rtc().await)
    }
}

// `#[tool_handler]` wires list_tools/call_tool to the generated router;
// without it the server advertises no tools at all.
#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "Controls a Riden RD6030 programmable DC power supply (60 V / 30 A). \
             Use get_telemetry/get_device_info/get_status to read state and \
             set_voltage/set_current/set_setpoint/set_output to change it. \
             Setpoints are limited by get_limits. Before enabling the output, \
             check the setpoints and protection thresholds.",
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
    #[schemars(description = "Target voltage in volts (0.0..=60.0, subject to limits).")]
    pub voltage: f64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetCurrentRequest {
    #[schemars(description = "Target current in amperes (0.0..=30.0, subject to limits).")]
    pub current: f64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetSetpointRequest {
    #[schemars(description = "Voltage setpoint in volts.")]
    pub voltage: f64,
    #[schemars(description = "Current setpoint in amperes.")]
    pub current: f64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct RampRequest {
    #[schemars(description = "Target voltage in volts.")]
    pub target: f64,
    #[schemars(description = "Maximum change per step in volts (>= 0.01).")]
    pub step: f64,
    #[schemars(description = "Delay between steps in milliseconds (>= 50).")]
    pub interval_ms: u64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SetProtectionRequest {
    #[schemars(description = "Over-voltage protection threshold in volts.")]
    pub ovp: f64,
    #[schemars(description = "Over-current protection threshold in amperes.")]
    pub ocp: f64,
}

#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
pub struct SlotRequest {
    #[schemars(description = "Preset slot number 0..=9 (M0..M9).")]
    pub slot: u8,
}

fn to_json<T: Serialize>(value: &T) -> ToolResult {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

/// Serialize a device result for a tool reply.
fn json_result<T: Serialize>(result: Result<T, DeviceError>) -> ToolResult {
    to_json(&result.map_err(|e| e.to_string())?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_plain_messages() {
        let r: ToolResult = json_result::<()>(Err(DeviceError::OutOfRange(
            "voltage \"70\" too high".to_string(),
        )));
        assert_eq!(
            r.unwrap_err(),
            "value out of range: voltage \"70\" too high"
        );
        assert_eq!(
            json_result(Ok(json!({"a": 1}))).unwrap(),
            "{\n  \"a\": 1\n}"
        );
    }
}
