//! Background telemetry logging to CSV or JSON Lines.
//!
//! Every row carries the time the sample was *received* from the device and
//! its sequence number. The logger only writes samples it has not written
//! before, so a stalled or disconnected device produces gaps rather than
//! repeated stale rows.

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
use crate::model::{ProtectionStatus, TelemetrySample};

const CSV_HEADER: &str =
    "timestamp,seq,voltage_out,current_out,power_out,voltage_in,keypad_locked,protection,constant_current,output_enabled\n";

/// Handle to a running telemetry logger.
pub struct TelemetryLogger {
    cancel: CancellationToken,
    task: JoinHandle<u64>,
}

impl TelemetryLogger {
    /// Start logging the device's background telemetry to a file.
    pub async fn start(
        controller: &Controller,
        config: &LoggingConfig,
    ) -> Result<Self, DeviceError> {
        if config.interval_ms == 0 {
            return Err(DeviceError::OutOfRange(
                "logging interval must be > 0 ms".to_string(),
            ));
        }
        let mut options = tokio::fs::OpenOptions::new();
        options.create(true);
        if config.append {
            options.append(true);
        } else {
            options.write(true).truncate(true);
        }
        let mut file = options.open(&config.path).await?;

        let empty = file.metadata().await?.len() == 0;
        if config.format == LogFormat::Csv && empty {
            file.write_all(CSV_HEADER.as_bytes()).await?;
        }

        let rx = controller.subscribe();
        let cancel = CancellationToken::new();
        let task = tokio::spawn(run(
            rx,
            file,
            config.format,
            Duration::from_millis(config.interval_ms),
            cancel.clone(),
        ));

        Ok(Self { cancel, task })
    }

    /// Stop logging, wait for the final flush and return the number of
    /// rows written.
    pub async fn stop(self) -> u64 {
        self.cancel.cancel();
        self.task.await.unwrap_or(0)
    }
}

async fn run(
    rx: watch::Receiver<Option<TelemetrySample>>,
    mut file: tokio::fs::File,
    format: LogFormat,
    every: Duration,
    cancel: CancellationToken,
) -> u64 {
    let mut ticker = interval(every);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_seq = 0;
    let mut rows = 0;

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                let Some(sample) = *rx.borrow() else { continue };
                if sample.seq == last_seq {
                    continue; // no new sample since the last row
                }
                last_seq = sample.seq;
                let line = format_line(&sample, format);
                match write_line(&mut file, &line).await {
                    Ok(()) => rows += 1,
                    Err(err) => warn!(error = %err, "telemetry log write failed"),
                }
            }
        }
    }
    let _ = file.flush().await;
    rows
}

async fn write_line(file: &mut tokio::fs::File, line: &str) -> std::io::Result<()> {
    file.write_all(line.as_bytes()).await?;
    file.flush().await
}

/// Format one sample as a CSV row or JSON line (including the newline).
pub fn format_line(sample: &TelemetrySample, format: LogFormat) -> String {
    match format {
        LogFormat::Csv => format_csv(sample),
        LogFormat::JsonLines => format_json(sample),
    }
}

fn format_csv(s: &TelemetrySample) -> String {
    let t = &s.telemetry;
    format!(
        "{},{},{:.2},{:.2},{:.2},{:.2},{},{},{},{}\n",
        s.timestamp
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        s.seq,
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

fn format_json(s: &TelemetrySample) -> String {
    let mut line = serde_json::to_string(s).unwrap_or_default();
    line.push('\n');
    line
}

fn protection_str(p: ProtectionStatus) -> &'static str {
    match p {
        ProtectionStatus::Ok => "ok",
        ProtectionStatus::Ovp => "ovp",
        ProtectionStatus::Ocp => "ocp",
        ProtectionStatus::Other => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Device, DriverOptions, MockBackend};
    use crate::register;

    #[tokio::test]
    async fn logs_only_new_samples() {
        let mock = MockBackend::rd6030();
        mock.set_register(register::reg::VOUT, 1234);
        let options = DriverOptions {
            poll_interval: Some(Duration::from_millis(20)),
            ..DriverOptions::default()
        };
        let c = Controller::from_device(Device::start(mock.connector(), options));
        let path = std::env::temp_dir().join(format!("rd6030-log-{}.csv", std::process::id()));
        let cfg = LoggingConfig {
            enabled: true,
            path: path.to_string_lossy().into_owned(),
            format: LogFormat::Csv,
            interval_ms: 5, // faster than polling: duplicates must be skipped
            append: false,
        };
        let logger = TelemetryLogger::start(&c, &cfg).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let rows = logger.stop().await;
        c.shutdown().await;

        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], CSV_HEADER.trim_end());
        assert_eq!(lines.len() as u64, rows + 1);
        assert!(rows >= 3, "only {rows} rows");
        let seqs: Vec<u64> = lines[1..]
            .iter()
            .map(|l| l.split(',').nth(1).unwrap().parse().unwrap())
            .collect();
        assert!(
            seqs.windows(2).all(|w| w[1] > w[0]),
            "duplicate rows: {seqs:?}"
        );
        assert!(lines[1].contains(",12.34,"));
    }

    #[test]
    fn json_line_is_flat_object() {
        let sample = TelemetrySample {
            telemetry: Default::default(),
            timestamp: chrono::Utc::now(),
            seq: 7,
            received_at: tokio::time::Instant::now(),
        };
        let line = format_line(&sample, LogFormat::JsonLines);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["seq"], 7);
        assert_eq!(v["protection"], "ok");
        assert!(v.get("voltage_out").is_some());
    }

    #[test]
    fn format_from_extension() {
        assert_eq!(LogFormat::from_path("a.jsonl"), LogFormat::JsonLines);
        assert_eq!(LogFormat::from_path("a.JSON"), LogFormat::JsonLines);
        assert_eq!(LogFormat::from_path("a.csv"), LogFormat::Csv);
        assert_eq!(LogFormat::from_path("a"), LogFormat::Csv);
    }
}
