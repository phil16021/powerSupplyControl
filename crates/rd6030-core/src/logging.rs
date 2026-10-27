//! Background telemetry logging to CSV or JSON Lines.

use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::config::{LogFormat, LoggingConfig};
use crate::controller::Controller;
use crate::error::DeviceError;
use crate::model::{DeviceTelemetry, ProtectionStatus};

const CSV_HEADER: &str =
    "timestamp,voltage_out,current_out,power_out,voltage_in,keypad_locked,protection,constant_current,output_enabled\n";

/// Handle to a running telemetry logger.
pub struct TelemetryLogger {
    cancel: CancellationToken,
    task: JoinHandle<()>,
}

impl TelemetryLogger {
    /// Start logging the device's background telemetry to a file.
    pub async fn start(
        controller: &Controller,
        config: &LoggingConfig,
    ) -> Result<Self, DeviceError> {
        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&config.path)
            .await?;

        if config.format == LogFormat::Csv {
            file.write_all(CSV_HEADER.as_bytes()).await?;
        }

        let rx = controller.subscribe();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            rx,
            file,
            config.format,
            config.interval_ms,
            cancel.clone(),
        ));

        Ok(Self { cancel, task })
    }

    /// Stop logging and wait for the final flush.
    pub async fn stop(self) {
        self.cancel.cancel();
        let _ = self.task.await;
    }
}

async fn run(
    mut rx: watch::Receiver<DeviceTelemetry>,
    mut file: tokio::fs::File,
    format: LogFormat,
    interval_ms: u64,
    cancel: CancellationToken,
) {
    let mut ticker = interval(Duration::from_millis(interval_ms));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let telem = *rx.borrow_and_update();
                let line = match format {
                    LogFormat::Csv => format_csv(&telem),
                    LogFormat::JsonLines => format_json(&telem),
                };
                if let Err(err) = write_line(&mut file, &line).await {
                    warn!(error = %err, "telemetry log write failed");
                }
            }
        }
    }
}

async fn write_line(file: &mut tokio::fs::File, line: &str) -> std::io::Result<()> {
    file.write_all(line.as_bytes()).await?;
    file.flush().await
}

fn format_csv(t: &DeviceTelemetry) -> String {
    format!(
        "{},{:.3},{:.3},{:.3},{:.3},{},{},{},{}\n",
        chrono::Local::now().to_rfc3339(),
        t.voltage_out,
        t.current_out,
        t.power_out,
        t.voltage_in,
        t.keypad_locked,
        protection_str(t.protection),
        t.constant_current,
        t.output_enabled,
    )
}

fn format_json(t: &DeviceTelemetry) -> String {
    let record = serde_json::json!({
        "timestamp": chrono::Local::now().to_rfc3339(),
        "voltage_out": t.voltage_out,
        "current_out": t.current_out,
        "power_out": t.power_out,
        "voltage_in": t.voltage_in,
        "keypad_locked": t.keypad_locked,
        "protection": t.protection,
        "constant_current": t.constant_current,
        "output_enabled": t.output_enabled,
    });
    format!("{record}\n")
}

fn protection_str(p: ProtectionStatus) -> &'static str {
    match p {
        ProtectionStatus::Ok => "ok",
        ProtectionStatus::Ovp => "ovp",
        ProtectionStatus::Ocp => "ocp",
        ProtectionStatus::Other => "other",
    }
}
