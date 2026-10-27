//! `rd6030-gui` — egui/eframe GUI for the Riden RD6030.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;
use egui_plot::{Legend, Line, Plot, PlotPoints};
use rd6030_core::config::Config;
use rd6030_core::{BatteryStatus, Controller, DeviceInfo, DeviceTelemetry, RtcTime, Setpoint};

/// Shared state written by async device tasks and read by the UI thread.
#[derive(Clone, Default)]
struct Shared {
    status: Arc<Mutex<String>>,
    info: Arc<Mutex<Option<DeviceInfo>>>,
    battery: Arc<Mutex<Option<BatteryStatus>>>,
    rtc: Arc<Mutex<Option<RtcTime>>>,
    setpoint: Arc<Mutex<Option<Setpoint>>>,
}

/// Ring buffer of recent telemetry samples for the plot.
struct History {
    samples: VecDeque<DeviceTelemetry>,
    capacity: usize,
}

impl History {
    fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    fn push(&mut self, sample: DeviceTelemetry) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(sample);
    }
}

fn main() -> eframe::Result {
    let config = parse_args();

    let runtime = tokio::runtime::Runtime::new().expect("create tokio runtime");
    let controller = match runtime.block_on(async { Controller::connect(&config) }) {
        Ok(controller) => controller,
        Err(err) => {
            eprintln!("Failed to connect to RD6030: {err}");
            std::process::exit(1);
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1000.0, 720.0]),
        ..Default::default()
    };

    eframe::run_native(
        "Riden RD6030 Control",
        options,
        Box::new(move |_cc| Ok(Box::new(App::new(controller, runtime)) as Box<dyn eframe::App>)),
    )
}

/// Minimal command-line parsing for `--config` and `--port`.
fn parse_args() -> Config {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut config_path = "config.toml".to_string();
    let mut port_override = None;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                config_path = args.get(i + 1).cloned().unwrap_or_default();
                i += 1;
            }
            "--port" => {
                port_override = args.get(i + 1).cloned();
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }

    let mut config = Config::load(&config_path).unwrap_or_default();
    if let Some(port) = port_override {
        config.port = port;
    }
    config
}

struct App {
    controller: Controller,
    runtime: tokio::runtime::Runtime,
    shared: Shared,
    telemetry: DeviceTelemetry,
    history: History,
    vset: f32,
    iset: f32,
    preset_slot: u8,
    setpoint_loaded: bool,
}

impl App {
    fn new(controller: Controller, runtime: tokio::runtime::Runtime) -> Self {
        let shared = Shared::default();
        let app = Self {
            controller,
            runtime,
            shared,
            telemetry: DeviceTelemetry::default(),
            history: History::new(600),
            vset: 0.0,
            iset: 0.0,
            preset_slot: 0,
            setpoint_loaded: false,
        };
        app.refresh_info();
        app.refresh_battery();
        app.refresh_rtc();
        app.refresh_setpoint();
        app
    }

    fn spawn(&self, future: impl std::future::Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(future);
    }

    fn refresh_info(&self) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            match c.info().await {
                Ok(info) => *shared.info.lock().unwrap() = Some(info),
                Err(err) => *shared.status.lock().unwrap() = err.to_string(),
            }
        });
    }

    fn refresh_battery(&self) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            match c.battery_status().await {
                Ok(battery) => *shared.battery.lock().unwrap() = Some(battery),
                Err(err) => *shared.status.lock().unwrap() = err.to_string(),
            }
        });
    }

    fn refresh_rtc(&self) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            match c.read_rtc().await {
                Ok(rtc) => *shared.rtc.lock().unwrap() = Some(rtc),
                Err(err) => *shared.status.lock().unwrap() = err.to_string(),
            }
        });
    }

    fn refresh_setpoint(&self) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            let result = match (c.voltage_setpoint().await, c.current_setpoint().await) {
                (Ok(v), Ok(a)) => {
                    *shared.setpoint.lock().unwrap() = Some(Setpoint {
                        voltage: v,
                        current: a,
                    });
                    Ok(())
                }
                (Err(err), _) | (_, Err(err)) => Err(err),
            };
            if let Err(err) = result {
                *shared.status.lock().unwrap() = err.to_string();
            }
        });
    }

    fn apply_setpoint(&self, voltage: f32, current: f32) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            let message = match c.set_setpoint(Setpoint { voltage, current }).await {
                Ok(()) => format!("Setpoint {voltage:.2} V / {current:.2} A applied"),
                Err(err) => err.to_string(),
            };
            *shared.status.lock().unwrap() = message;
        });
    }

    fn set_output(&self, enabled: bool) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            let message = match c.set_output(enabled).await {
                Ok(()) => format!("Output {}", if enabled { "enabled" } else { "disabled" }),
                Err(err) => err.to_string(),
            };
            *shared.status.lock().unwrap() = message;
        });
    }

    fn select_preset(&self, slot: u8) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            let message = match c.select_preset(slot).await {
                Ok(()) => format!("Selected preset M{slot}"),
                Err(err) => err.to_string(),
            };
            *shared.status.lock().unwrap() = message;
        });
    }

    fn sync_rtc(&self) {
        let c = self.controller.clone();
        let shared = self.shared.clone();
        self.spawn(async move {
            match c.sync_rtc().await {
                Ok(rtc) => {
                    *shared.rtc.lock().unwrap() = Some(rtc);
                    *shared.status.lock().unwrap() = format!("RTC synchronized to {rtc}");
                }
                Err(err) => *shared.status.lock().unwrap() = err.to_string(),
            }
        });
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let telemetry = self.controller.latest();
        if telemetry != self.telemetry {
            self.history.push(telemetry);
            self.telemetry = telemetry;
        }

        // Populate the editable setpoint fields once the device responds.
        if !self.setpoint_loaded {
            if let Some(sp) = *self.shared.setpoint.lock().unwrap() {
                self.vset = sp.voltage;
                self.iset = sp.current;
                self.setpoint_loaded = true;
            }
        }

        egui::Panel::top("status_bar").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Riden RD6030");
                let status = self.shared.status.lock().unwrap().clone();
                if !status.is_empty() {
                    ui.separator();
                    ui.label(status);
                }
            });
        });

        egui::Panel::right("info_panel")
            .default_size(240.0)
            .show(ui, |ui| {
                ui.heading("Device");
                if let Some(info) = *self.shared.info.lock().unwrap() {
                    ui.label(format!("Model:     {}", info.model));
                    ui.label(format!("Serial:    {}", info.serial_number));
                    ui.label(format!("Firmware:  {:.2}", info.firmware));
                    ui.label(format!(
                        "Temp:      {:.1} °C / {:.1} °F",
                        info.temperature_c, info.temperature_f
                    ));
                }
                if ui.button("Refresh").clicked() {
                    self.refresh_info();
                }
                ui.separator();

                ui.heading("Battery");
                if let Some(b) = *self.shared.battery.lock().unwrap() {
                    ui.label(format!(
                        "Charging:  {}",
                        if b.charging { "yes" } else { "no" }
                    ));
                    ui.label(format!("Voltage:   {:.2} V", b.voltage));
                    ui.label(format!("Temp:      {:.1} °C", b.temperature_c));
                    ui.label(format!("Capacity:  {:.3} Ah", b.capacity_ah));
                    ui.label(format!("Energy:    {:.3} Wh", b.energy_wh));
                }
                if ui.button("Refresh").clicked() {
                    self.refresh_battery();
                }
                ui.separator();

                ui.heading("RTC");
                if let Some(rtc) = *self.shared.rtc.lock().unwrap() {
                    ui.label(format!("{rtc}"));
                }
                ui.horizontal(|ui| {
                    if ui.button("Read").clicked() {
                        self.refresh_rtc();
                    }
                    if ui.button("Sync to host").clicked() {
                        self.sync_rtc();
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Output");
            ui.horizontal(|ui| {
                ui.monospace(format!("Vout: {:>6.2} V", self.telemetry.voltage_out));
                ui.monospace(format!("Iout: {:>6.2} A", self.telemetry.current_out));
                ui.monospace(format!("Pout: {:>6.2} W", self.telemetry.power_out));
                ui.monospace(format!("Vin: {:>6.2} V", self.telemetry.voltage_in));
            });
            ui.horizontal(|ui| {
                ui.label(format!(
                    "Mode: {}",
                    if self.telemetry.constant_current {
                        "CC"
                    } else {
                        "CV"
                    }
                ));
                ui.label(format!("Protection: {:?}", self.telemetry.protection));
                ui.label(format!(
                    "Keypad: {}",
                    if self.telemetry.keypad_locked {
                        "locked"
                    } else {
                        "unlocked"
                    }
                ));
            });
            ui.separator();

            ui.heading("Control");
            ui.horizontal(|ui| {
                ui.label("Vset:");
                ui.add(
                    egui::DragValue::new(&mut self.vset)
                        .range(0.0..=60.0)
                        .speed(0.1)
                        .suffix(" V"),
                );
                ui.label("Iset:");
                ui.add(
                    egui::DragValue::new(&mut self.iset)
                        .range(0.0..=30.0)
                        .speed(0.1)
                        .suffix(" A"),
                );
                if ui.button("Apply").clicked() {
                    let (voltage, current) = (self.vset, self.iset);
                    self.apply_setpoint(voltage, current);
                }
            });
            ui.horizontal(|ui| {
                let mut output = self.telemetry.output_enabled;
                if ui.toggle_value(&mut output, "Output").changed() {
                    self.set_output(output);
                }
                ui.label("Preset:");
                let mut preset = self.preset_slot;
                egui::ComboBox::from_id_salt("preset")
                    .selected_text(format!("M{preset}"))
                    .show_ui(ui, |ui| {
                        for slot in 0..10u8 {
                            ui.selectable_value(&mut preset, slot, format!("M{slot}"));
                        }
                    });
                if preset != self.preset_slot {
                    self.preset_slot = preset;
                }
                if ui.button("Select").clicked() {
                    self.select_preset(self.preset_slot);
                }
            });
            ui.separator();

            ui.heading("History");
            let vpoints: PlotPoints = self
                .history
                .samples
                .iter()
                .enumerate()
                .map(|(i, t)| [i as f64, f64::from(t.voltage_out)])
                .collect();
            let ipoints: PlotPoints = self
                .history
                .samples
                .iter()
                .enumerate()
                .map(|(i, t)| [i as f64, f64::from(t.current_out)])
                .collect();
            let vline = Line::new("Vout (V)", vpoints).color(egui::Color32::LIGHT_BLUE);
            let iline = Line::new("Iout (A)", ipoints).color(egui::Color32::LIGHT_GREEN);
            Plot::new("history")
                .legend(Legend::default())
                .height(220.0)
                .show(ui, |plot_ui| {
                    plot_ui.line(vline);
                    plot_ui.line(iline);
                });
        });

        // Keep refreshing so live telemetry stays current.
        ui.ctx().request_repaint_after(Duration::from_millis(100));
    }
}
