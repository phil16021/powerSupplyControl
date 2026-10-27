//! `rd6030-server` binary — REST API and MCP server entry point.

use clap::{Parser, Subcommand};
use rd6030_core::config::Config;
use rd6030_core::Controller;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "rd6030-server",
    version,
    about = "REST API and MCP server for the Riden RD6030"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, default_value = "config.toml")]
    config: String,

    /// Serial port override (e.g. /dev/ttyUSB0, COM3).
    #[arg(long)]
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
    init_tracing();
    let cli = Cli::parse();

    let mut config = Config::load(&cli.config)?;
    if let Some(port) = cli.port {
        config.port = port;
    }

    let controller = Controller::connect(&config)?;

    match cli.command {
        Command::Mcp => rd6030_server::mcp::serve_stdio(controller).await,
        Command::Serve => rd6030_server::http::serve_http(controller, &config).await,
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
