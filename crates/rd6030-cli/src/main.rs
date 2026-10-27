//! `rd6030-cli` — command-line interface for the Riden RD6030.

use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use clap::{Parser, Subcommand, ValueEnum};
use rd6030_core::config::{Config, LogFormat, LoggingConfig};
use rd6030_core::logging::TelemetryLogger;
use rd6030_core::{
    Controller, DriverOptions, Preset, PresetBank, Protection, RtcTime, Setpoint, TelemetrySample,
};
use serde::Serialize;
use serde_json::json;

#[derive(Parser)]
#[command(
    name = "rd6030-cli",
    version,
    about = "Control the Riden RD6030 programmable DC power supply"
)]
struct Cli {
    /// Configuration file (default: $RD6030_CONFIG, ./config.toml,
    /// $XDG_CONFIG_HOME/rd6030/config.toml, then built-in defaults).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Serial port override (e.g. /dev/ttyUSB0, /dev/rd6030).
    #[arg(long, global = true)]
    port: Option<String>,

    /// Baud rate override (9600..=115200).
    #[arg(long, global = true)]
    baud: Option<u32>,

    /// Modbus slave address override (1..=247).
    #[arg(long, global = true)]
    slave: Option<u8>,

    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show device identity, serial number, firmware and temperatures.
    Info,
    /// Show link state, identity, setpoints and a telemetry snapshot.
    Status,
    /// List serial ports (CH340 adapters are marked).
    Ports,
    /// Read one telemetry snapshot, or watch live values with --watch.
    Telemetry {
        /// Continuously print telemetry until Ctrl+C.
        #[arg(long)]
        watch: bool,
        /// Stop after this many samples (with --watch).
        #[arg(long)]
        count: Option<u64>,
    },
    /// Set the voltage and/or current setpoint (volatile).
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
        state: OnOff,
    },
    /// Ramp the voltage or current setpoint gradually.
    Ramp {
        /// Target voltage in volts.
        #[arg(long, conflicts_with = "current", required_unless_present = "current")]
        voltage: Option<f32>,
        /// Target current in amperes.
        #[arg(long)]
        current: Option<f32>,
        /// Maximum change per step (V or A).
        #[arg(long)]
        step: f32,
        /// Delay between steps in milliseconds (>= 50).
        #[arg(long, default_value_t = 100)]
        interval_ms: u64,
    },
    /// Read or set the over-voltage / over-current protection thresholds.
    Protection {
        #[command(subcommand)]
        command: ProtectionCommand,
    },
    /// Lock or unlock the front-panel keypad.
    Lock {
        #[arg(value_enum)]
        state: OnOff,
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
    /// Read or change system options (buzzer, backlight).
    System {
        #[command(subcommand)]
        command: SystemCommand,
    },
    /// Calibration operations (dangerous — writes NVRAM).
    Calibrate {
        #[command(subcommand)]
        command: CalibrateCommand,
    },
    /// Raw register access for debugging.
    Raw {
        #[command(subcommand)]
        command: RawCommand,
    },
    /// Record telemetry to a file until Ctrl+C.
    Log {
        /// Output file (.csv, or .jsonl/.json for JSON Lines).
        #[arg(long, default_value = "rd6030-telemetry.csv")]
        path: PathBuf,
        /// Sample interval in milliseconds.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
        /// Append to an existing file.
        #[arg(long)]
        append: bool,
    },
    /// Run the HTTP server (REST API + MCP).
    Serve,
    /// Run the MCP server over stdio.
    Mcp,
    /// Launch the GUI.
    Gui,
}

#[derive(Clone, Copy, ValueEnum)]
enum OnOff {
    On,
    Off,
}

impl OnOff {
    fn as_bool(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Subcommand)]
enum ProtectionCommand {
    /// Show the active OVP/OCP thresholds.
    Get,
    /// Set the active OVP/OCP thresholds (stored in EEPROM).
    Set {
        /// Over-voltage threshold in volts.
        #[arg(long)]
        ovp: f32,
        /// Over-current threshold in amperes.
        #[arg(long)]
        ocp: f32,
    },
}

#[derive(Subcommand)]
enum PresetCommand {
    /// Recall a preset slot (0..=9, corresponding to M0..M9).
    Select { slot: u8 },
    /// Show the active slot and setpoint.
    Get,
    /// Set the active (volatile) setpoint.
    Set { voltage: f32, current: f32 },
    /// Show a stored preset slot.
    Read { slot: u8 },
    /// Store a preset slot (EEPROM write).
    Write {
        slot: u8,
        #[arg(long)]
        voltage: f32,
        #[arg(long)]
        current: f32,
        #[arg(long)]
        ovp: f32,
        #[arg(long)]
        ocp: f32,
    },
    /// Export all presets as JSON (stdout or --file).
    Export {
        #[arg(long)]
        file: Option<PathBuf>,
    },
    /// Import presets from JSON, writing only slots that change.
    Import { file: PathBuf },
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
enum SystemCommand {
    /// Show the system options.
    Get,
    /// Enable or disable the key beep.
    Buzzer {
        #[arg(value_enum)]
        state: OnOff,
    },
    /// Set the backlight level (0..=5).
    Backlight { level: u8 },
}

#[derive(Subcommand)]
enum CalibrateCommand {
    /// Commit pending calibration changes to NVRAM (dangerous, unverified).
    Commit {
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Read calibration registers (all of 55..=62, or one address).
    Read { addr: Option<u16> },
    /// Write a raw calibration register 55..=62 (dangerous).
    Write {
        addr: u16,
        value: u16,
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand)]
enum RawCommand {
    /// Read holding registers.
    Read {
        addr: u16,
        #[arg(default_value_t = 1)]
        count: u16,
    },
    /// Write holding registers (documented writable registers only).
    Write {
        addr: u16,
        #[arg(required = true)]
        values: Vec<u16>,
        /// Required confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    rd6030_server::init_tracing("warn");
    let cli = Cli::parse();

    match &cli.command {
        // The GUI is a separate binary; forward the connection options.
        Command::Gui => return dispatch("rd6030-gui", &gui_args(&cli)),
        Command::Ports => return cmd_ports(cli.json),
        _ => {}
    }

    let config = resolve_config(&cli)?;
    let out = Output { json: cli.json };

    match cli.command {
        Command::Serve => return rd6030_server::run_serve(&config).await,
        Command::Mcp => return rd6030_server::run_mcp_stdio(&config).await,
        _ => {}
    }

    // One-shot commands do not need background polling.
    let polling = matches!(
        cli.command,
        Command::Telemetry { watch: true, .. } | Command::Log { .. }
    );
    let mut options = DriverOptions::from_config(&config);
    if !polling {
        options = options.without_polling();
    }
    let controller = Controller::connect_with(&config, options)
        .await
        .with_context(|| format!("connect to RD6030 on {}", config.port))?;

    let result = run(&controller, &config, cli.command, out).await;
    controller.shutdown().await;
    result
}

async fn run(c: &Controller, config: &Config, command: Command, out: Output) -> Result<()> {
    match command {
        Command::Info => {
            let info = c.info().await?;
            out.emit(&info, || {
                format!(
                    "Model:         {} (raw {})\nSerial number: {}\nFirmware:      {:.2}\nTemperature:   {:.1} °C / {:.1} °F",
                    model_name(info.model),
                    info.model,
                    info.serial_number,
                    info.firmware,
                    info.temperature_c,
                    info.temperature_f
                )
            })
        }
        Command::Status => cmd_status(c, out).await,
        Command::Telemetry { watch, count } => cmd_telemetry(c, watch, count, out).await,
        Command::Set { voltage, current } => cmd_set(c, voltage, current, out).await,
        Command::Output { state } => {
            c.set_output(state.as_bool()).await?;
            out.emit(&json!({ "output_enabled": state.as_bool() }), || {
                format!(
                    "Output {}",
                    if state.as_bool() {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )
            })
        }
        Command::Ramp {
            voltage,
            current,
            step,
            interval_ms,
        } => {
            let interval = Duration::from_millis(interval_ms);
            match (voltage, current) {
                (Some(v), _) => c.ramp_voltage(v, step, interval).await?,
                (None, Some(a)) => c.ramp_current(a, step, interval).await?,
                (None, None) => bail!("specify --voltage or --current"),
            }
            let sp = c.setpoint().await?;
            out.emit(&sp, || {
                format!("Setpoint now {:.2} V / {:.2} A", sp.voltage, sp.current)
            })
        }
        Command::Protection { command } => cmd_protection(c, command, out).await,
        Command::Lock { state } => {
            c.set_keypad_lock(state.as_bool()).await?;
            out.emit(&json!({ "keypad_locked": state.as_bool() }), || {
                format!(
                    "Keypad {}",
                    if state.as_bool() {
                        "locked"
                    } else {
                        "unlocked"
                    }
                )
            })
        }
        Command::Preset { command } => cmd_preset(c, command, out).await,
        Command::Rtc { command } => cmd_rtc(c, command, out).await,
        Command::Battery => {
            let b = c.battery_status().await?;
            out.emit(&b, || {
                format!(
                    "Charging:    {}\nVoltage:     {:.2} V\nTemperature: {:.1} °C / {:.1} °F\nCapacity:    {:.3} Ah\nEnergy:      {:.3} Wh",
                    if b.charging { "active" } else { "inactive" },
                    b.voltage,
                    b.temperature_c,
                    b.temperature_f,
                    b.capacity_ah,
                    b.energy_wh
                )
            })
        }
        Command::System { command } => cmd_system(c, command, out).await,
        Command::Calibrate { command } => cmd_calibrate(c, command, out).await,
        Command::Raw { command } => cmd_raw(c, command, out).await,
        Command::Log {
            path,
            interval_ms,
            append,
        } => cmd_log(c, config, &path, interval_ms, append).await,
        Command::Serve | Command::Mcp | Command::Gui | Command::Ports => {
            unreachable!("handled in main")
        }
    }
}

/// Text or JSON output.
#[derive(Clone, Copy)]
struct Output {
    json: bool,
}

impl Output {
    fn emit<T: Serialize>(&self, value: &T, text: impl FnOnce() -> String) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string_pretty(value)?);
        } else {
            println!("{}", text());
        }
        Ok(())
    }
}

fn resolve_config(cli: &Cli) -> Result<Config> {
    let (mut config, source) = Config::discover(cli.config.as_deref())?;
    tracing::debug!(?source, "configuration loaded");
    if let Some(port) = &cli.port {
        config.port = port.clone();
    }
    if let Some(baud) = cli.baud {
        config.baud_rate = baud;
    }
    if let Some(slave) = cli.slave {
        config.slave_id = slave;
    }
    config.validate()?;
    Ok(config)
}

fn model_name(raw: u16) -> String {
    match rd6030_core::register::Model::from_raw(raw) {
        rd6030_core::register::Model::Rd6030 => "RD6030".to_string(),
        rd6030_core::register::Model::Other(raw) => format!("unknown ({raw})"),
    }
}

fn cmd_ports(as_json: bool) -> Result<()> {
    let ports = tokio_serial::available_ports().context("enumerate serial ports")?;
    let rows: Vec<_> = ports
        .iter()
        .map(|p| {
            let (usb, ch340) = match &p.port_type {
                tokio_serial::SerialPortType::UsbPort(u) => (
                    Some(format!(
                        "{:04x}:{:04x} {}",
                        u.vid,
                        u.pid,
                        u.product.clone().unwrap_or_default()
                    )),
                    u.vid == 0x1a86 && u.pid == 0x7523,
                ),
                _ => (None, false),
            };
            json!({ "port": p.port_name, "usb": usb, "ch340": ch340 })
        })
        .collect();
    Output { json: as_json }.emit(&rows, || {
        if rows.is_empty() {
            return "No serial ports found (is the RD6030 plugged in? see README troubleshooting)"
                .to_string();
        }
        rows.iter()
            .map(|r| {
                format!(
                    "{:<24} {}{}",
                    r["port"].as_str().unwrap_or_default(),
                    r["usb"].as_str().unwrap_or("-"),
                    if r["ch340"] == true {
                        "  <- CH340 (likely RD6030)"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    })
}

async fn cmd_status(c: &Controller, out: Output) -> Result<()> {
    let sample = c.telemetry_sample().await?;
    let setpoint = c.setpoint().await?;
    let protection = c.protection().await?;
    let link = c.link();
    let value = json!({
        "link": link,
        "setpoint": setpoint,
        "protection": protection,
        "telemetry": sample,
        "limits": { "max_voltage": c.limits().max_voltage, "max_current": c.limits().max_current },
    });
    out.emit(&value, || {
        let t = &sample.telemetry;
        let info = link
            .info
            .map(|i| {
                format!(
                    "{} #{} fw {:.2}",
                    model_name(i.model),
                    i.serial_number,
                    i.firmware
                )
            })
            .unwrap_or_else(|| "-".to_string());
        format!(
            "Link:       {:?} ({} requests, {} errors, {} timeouts)\nDevice:     {info}\n\
             Setpoint:   {:.2} V / {:.2} A   (limits {:.2} V / {:.2} A)\n\
             Protection: OVP {:.2} V / OCP {:.2} A   status {:?}\n\
             Output:     {}  {}  {:.2} V  {:.2} A  {:.2} W  (Vin {:.2} V)",
            link.state,
            link.requests,
            link.errors,
            link.timeouts,
            setpoint.voltage,
            setpoint.current,
            c.limits().max_voltage,
            c.limits().max_current,
            protection.ovp,
            protection.ocp,
            t.protection,
            if t.output_enabled { "ON " } else { "OFF" },
            if t.constant_current { "CC" } else { "CV" },
            t.voltage_out,
            t.current_out,
            t.power_out,
            t.voltage_in
        )
    })
}

async fn cmd_telemetry(c: &Controller, watch: bool, count: Option<u64>, out: Output) -> Result<()> {
    if !watch {
        let s = c.telemetry_sample().await?;
        if !out.json {
            print_header();
        }
        return print_sample(&s, out);
    }
    let mut rx = c.subscribe();
    if !out.json {
        print_header();
    }
    let mut printed = 0;
    let mut last_seq = 0;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            changed = rx.changed() => {
                if changed.is_err() {
                    bail!("telemetry stream closed");
                }
                let Some(s) = *rx.borrow_and_update() else { continue };
                if s.seq == last_seq {
                    continue;
                }
                last_seq = s.seq;
                print_sample(&s, out)?;
                printed += 1;
                if count.is_some_and(|n| printed >= n) {
                    break;
                }
            }
        }
    }
    Ok(())
}

fn print_header() {
    println!(
        "{:>10} {:>10} {:>10} {:>10}  {:>4}  {:>3}  {:>4}",
        "Vout(V)", "Iout(A)", "Pout(W)", "Vin(V)", "Mode", "Out", "Prot"
    );
}

fn print_sample(s: &TelemetrySample, out: Output) -> Result<()> {
    if out.json {
        println!("{}", serde_json::to_string(s)?);
        return Ok(());
    }
    let t = &s.telemetry;
    println!(
        "{:>10.2} {:>10.2} {:>10.2} {:>10.2}  {:>4}  {:>3}  {:>4}",
        t.voltage_out,
        t.current_out,
        t.power_out,
        t.voltage_in,
        if t.constant_current { "CC" } else { "CV" },
        if t.output_enabled { "ON" } else { "OFF" },
        format!("{:?}", t.protection).to_uppercase()
    );
    Ok(())
}

async fn cmd_set(
    c: &Controller,
    voltage: Option<f32>,
    current: Option<f32>,
    out: Output,
) -> Result<()> {
    match (voltage, current) {
        (None, None) => bail!("specify at least one of --voltage or --current"),
        (Some(v), None) => c.set_voltage(v).await?,
        (None, Some(a)) => c.set_current(a).await?,
        (Some(voltage), Some(current)) => c.set_setpoint(Setpoint { voltage, current }).await?,
    }
    let sp = c.setpoint().await?;
    out.emit(&sp, || {
        format!("Setpoint: {:.2} V / {:.2} A", sp.voltage, sp.current)
    })
}

async fn cmd_protection(c: &Controller, command: ProtectionCommand, out: Output) -> Result<()> {
    if let ProtectionCommand::Set { ovp, ocp } = command {
        c.set_protection(Protection { ovp, ocp }).await?;
    }
    let p = c.protection().await?;
    out.emit(&p, || format!("OVP: {:.2} V\nOCP: {:.2} A", p.ovp, p.ocp))
}

fn preset_line(slot: u8, p: &Preset) -> String {
    format!(
        "M{slot}: {:>6.2} V {:>6.2} A   OVP {:>6.2} V  OCP {:>6.2} A",
        p.voltage, p.current, p.ovp, p.ocp
    )
}

async fn cmd_preset(c: &Controller, command: PresetCommand, out: Output) -> Result<()> {
    match command {
        PresetCommand::Select { slot } => {
            c.select_preset(slot).await?;
            let sp = c.setpoint().await?;
            out.emit(&json!({ "slot": slot, "setpoint": sp }), || {
                format!(
                    "Selected preset M{slot} ({:.2} V / {:.2} A)",
                    sp.voltage, sp.current
                )
            })
        }
        PresetCommand::Get => {
            let slot = c.active_preset().await?;
            let sp = c.setpoint().await?;
            out.emit(&json!({ "slot": slot, "setpoint": sp }), || {
                format!(
                    "Active preset: M{slot}\nSetpoint:      {:.2} V / {:.2} A",
                    sp.voltage, sp.current
                )
            })
        }
        PresetCommand::Set { voltage, current } => {
            c.set_setpoint(Setpoint { voltage, current }).await?;
            out.emit(&Setpoint { voltage, current }, || {
                format!("Setpoint set to {voltage:.2} V / {current:.2} A")
            })
        }
        PresetCommand::Read { slot } => {
            let p = c.read_preset(slot).await?;
            out.emit(&p, || preset_line(slot, &p))
        }
        PresetCommand::Write {
            slot,
            voltage,
            current,
            ovp,
            ocp,
        } => {
            let p = Preset {
                voltage,
                current,
                ovp,
                ocp,
            };
            let written = c.write_preset(slot, p).await?;
            out.emit(&json!({ "slot": slot, "written": written }), || {
                if written {
                    format!("Stored {}", preset_line(slot, &p))
                } else {
                    format!("M{slot} already holds these values (no EEPROM write)")
                }
            })
        }
        PresetCommand::Export { file } => {
            let bank = c.export_presets().await?;
            let text = bank.to_json()?;
            match file {
                Some(path) => {
                    std::fs::write(&path, &text)
                        .with_context(|| format!("write {}", path.display()))?;
                    eprintln!(
                        "Exported {} presets to {}",
                        bank.slots.len(),
                        path.display()
                    );
                }
                None if out.json => println!("{text}"),
                None => {
                    for (i, p) in bank.slots.iter().enumerate() {
                        println!("{}", preset_line(i as u8, p));
                    }
                }
            }
            Ok(())
        }
        PresetCommand::Import { file } => {
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("read {}", file.display()))?;
            let bank = PresetBank::from_json(&text).context("parse preset JSON")?;
            let written = c.import_presets(&bank).await?;
            out.emit(&json!({ "written": written }), || {
                if written.is_empty() {
                    "All presets already up to date (no EEPROM writes)".to_string()
                } else {
                    format!("Wrote slots {written:?}")
                }
            })
        }
    }
}

async fn cmd_rtc(c: &Controller, command: RtcCommand, out: Output) -> Result<()> {
    let t = match command {
        RtcCommand::Get => c.read_rtc().await?,
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
            t
        }
        RtcCommand::Sync => c.sync_rtc().await?,
    };
    out.emit(&t, || format!("Device RTC: {t}"))
}

async fn cmd_system(c: &Controller, command: SystemCommand, out: Output) -> Result<()> {
    match command {
        SystemCommand::Get => {}
        SystemCommand::Buzzer { state } => c.set_buzzer(state.as_bool()).await?,
        SystemCommand::Backlight { level } => c.set_backlight(level).await?,
    }
    let o = c.system_options().await?;
    out.emit(&o, || {
        format!(
            "Buzzer:     {}\nBacklight:  {}\nBoot power: {}\nTake OK:    {}\nTake out:   {}\nLogo:       {}\nLanguage:   {}",
            o.buzzer, o.backlight, o.boot_power, o.take_ok, o.take_out, o.logo, o.language
        )
    })
}

async fn cmd_calibrate(c: &Controller, command: CalibrateCommand, out: Output) -> Result<()> {
    match command {
        CalibrateCommand::Commit { yes } => {
            if !yes {
                bail!("calibration commit writes NVRAM permanently (and is unverified on hardware); pass --yes to confirm");
            }
            c.commit_calibration().await?;
            out.emit(&json!({ "committed": true }), || {
                "Calibration committed to NVRAM".to_string()
            })
        }
        CalibrateCommand::Read { addr: Some(addr) } => {
            let value = c.read_calibration_register(addr).await?;
            out.emit(&json!({ "addr": addr, "value": value }), || {
                format!("Register {addr}: {value} (0x{value:04X})")
            })
        }
        CalibrateCommand::Read { addr: None } => {
            let words = c.read_calibration().await?;
            let value: Vec<_> = words
                .iter()
                .map(|(name, addr, value)| json!({ "name": name, "addr": addr, "value": value }))
                .collect();
            out.emit(&value, || {
                words
                    .iter()
                    .map(|(name, addr, value)| {
                        format!("{addr:>3} {name:<13} {value:>5} (0x{value:04X})")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        CalibrateCommand::Write { addr, value, yes } => {
            if !yes {
                bail!("writing calibration registers is dangerous; pass --yes to confirm");
            }
            c.write_calibration_register(addr, value).await?;
            out.emit(&json!({ "addr": addr, "value": value }), || {
                format!("Wrote {value} to register {addr} (not committed)")
            })
        }
    }
}

async fn cmd_raw(c: &Controller, command: RawCommand, out: Output) -> Result<()> {
    match command {
        RawCommand::Read { addr, count } => {
            let regs = c.read_raw(addr, count).await?;
            out.emit(&json!({ "addr": addr, "values": regs }), || {
                regs.iter()
                    .enumerate()
                    .map(|(i, v)| {
                        format!(
                            "{:>5} (0x{:04X}): {v:>5}  0x{v:04X}",
                            addr as usize + i,
                            addr as usize + i
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            })
        }
        RawCommand::Write { addr, values, yes } => {
            if !yes {
                bail!("raw register writes bypass most safety checks; pass --yes to confirm");
            }
            c.write_raw(addr, &values).await?;
            out.emit(&json!({ "addr": addr, "values": values }), || {
                format!("Wrote {values:?} at {addr}")
            })
        }
    }
}

async fn cmd_log(
    c: &Controller,
    config: &Config,
    path: &Path,
    interval_ms: u64,
    append: bool,
) -> Result<()> {
    let log_config = LoggingConfig {
        enabled: true,
        path: path.to_string_lossy().into_owned(),
        format: LogFormat::from_path(path),
        interval_ms,
        append: append || config.logging.append,
    };
    let logger = TelemetryLogger::start(c, &log_config).await?;
    eprintln!("Logging to {} (Ctrl+C to stop)", log_config.path);
    tokio::signal::ctrl_c().await?;
    let rows = logger.stop().await;
    eprintln!("Logging stopped ({rows} rows)");
    Ok(())
}

/// Connection options forwarded to the GUI binary.
fn gui_args(cli: &Cli) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(config) = &cli.config {
        args.extend(["--config".to_string(), config.display().to_string()]);
    }
    if let Some(port) = &cli.port {
        args.extend(["--port".to_string(), port.clone()]);
    }
    if let Some(baud) = cli.baud {
        args.extend(["--baud".to_string(), baud.to_string()]);
    }
    if let Some(slave) = cli.slave {
        args.extend(["--slave".to_string(), slave.to_string()]);
    }
    args
}

/// Run a sibling binary from the same directory as the CLI executable.
fn dispatch(bin: &str, args: &[String]) -> Result<()> {
    let exe = std::env::current_exe().context("determine executable path")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("executable has no parent directory"))?;
    let path = dir.join(format!("{bin}{}", std::env::consts::EXE_SUFFIX));
    if !path.exists() {
        bail!(
            "{bin} binary not found at {} (build it with `cargo build --workspace`)",
            path.display()
        );
    }
    let status = ProcessCommand::new(&path)
        .args(args)
        .status()
        .with_context(|| format!("spawn {bin}"))?;
    if !status.success() {
        bail!("{bin} exited with {status}");
    }
    Ok(())
}
