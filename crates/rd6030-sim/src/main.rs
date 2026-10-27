//! `rd6030-sim` — run a simulated RD6030 on a pseudo-terminal.
//!
//! ```sh
//! rd6030-sim --link /tmp/rd6030 --load-ohms 10
//! rd6030-cli --port /tmp/rd6030 info
//! ```

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use clap::Parser;
use rd6030_sim::{serve, SimDevice};
use tokio_serial::SerialPort as _;

#[derive(Parser)]
#[command(
    name = "rd6030-sim",
    version,
    about = "Simulated Riden RD6030 on a PTY"
)]
struct Cli {
    /// Create a symlink to the PTY at this path (e.g. /tmp/rd6030).
    #[arg(long)]
    link: Option<PathBuf>,
    /// Resistive load on the output in ohms (omit for an open output).
    #[arg(long)]
    load_ohms: Option<f32>,
    /// Cycle through these loads in ohms (e.g. `10,5,2,1`) to exercise
    /// CV/CC transitions; overrides --load-ohms.
    #[arg(long, value_delimiter = ',')]
    load_cycle: Vec<f32>,
    /// Seconds per step of --load-cycle.
    #[arg(long, default_value_t = 3.0)]
    cycle_secs: f32,
    /// Modbus slave id to answer to.
    #[arg(long, default_value_t = 1)]
    slave_id: u8,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
    let cli = Cli::parse();

    let mut device = SimDevice::new();
    device.load_ohms = cli.load_ohms;
    let device = Arc::new(Mutex::new(device));

    if !cli.load_cycle.is_empty() {
        let device = Arc::clone(&device);
        let loads = cli.load_cycle.clone();
        let step = std::time::Duration::from_secs_f32(cli.cycle_secs.max(0.1));
        tokio::spawn(async move {
            for ohms in loads.iter().cycle() {
                device.lock().unwrap().load_ohms = Some(*ohms);
                tokio::time::sleep(step).await;
            }
        });
    }

    let (master, slave) = tokio_serial::SerialStream::pair().context("open PTY pair")?;
    let path = slave.name().context("PTY has no name")?;
    // Keep the PTY alive across client reconnects with a plain file handle:
    // the serialport handle holds an advisory flock that would prevent the
    // client from opening the port exclusively.
    let keepalive = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("open {path}"))?;
    drop(slave);

    if let Some(link) = &cli.link {
        let _ = std::fs::remove_file(link);
        std::os::unix::fs::symlink(&path, link)
            .with_context(|| format!("create symlink {}", link.display()))?;
    }
    // The PTY path on stdout (machine-readable), details on stderr.
    println!(
        "{}",
        cli.link
            .as_ref()
            .map_or(path.clone(), |l| l.display().to_string())
    );
    tracing::info!(pty = %path, load_ohms = ?cli.load_ohms, "RD6030 simulator running (Ctrl+C to stop)");

    let result = tokio::select! {
        r = serve(master, device, cli.slave_id) => r.context("serve"),
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    drop(keepalive);
    if let Some(link) = &cli.link {
        let _ = std::fs::remove_file(link);
    }
    result
}
