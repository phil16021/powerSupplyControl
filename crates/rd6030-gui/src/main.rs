//! `rd6030-gui` — egui/eframe GUI for the Riden RD6030.
//!
//! The GUI starts even when the device is absent: the driver reconnects in
//! the background and the status bar shows the link state. Device reads and
//! writes run on a tokio runtime; results are handed to the UI thread
//! through [`Shared`].

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use clap::Parser;
use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints};
use rd6030_core::config::Config;
use rd6030_core::{
    BatteryStatus, Controller, DeviceError, DeviceInfo, DeviceTelemetry, LinkState, Protection,
    RtcTime, Setpoint,
};

/// How long status messages stay visible.
const STATUS_TTL: Duration = Duration::from_secs(8);
/// How often setpoints, protection and preset slot are re-read.
const RESYNC_INTERVAL: Duration = Duration::from_secs(2);
/// Plot history length in seconds.
const HISTORY_SECONDS: f64 = 120.0;

#[derive(Parser)]
#[command(name = "rd6030-gui", version, about = "GUI for the Riden RD6030")]
struct Args {
    /// Configuration file (default lookup as for rd6030-cli).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Serial port override.
    #[arg(long)]
    port: Option<String>,
    /// Baud rate override.
    #[arg(long)]
    baud: Option<u32>,
    /// Modbus slave address override.
    #[arg(long)]
    slave: Option<u8>,
}

fn load_config(args: &Args) -> Result<Config, String> {
    let (mut config, _) = Config::discover(args.config.as_deref()).map_err(|e| e.to_string())?;
    if let Some(port) = &args.port {
        config.port = port.clone();
    }
    if let Some(baud) = args.baud {
        config.baud_rate = baud;
    }
    if let Some(slave) = args.slave {
        config.slave_id = slave;
    }
    config.validate().map_err(|e| e.to_string())?;
    Ok(config)
}

/// A status-bar message.
#[derive(Clone)]
struct Status {
    text: String,
    error: bool,
    at: Instant,
}

/// Shared state written by async device tasks and read by the UI thread.
#[derive(Clone, Default)]
struct Shared {
    status: Arc<Mutex<Option<Status>>>,
    info: Arc<Mutex<Option<DeviceInfo>>>,
    battery: Arc<Mutex<Option<BatteryStatus>>>,
    rtc: Arc<Mutex<Option<RtcTime>>>,
    setpoint: Arc<Mutex<Option<Setpoint>>>,
    protection: Arc<Mutex<Option<Protection>>>,
    preset_slot: Arc<Mutex<Option<u8>>>,
}

impl Shared {
    fn ok(&self, text: impl Into<String>) {
        self.set_status(text.into(), false);
    }

    fn err(&self, err: &DeviceError) {
        self.set_status(err.to_string(), true);
    }

    fn set_status(&self, text: String, error: bool) {
        *self.status.lock().unwrap() = Some(Status {
            text,
            error,
            at: Instant::now(),
        });
    }
}

/// Plot id, legend name, colour and value accessor.
type Series = (
    &'static str,
    &'static str,
    egui::Color32,
    fn(&DeviceTelemetry) -> f32,
);

/// Time-stamped samples for the plots.
struct History {
    samples: VecDeque<(f64, DeviceTelemetry)>,
    last_seq: u64,
    origin: chrono::DateTime<chrono::Utc>,
}

impl History {
    fn new() -> Self {
        Self {
            samples: VecDeque::new(),
            last_seq: 0,
            origin: chrono::Utc::now(),
        }
    }

    fn push(&mut self, sample: &rd6030_core::TelemetrySample) {
        if sample.seq == self.last_seq {
            return;
        }
        self.last_seq = sample.seq;
        let t = (sample.timestamp - self.origin).num_milliseconds() as f64 / 1000.0;
        self.samples.push_back((t, sample.telemetry));
        while self
            .samples
            .front()
            .is_some_and(|(t0, _)| t - t0 > HISTORY_SECONDS)
        {
            self.samples.pop_front();
        }
    }

    fn series(&self, f: impl Fn(&DeviceTelemetry) -> f32) -> PlotPoints<'static> {
        self.samples
            .iter()
            .map(|(t, s)| [*t, f64::from(f(s))])
            .collect()
    }
}

fn main() -> eframe::Result {
    let args = Args::parse();
    let config = match load_config(&args) {
        Ok(config) => config,
        Err(err) => {
            eprintln!("rd6030-gui: {err}");
            std::process::exit(2);
        }
    };

    let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let controller = {
        let _guard = runtime.enter();
        Arc::new(Mutex::new(Controller::start(&config)))
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1100.0, 780.0]),
        ..Default::default()
    };

    let app_controller = Arc::clone(&controller);
    let handle = runtime.handle().clone();
    let app_config = config.clone();
    let result = eframe::run_native(
        "Riden RD6030 Control",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(App::new(app_controller, handle, app_config)) as Box<dyn eframe::App>)
        }),
    );

    // Apply the exit safety policy (e.g. switch the output off).
    let controller = controller.lock().unwrap().clone();
    runtime.block_on(controller.shutdown_safely(&config.safety));
    result
}

struct App {
    controller: Arc<Mutex<Controller>>,
    runtime: tokio::runtime::Handle,
    config: Config,
    shared: Shared,
    history: History,
    vset: f32,
    iset: f32,
    ovp: f32,
    ocp: f32,
    preset_slot: u8,
    edits_loaded: bool,
    last_resync: Instant,
    port_input: String,
    ports: Vec<String>,
}

impl App {
    fn new(
        controller: Arc<Mutex<Controller>>,
        runtime: tokio::runtime::Handle,
        config: Config,
    ) -> Self {
        let app = Self {
            controller,
            runtime,
            port_input: config.port.clone(),
            config,
            shared: Shared::default(),
            history: History::new(),
            vset: 0.0,
            iset: 0.0,
            ovp: 0.0,
            ocp: 0.0,
            preset_slot: 0,
            edits_loaded: false,
            last_resync: Instant::now(),
            ports: list_ports(),
        };
        app.refresh_all();
        app
    }

    fn controller(&self) -> Controller {
        self.controller.lock().unwrap().clone()
    }

    /// Run `f` with a controller clone on the runtime.
    fn spawn<F, Fut>(&self, f: F)
    where
        F: FnOnce(Controller, Shared) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        self.runtime
            .spawn(f(self.controller(), self.shared.clone()));
    }

    fn refresh_all(&self) {
        self.spawn(|c, s| async move {
            match c.info().await {
                Ok(info) => *s.info.lock().unwrap() = Some(info),
                Err(err) => return s.err(&err),
            }
            if let Ok(b) = c.battery_status().await {
                *s.battery.lock().unwrap() = Some(b);
            }
            if let Ok(rtc) = c.read_rtc().await {
                *s.rtc.lock().unwrap() = Some(rtc);
            }
        });
        self.resync();
    }

    /// Re-read setpoints, protection and preset slot (front-panel changes).
    fn resync(&self) {
        self.spawn(|c, s| async move {
            if let Ok(sp) = c.setpoint().await {
                *s.setpoint.lock().unwrap() = Some(sp);
            }
            if let Ok(p) = c.protection().await {
                *s.protection.lock().unwrap() = Some(p);
            }
            if let Ok(slot) = c.active_preset().await {
                *s.preset_slot.lock().unwrap() = Some(slot);
            }
        });
    }

    fn reconnect(&mut self) {
        let mut config = self.config.clone();
        config.port = self.port_input.trim().to_string();
        if let Err(err) = config.validate() {
            self.shared.set_status(err.to_string(), true);
            return;
        }
        let old = {
            let _guard = self.runtime.enter();
            std::mem::replace(
                &mut *self.controller.lock().unwrap(),
                Controller::start(&config),
            )
        };
        self.runtime.spawn(async move { old.shutdown().await });
        self.config = config;
        self.history = History::new();
        self.edits_loaded = false;
        self.shared
            .ok(format!("Connecting to {}", self.config.port));
        self.refresh_all();
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        let link = self.controller().link();
        ui.horizontal(|ui| {
            ui.heading("Riden RD6030");
            ui.separator();
            let (color, label) = match link.state {
                LinkState::Connected => (egui::Color32::from_rgb(60, 180, 75), "connected"),
                LinkState::Degraded => (egui::Color32::from_rgb(230, 160, 30), "degraded"),
                LinkState::Disconnected => (egui::Color32::from_rgb(220, 60, 60), "disconnected"),
                LinkState::Connecting => (egui::Color32::GRAY, "connecting"),
            };
            ui.colored_label(color, format!("● {label}"));
            ui.label(&self.config.port);
            if let Some(err) = &link.last_error {
                if link.state != LinkState::Connected {
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), err);
                }
            }
            let mut status = self.shared.status.lock().unwrap();
            if status.as_ref().is_some_and(|s| s.at.elapsed() > STATUS_TTL) {
                *status = None;
            }
            if let Some(s) = status.as_ref() {
                ui.separator();
                if s.error {
                    ui.colored_label(egui::Color32::from_rgb(220, 60, 60), &s.text);
                } else {
                    ui.label(&s.text);
                }
            }
        });
    }

    fn side_panel(&mut self, ui: &mut egui::Ui) {
        ui.set_max_width(260.0);
        ui.heading("Connection");
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("ports")
                .width(200.0)
                .truncate()
                .selected_text(short_port(&self.port_input))
                .show_ui(ui, |ui| {
                    for p in &self.ports {
                        ui.selectable_value(&mut self.port_input, p.clone(), p)
                            .on_hover_text(p);
                    }
                });
            if ui.button("⟳").on_hover_text("Rescan ports").clicked() {
                self.ports = list_ports();
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.port_input).desired_width(230.0));
        if ui.button("Connect").clicked() {
            self.reconnect();
        }
        let link = self.controller().link();
        ui.small(format!(
            "{} requests · {} errors · {} timeouts · {} connects",
            link.requests, link.errors, link.timeouts, link.connects
        ));
        ui.separator();

        ui.heading("Device");
        if let Some(info) = *self.shared.info.lock().unwrap() {
            ui.label(format!("Model:     {}", info.model));
            ui.label(format!("Serial:    {}", info.serial_number));
            ui.label(format!("Firmware:  {:.2}", info.firmware));
            ui.label(format!(
                "Temp:      {:.0} °C / {:.0} °F",
                info.temperature_c, info.temperature_f
            ));
        }
        if ui.button("Refresh").clicked() {
            self.refresh_all();
        }
        ui.separator();

        ui.heading("Battery");
        if let Some(b) = *self.shared.battery.lock().unwrap() {
            ui.label(format!(
                "Charging:  {}",
                if b.charging { "yes" } else { "no" }
            ));
            ui.label(format!("Voltage:   {:.2} V", b.voltage));
            ui.label(format!("Capacity:  {:.3} Ah", b.capacity_ah));
            ui.label(format!("Energy:    {:.3} Wh", b.energy_wh));
        }
        ui.separator();

        ui.heading("RTC");
        if let Some(rtc) = *self.shared.rtc.lock().unwrap() {
            ui.label(rtc.to_string());
        }
        if ui.button("Sync to host").clicked() {
            self.spawn(|c, s| async move {
                match c.sync_rtc().await {
                    Ok(rtc) => {
                        *s.rtc.lock().unwrap() = Some(rtc);
                        s.ok(format!("RTC synchronized to {rtc}"));
                    }
                    Err(err) => s.err(&err),
                }
            });
        }
    }

    /// `fresh` is false while there is no recent telemetry: the displayed
    /// output/lock state is then unknown, so state-dependent toggles must not
    /// be offered (a toggle built from a default "off" would switch ON).
    fn controls(&mut self, ui: &mut egui::Ui, t: &DeviceTelemetry, fresh: bool) {
        let limits = self.controller().limits();
        let device_sp = *self.shared.setpoint.lock().unwrap();
        let device_prot = *self.shared.protection.lock().unwrap();

        ui.heading("Setpoint");
        ui.horizontal(|ui| {
            ui.label("V:");
            ui.add(
                egui::DragValue::new(&mut self.vset)
                    .range(0.0..=limits.max_voltage)
                    .speed(0.05)
                    .fixed_decimals(2)
                    .suffix(" V"),
            );
            ui.label("I:");
            ui.add(
                egui::DragValue::new(&mut self.iset)
                    .range(0.0..=limits.max_current)
                    .speed(0.05)
                    .fixed_decimals(2)
                    .suffix(" A"),
            );
            // Never write the placeholder 0 V / 0 A before the device
            // setpoint has been loaded into the edit fields.
            let apply = ui
                .add_enabled(self.edits_loaded, egui::Button::new("Apply"))
                .on_disabled_hover_text("Waiting for the device setpoint");
            if apply.clicked() {
                let sp = Setpoint {
                    voltage: self.vset,
                    current: self.iset,
                };
                self.spawn(move |c, s| async move {
                    match c.set_setpoint(sp).await {
                        Ok(()) => {
                            *s.setpoint.lock().unwrap() = Some(sp);
                            s.ok(format!(
                                "Setpoint {:.2} V / {:.2} A applied",
                                sp.voltage, sp.current
                            ));
                        }
                        Err(err) => s.err(&err),
                    }
                });
            }
            if let Some(sp) = device_sp {
                ui.weak(format!("device: {:.2} V / {:.2} A", sp.voltage, sp.current));
                if ui
                    .small_button("↺")
                    .on_hover_text("Load device values")
                    .clicked()
                {
                    self.vset = sp.voltage;
                    self.iset = sp.current;
                }
            }
        });

        ui.horizontal(|ui| {
            // Explicit ON/OFF commands instead of a toggle derived from
            // (possibly stale) telemetry. OFF is always available.
            let state = match (fresh, t.output_enabled) {
                (false, _) => "Output: ?",
                (true, true) => "Output: ON",
                (true, false) => "Output: OFF",
            };
            ui.label(egui::RichText::new(state).strong());
            let on = ui
                .add_enabled(fresh && !t.output_enabled, egui::Button::new("On"))
                .on_disabled_hover_text("Needs fresh telemetry and output off")
                .clicked();
            let off = ui.button("Off").clicked();
            if on || off {
                let output = on;
                self.spawn(move |c, s| async move {
                    match c.set_output(output).await {
                        Ok(()) => s.ok(if output {
                            "Output enabled"
                        } else {
                            "Output disabled"
                        }),
                        Err(err) => s.err(&err),
                    }
                });
            }
            let mut locked = t.keypad_locked;
            let lock = ui
                .add_enabled(fresh, egui::Button::selectable(locked, "Keypad lock"))
                .on_hover_text(
                    "fw 1.47 shows the lock while remote-controlled and ignores unlock writes",
                );
            if lock.clicked() {
                locked = !locked;
                self.spawn(move |c, s| async move {
                    if let Err(err) = c.set_keypad_lock(locked).await {
                        s.err(&err);
                    }
                });
            }
            ui.separator();
            ui.label("Preset:");
            egui::ComboBox::from_id_salt("preset")
                .selected_text(format!("M{}", self.preset_slot))
                .show_ui(ui, |ui| {
                    for slot in 0..rd6030_core::PRESET_COUNT {
                        ui.selectable_value(&mut self.preset_slot, slot, format!("M{slot}"));
                    }
                });
            if ui.button("Recall").clicked() {
                let slot = self.preset_slot;
                self.spawn(move |c, s| async move {
                    match c.select_preset(slot).await {
                        Ok(()) => s.ok(format!("Recalled preset M{slot}")),
                        Err(err) => s.err(&err),
                    }
                });
                self.edits_loaded = false;
                self.resync();
            }
            if let Some(active) = *self.shared.preset_slot.lock().unwrap() {
                ui.weak(format!("active: M{active}"));
            }
        });

        ui.horizontal(|ui| {
            ui.label("OVP:");
            ui.add(
                egui::DragValue::new(&mut self.ovp)
                    .range(0.0..=rd6030_core::register::MAX_OVP)
                    .speed(0.05)
                    .fixed_decimals(2)
                    .suffix(" V"),
            );
            ui.label("OCP:");
            ui.add(
                egui::DragValue::new(&mut self.ocp)
                    .range(0.0..=rd6030_core::register::MAX_OCP)
                    .speed(0.05)
                    .fixed_decimals(2)
                    .suffix(" A"),
            );
            if ui
                .button("Set protection")
                .on_hover_text("Stored in device EEPROM")
                .clicked()
            {
                let p = Protection {
                    ovp: self.ovp,
                    ocp: self.ocp,
                };
                self.spawn(move |c, s| async move {
                    match c.set_protection(p).await {
                        Ok(()) => {
                            *s.protection.lock().unwrap() = Some(p);
                            s.ok(format!(
                                "Protection OVP {:.2} V / OCP {:.2} A",
                                p.ovp, p.ocp
                            ));
                        }
                        Err(err) => s.err(&err),
                    }
                });
            }
            if let Some(p) = device_prot {
                ui.weak(format!("device: {:.2} V / {:.2} A", p.ovp, p.ocp));
            }
        });
    }

    fn plots(&self, ui: &mut egui::Ui) {
        let height = ((ui.available_height() - 20.0) / 3.0).max(80.0);
        let series: [Series; 3] = [
            ("v", "Vout (V)", egui::Color32::LIGHT_BLUE, |t| {
                t.voltage_out
            }),
            ("i", "Iout (A)", egui::Color32::LIGHT_GREEN, |t| {
                t.current_out
            }),
            (
                "p",
                "Pout (W)",
                egui::Color32::from_rgb(240, 180, 80),
                |t| t.power_out,
            ),
        ];
        for (id, name, color, f) in series {
            Plot::new(id)
                .legend(Legend::default())
                .height(height)
                .include_y(0.0)
                .x_axis_label("t (s)")
                .link_axis(egui::Id::unique("rd6030-history"), [true, false])
                .show(ui, |plot_ui| {
                    plot_ui.line(Line::new(name, self.history.series(f)).color(color));
                });
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let latest = self.controller().latest();
        if let Some(sample) = &latest {
            self.history.push(sample);
        }
        let telemetry = latest.map(|s| s.telemetry).unwrap_or_default();
        let fresh = latest.is_some_and(|s| s.age() < Duration::from_secs(2));

        if self.last_resync.elapsed() > RESYNC_INTERVAL {
            self.last_resync = Instant::now();
            if self.controller().link().state == LinkState::Connected {
                self.resync();
            }
        }
        // Populate the editable fields once the device responds.
        if !self.edits_loaded {
            let sp = *self.shared.setpoint.lock().unwrap();
            let prot = *self.shared.protection.lock().unwrap();
            if let (Some(sp), Some(p)) = (sp, prot) {
                (self.vset, self.iset, self.ovp, self.ocp) = (sp.voltage, sp.current, p.ovp, p.ocp);
                if let Some(slot) = *self.shared.preset_slot.lock().unwrap() {
                    self.preset_slot = slot;
                }
                self.edits_loaded = true;
            }
        }

        egui::Panel::top("status_bar").show(ui, |ui| self.status_bar(ui));
        egui::Panel::right("side_panel")
            .default_size(260.0)
            .show(ui, |ui| self.side_panel(ui));

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Output");
            let value = |v: f32, unit: &str| {
                if fresh {
                    format!("{v:>6.2} {unit}")
                } else {
                    format!("{:>6} {unit}", "--.--")
                }
            };
            ui.horizontal(|ui| {
                for (label, text) in [
                    ("Vout", value(telemetry.voltage_out, "V")),
                    ("Iout", value(telemetry.current_out, "A")),
                    ("Pout", value(telemetry.power_out, "W")),
                    ("Vin", value(telemetry.voltage_in, "V")),
                ] {
                    ui.label(label);
                    ui.add(egui::Label::new(
                        egui::RichText::new(text).monospace().size(22.0),
                    ));
                    ui.add_space(12.0);
                }
            });
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Mode: {}",
                    if telemetry.constant_current {
                        "CC"
                    } else {
                        "CV"
                    }
                ));
                let prot = format!("{:?}", telemetry.protection).to_uppercase();
                if telemetry.protection == rd6030_core::ProtectionStatus::Ok {
                    ui.label(format!("Protection: {prot}"));
                } else {
                    ui.colored_label(
                        egui::Color32::from_rgb(220, 60, 60),
                        format!("Protection: {prot} tripped"),
                    );
                }
                ui.label(format!(
                    "Output: {}",
                    if telemetry.output_enabled {
                        "ON"
                    } else {
                        "OFF"
                    }
                ));
                if !fresh {
                    ui.colored_label(egui::Color32::GRAY, "(no live data)");
                }
            });
            ui.separator();
            self.controls(ui, &telemetry, fresh);
            ui.separator();
            ui.heading("History");
            self.plots(ui);
        });

        // Keep refreshing so live telemetry stays current.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}

/// Last path component of a port path, for compact display.
fn short_port(port: &str) -> &str {
    port.rsplit('/').next().unwrap_or(port)
}

fn list_ports() -> Vec<String> {
    let mut ports: Vec<String> = tokio_serial::available_ports()
        .map(|ps| ps.into_iter().map(|p| p.port_name).collect())
        .unwrap_or_default();
    if let Ok(entries) = std::fs::read_dir("/dev/serial/by-id") {
        ports.extend(entries.flatten().map(|e| e.path().display().to_string()));
    }
    if std::path::Path::new("/dev/rd6030").exists() {
        ports.insert(0, "/dev/rd6030".to_string());
    }
    ports.dedup();
    ports
}
