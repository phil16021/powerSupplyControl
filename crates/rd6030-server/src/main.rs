//! `rd6030-server` binary — REST API and MCP server entry point.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use rd6030_core::config::Config;

#[derive(Parser)]
#[command(
    name = "rd6030-server",
    version,
    about = "REST API and MCP server for the Riden RD6030"
)]
struct Cli {
    /// Path to the configuration file (default: $RD6030_CONFIG,
    /// ./config.toml, $XDG_CONFIG_HOME/rd6030/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Serial port override (e.g. /dev/ttyUSB0, COM3).
    #[arg(long, global = true)]
    port: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the MCP server over stdio (Claude Desktop / IDEs).
    Mcp,
    /// Run the HTTP server (REST API + streamable-HTTP MCP endpoint).
    Serve,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rd6030_server::init_tracing("info");
    let cli = Cli::parse();

    let (mut config, source) = Config::discover(cli.config.as_deref())?;
    if let Some(port) = cli.port {
        config.port = port;
    }
    config.validate()?;
    tracing::debug!(?source, "configuration loaded");

    match cli.command {
        Command::Mcp => rd6030_server::run_mcp_stdio(&config).await,
        Command::Serve => rd6030_server::run_serve(&config).await,
    }
}
