//! `rd6030-cli` — command-line interface for the Riden RD6030.

use std::path::PathBuf;
use std::process::Command as ProcessCommand;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rd6030_core::config::{Config, LogFormat, LoggingConfig};
use rd6030_core::logging::TelemetryLogger;
use rd6030_core::{Controller, DeviceTelemetry, RtcTime, Setpoint};

#[derive(Parser)]
#[command(
    name = "rd6030",
    version,
    about = "Control the Riden RD6030 programmable DC power supply"
)]
struct Cli {
    /// Path to the configuration file.
    #[arg(long, global = true, default_value = "config.toml")]
    config: PathBuf,

    /// Serial port override (e.g. /dev/ttyUSB0, COM3).
    #[arg(long, global = true)]
    port: Option<String>,

    /// Baud rate override (9600..=115200).
    #[arg(long, global = true)]
    baud: Option<u32>,

    /// Modbus slave address override (1..=247).
    #[arg(long, global = true)]
    slave: Option<u8>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show device identity, serial number, firmware and temperatures.
    Info,
    /// Read one telemetry snapshot, or watch live values with --watch.
    Telemetry {
        /// Continuously print telemetry until Ctrl+C.
        #[arg(long)]
        watch: bool,
    },
    /// Set the voltage and/or current setpoint.
    Set {
        /// Voltage setpoint in volts.
        #[arg(long, value_name = "VOLTS")]
        voltage: Option<f32>,
        /// Current setpoint in amperes.
        #[arg(long, value_name = "AMPS")]
        current: Option<f32>,
    },
    /// Enable or disable the output.
    Output {
        #[arg(value_enum)]
        state: OutputState,
    },
    /// Manage preset slots and the active setpoint.
    Preset {
        #[command(subcommand)]
        command: PresetCommand,
    },
    /// Read or set the device real-time clock.
    Rtc {
        #[command(subcommand)]
        command: RtcCommand,
    },
    /// Read the battery charging status.
    Battery,
    /// Calibration operations (dangerous — writes NVRAM).
    Calibrate {
        #[command(subcommand)]
        command: CalibrateCommand,
    },
    /// Record telemetry to a file.
    Log {
        #[command(subcommand)]
        command: LogCommand,
    },
    /// Run the HTTP server (REST API + MCP).
    Serve,
    /// Run the MCP server over stdio.
    Mcp,
    /// Launch the GUI.
    Gui,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputState {
    On,
    Off,
}

#[derive(Subcommand)]
enum PresetCommand {
    /// Select a preset slot (0..=9, corresponding to M0..M9).
    Select { slot: u8 },
    /// Show the active slot and setpoint.
    Get,
    /// Set the active (volatile) setpoint.
    Set { voltage: f32, current: f32 },
}

#[derive(Subcommand)]
enum RtcCommand {
    /// Print the device RTC.
    Get,
    /// Set the device RTC explicitly.
    Set {
        year: u16,
        month: u8,
        day: u8,
        hour: u8,
        minute: u8,
        second: u8,
    },
    /// Synchronize the device RTC to the host clock.
    Sync,
}

#[derive(Subcommand)]
enum CalibrateCommand {
    /// Commit pending calibration changes to NVRAM (dangerous).
    Commit {
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Read a raw calibration register (address >= 55).
    Read { addr: u16 },
    /// Write a raw calibration register (dangerous).
    Write {
        addr: u16,
        value: u16,
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum LogCommand {
    /// Start logging until Ctrl+C.
    Start {
        /// Output file (.csv or .jsonl).
        #[arg(long, default_value = "rd6030-telemetry.csv")]
        path: PathBuf,
        /// Sample interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
    /// Hint: stop logging by sending Ctrl+C to the `log start` process.
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let cli = Cli::parse();

    // The GUI is a separate binary with its own config handling.
    if matches!(&cli.command, Command::Gui) {
        return dispatch("rd6030-gui");
    }

    let config = resolve_config(&cli)?;
    let controller = Controller::connect(&config)?;

    match cli.command {
        Command::Info => cmd_info(&controller).await,
        Command::Telemetry { watch } => cmd_telemetry(&controller, watch).await,
        Command::Set { voltage, current } => cmd_set(&controller, voltage, current).await,
        Command::Output { state } => cmd_output(&controller, state).await,
        Command::Preset { command } => cmd_preset(&controller, command).await,
        Command::Rtc { command } => cmd_rtc(&controller, command).await,
        Command::Battery => cmd_battery(&controller).await,
        Command::Calibrate { command } => cmd_calibrate(&controller, command).await,
        Command::Log { command } => cmd_log(&controller, command).await,
        Command::Serve => rd6030_server::http::serve_http(controller, &config).await,
        Command::Mcp => rd6030_server::mcp::serve_stdio(controller).await,
        Command::Gui => unreachable!("handled above"),
    }
}

fn resolve_config(cli: &Cli) -> Result<Config> {
    let mut config = Config::load(&cli.config)?;
    if let Some(port) = &cli.port {
        config.port = port.clone();
    }
    if let Some(baud) = cli.baud {
        config.baud_rate = baud;
    }
    if let Some(slave) = cli.slave {
        config.slave_id = slave;
    }
    config.validate().map_err(anyhow::Error::from)?;
    Ok(config)
}

async fn cmd_info(c: &Controller) -> Result<()> {
    let info = c.info().await?;
    println!("Model:         {}", info.model);
    println!("Serial number: {}", info.serial_number);
    println!("Firmware:      {:.2}", info.firmware);
    println!(
        "Temperature:   {:.1} °C / {:.1} °F",
        info.temperature_c, info.temperature_f
    );
    Ok(())
}

async fn cmd_telemetry(c: &Controller, watch: bool) -> Result<()> {
    if watch {
        let mut rx = c.subscribe();
        println!(
            "{:>10} {:>10} {:>10} {:>10}  {:>4}  {:>3}",
            "Vout(V)", "Iout(A)", "Pout(W)", "Vin(V)", "Mode", "Out"
        );
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                changed = rx.changed() => {
                    if changed.is_err() {
                        bail!("telemetry stream closed");
                    }
                    let t = *rx.borrow_and_update();
                    print_telemetry(&t);
                }
            }
        }
    } else {
        let t = c.telemetry().await?;
        print_telemetry(&t);
    }
    Ok(())
}

fn print_telemetry(t: &DeviceTelemetry) {
    let mode = if t.constant_current { "CC" } else { "CV" };
    let out = if t.output_enabled { "ON" } else { "OFF" };
    println!(
        "{:>10.3} {:>10.3} {:>10.3} {:>10.3}  {:>4}  {:>3}",
        t.voltage_out, t.current_out, t.power_out, t.voltage_in, mode, out
    );
}

async fn cmd_set(c: &Controller, voltage: Option<f32>, current: Option<f32>) -> Result<()> {
    match (voltage, current) {
        (None, None) => bail!("specify at least one of --voltage or --current"),
        (Some(v), None) => {
            c.set_voltage(v).await?;
            println!("Voltage set to {v:.2} V");
        }
        (None, Some(a)) => {
            c.set_current(a).await?;
            println!("Current set to {a:.2} A");
        }
        (Some(v), Some(a)) => {
            c.set_setpoint(Setpoint {
                voltage: v,
                current: a,
            })
            .await?;
            println!("Setpoint set to {v:.2} V / {a:.2} A");
        }
    }
    Ok(())
}

async fn cmd_output(c: &Controller, state: OutputState) -> Result<()> {
    let enable = matches!(state, OutputState::On);
    c.set_output(enable).await?;
    println!("Output {}", if enable { "enabled" } else { "disabled" });
    Ok(())
}

async fn cmd_preset(c: &Controller, command: PresetCommand) -> Result<()> {
    match command {
        PresetCommand::Select { slot } => {
            c.select_preset(slot).await?;
            println!("Selected preset M{slot}");
        }
        PresetCommand::Get => {
            let slot = c.active_preset().await?;
            let sp = c.setpoint().await?;
            println!("Active preset: M{slot}");
            println!("Setpoint:      {:.2} V / {:.2} A", sp.voltage, sp.current);
        }
        PresetCommand::Set { voltage, current } => {
            c.set_setpoint(Setpoint { voltage, current }).await?;
            println!("Setpoint set to {voltage:.2} V / {current:.2} A");
        }
    }
    Ok(())
}

async fn cmd_rtc(c: &Controller, command: RtcCommand) -> Result<()> {
    match command {
        RtcCommand::Get => {
            let t = c.read_rtc().await?;
            println!("Device RTC: {t}");
        }
        RtcCommand::Set {
            year,
            month,
            day,
            hour,
            minute,
            second,
        } => {
            let t = RtcTime {
                year,
                month,
                day,
                hour,
                minute,
                second,
            };
            c.set_rtc(t).await?;
            println!("RTC set to {t}");
        }
        RtcCommand::Sync => {
            let t = c.sync_rtc().await?;
            println!("RTC synchronized to {t}");
        }
    }
    Ok(())
}

async fn cmd_battery(c: &Controller) -> Result<()> {
    let b = c.battery_status().await?;
    println!(
        "Charging:    {}",
        if b.charging { "active" } else { "inactive" }
    );
    println!("Voltage:     {:.2} V", b.voltage);
    println!(
        "Temperature: {:.1} °C / {:.1} °F",
        b.temperature_c, b.temperature_f
    );
    println!("Capacity:    {:.3} Ah", b.capacity_ah);
    println!("Energy:      {:.3} Wh", b.energy_wh);
    Ok(())
}

async fn cmd_calibrate(c: &Controller, command: CalibrateCommand) -> Result<()> {
    match command {
        CalibrateCommand::Commit { yes } => {
            if !yes {
                bail!("calibration commit writes NVRAM permanently; pass --yes to confirm");
            }
            c.commit_calibration().await?;
            println!("Calibration committed to NVRAM");
        }
        CalibrateCommand::Read { addr } => {
            let value = c.read_calibration_register(addr).await?;
            println!("Register {addr}: {value} (0x{value:04X})");
        }
        CalibrateCommand::Write { addr, value, yes } => {
            if !yes {
                bail!("writing calibration registers is dangerous; pass --yes to confirm");
            }
            c.write_calibration_register(addr, value).await?;
            println!("Wrote {value} to register {addr}");
        }
    }
    Ok(())
}

async fn cmd_log(c: &Controller, command: LogCommand) -> Result<()> {
    match command {
        LogCommand::Start { path, interval_ms } => {
            let format = match path.extension().and_then(|e| e.to_str()) {
                Some("jsonl") => LogFormat::JsonLines,
                _ => LogFormat::Csv,
            };
            let log_config = LoggingConfig {
                enabled: true,
                path: path.to_string_lossy().into_owned(),
                format,
                interval_ms,
            };
            let logger = TelemetryLogger::start(c, &log_config).await?;
            println!("Logging to {} (Ctrl+C to stop)", log_config.path);
            tokio::signal::ctrl_c().await?;
            logger.stop().await;
            println!("Logging stopped");
        }
        LogCommand::Stop => {
            println!("Stop logging by sending Ctrl+C to the `log start` process.");
        }
    }
    Ok(())
}

/// Run a sibling binary from the same directory as the CLI executable.
fn dispatch(bin: &str) -> Result<()> {
    let exe = std::env::current_exe().context("determine executable path")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?;
    let path = if std::env::consts::EXE_EXTENSION.is_empty() {
        dir.join(bin)
    } else {
        dir.join(format!("{bin}.{}", std::env::consts::EXE_EXTENSION))
    };
    if !path.exists() {
        bail!(
            "{bin} binary not found at {} (build it with `cargo build --workspace`)",
            path.display()
        );
    }
    let status = ProcessCommand::new(&path)
        .status()
        .with_context(|| format!("spawn {bin}"))?;
    if !status.success() {
        bail!("{bin} exited with {status}");
    }
    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).try_init();
}
