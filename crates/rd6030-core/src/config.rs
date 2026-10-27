//! Application configuration, loaded from a TOML file.
//!
//! All fields have sane defaults (see [`Config::default`]) so that a minimal
//! or empty configuration file is accepted. The device is always operated
//! with the fixed serial framing 8N1, so data bits, parity and stop bits are
//! *not* part of the configuration.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::register;

/// Root configuration for the RD6030 control software.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Serial port path, e.g. `/dev/ttyUSB0`, `/dev/tty.usbserial-*` or `COM3`.
    pub port: String,
    /// Serial baud rate. The RD6030 supports 9600..=115200.
    pub baud_rate: u32,
    /// Modbus slave (device) address, 1..=247.
    pub slave_id: u8,
    /// Per-transaction communication timeout in milliseconds (>= 300 ms).
    pub timeout_ms: u64,
    /// Background telemetry polling interval in milliseconds (50..=1000 ms,
    /// i.e. 10..=20 Hz is recommended by the hardware).
    pub poll_interval_ms: u64,
    /// Minimum idle gap between two Modbus transactions in milliseconds
    /// (protects the device MCU from back-to-back requests).
    pub min_gap_ms: u64,
    /// Accept devices whose model id is not an RD6030 (RD6030 scaling is
    /// still applied, which is wrong for e.g. P variants).
    pub allow_unknown_model: bool,
    /// Software limits applied on top of the device rating.
    pub limits: LimitsConfig,
    /// Safety behaviour.
    pub safety: SafetyConfig,
    /// Settings for the HTTP (REST) server and the streamable-HTTP MCP mount.
    pub server: ServerConfig,
    /// Telemetry logging settings.
    pub logging: LoggingConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 115_200,
            slave_id: 1,
            timeout_ms: 500,
            poll_interval_ms: 100,
            min_gap_ms: 5,
            allow_unknown_model: false,
            limits: LimitsConfig::default(),
            safety: SafetyConfig::default(),
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

/// User-configurable software limits. Every setter (CLI, REST, MCP, GUI)
/// rejects values above these, protecting the device under test.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LimitsConfig {
    /// Maximum voltage setpoint in volts (<= 60 V).
    pub max_voltage: f32,
    /// Maximum current setpoint in amperes (<= 30 A).
    pub max_current: f32,
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            max_voltage: register::MAX_VOLTAGE,
            max_current: register::MAX_CURRENT,
        }
    }
}

/// Safety behaviour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SafetyConfig {
    /// Switch the output off when a long-running process (server, GUI)
    /// shuts down.
    pub output_off_on_exit: bool,
}

/// HTTP server (REST API) settings. The MCP streamable-HTTP endpoint is
/// mounted on the same listener.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Bind host for the HTTP listener.
    pub host: String,
    /// Bind port for the HTTP listener.
    pub port: u16,
    /// Bearer token required on every request (`Authorization: Bearer ...`).
    /// Mandatory when binding to a non-loopback address.
    pub token: Option<String>,
    /// Expose the calibration endpoints (NVRAM writes). Off by default.
    pub allow_calibration: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
            token: None,
            allow_calibration: false,
        }
    }
}

/// Telemetry logging settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Whether a background logger is started together with the device.
    pub enabled: bool,
    /// Output file path for CSV or JSON-lines logging.
    pub path: String,
    /// Log file format.
    pub format: LogFormat,
    /// Interval between two logged samples in milliseconds.
    pub interval_ms: u64,
    /// Append to an existing file instead of truncating it.
    pub append: bool,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "rd6030-telemetry.csv".to_string(),
            format: LogFormat::Csv,
            interval_ms: 1000,
            append: false,
        }
    }
}

/// Log file format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// Comma-separated values, one header row plus one row per sample.
    Csv,
    /// One JSON object per line (JSON Lines).
    JsonLines,
}

impl LogFormat {
    /// Pick the format from a file extension (`.jsonl`/`.json` → JSON Lines,
    /// anything else → CSV).
    pub fn from_path(path: impl AsRef<Path>) -> Self {
        match path.as_ref().extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("jsonl") || ext.eq_ignore_ascii_case("json") => {
                Self::JsonLines
            }
            _ => Self::Csv,
        }
    }
}

/// Errors that can occur while loading or validating the configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read configuration file: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("invalid configuration value: {0}")]
    Invalid(String),
}

/// Where a configuration was loaded from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    /// A configuration file.
    File(PathBuf),
    /// No file was found; built-in defaults are used.
    Defaults,
}

/// Environment variable naming an explicit configuration file.
pub const CONFIG_ENV: &str = "RD6030_CONFIG";

impl Config {
    /// Load and validate a configuration from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|err| {
            ConfigError::Io(std::io::Error::new(
                err.kind(),
                format!("{}: {err}", path.display()),
            ))
        })?;
        text.parse()
    }

    /// Resolve and load the configuration.
    ///
    /// Lookup order: `explicit` path → `$RD6030_CONFIG` → `./config.toml` →
    /// `$XDG_CONFIG_HOME/rd6030/config.toml` (or `~/.config/...`) → built-in
    /// defaults. An explicitly named file (argument or environment) must
    /// exist; the implicit locations are optional.
    pub fn discover(explicit: Option<&Path>) -> Result<(Self, ConfigSource), ConfigError> {
        let env = std::env::var_os(CONFIG_ENV).map(PathBuf::from);
        Self::discover_in(explicit, env.as_deref(), &Self::implicit_paths())
    }

    fn discover_in(
        explicit: Option<&Path>,
        env: Option<&Path>,
        implicit: &[PathBuf],
    ) -> Result<(Self, ConfigSource), ConfigError> {
        if let Some(path) = explicit.or(env) {
            return Ok((Self::load(path)?, ConfigSource::File(path.to_path_buf())));
        }
        for path in implicit {
            if path.is_file() {
                return Ok((Self::load(path)?, ConfigSource::File(path.clone())));
            }
        }
        Ok((Self::default(), ConfigSource::Defaults))
    }

    fn implicit_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("config.toml")];
        let xdg = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
        if let Some(dir) = xdg {
            paths.push(dir.join("rd6030").join("config.toml"));
        }
        paths
    }

    /// Per-transaction timeout.
    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Background polling interval.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_millis(self.poll_interval_ms)
    }

    /// Validate the configuration values against the device constraints.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !(1..=247).contains(&self.slave_id) {
            return Err(ConfigError::Invalid(format!(
                "slave_id must be within 1..=247, got {}",
                self.slave_id
            )));
        }
        if !(9_600..=115_200).contains(&self.baud_rate) {
            return Err(ConfigError::Invalid(format!(
                "baud_rate must be within 9600..=115200, got {}",
                self.baud_rate
            )));
        }
        if self.timeout_ms < 300 {
            return Err(ConfigError::Invalid(format!(
                "timeout_ms must be >= 300 ms (got {})",
                self.timeout_ms
            )));
        }
        if !(50..=1000).contains(&self.poll_interval_ms) {
            return Err(ConfigError::Invalid(format!(
                "poll_interval_ms must be within 50..=1000 ms (got {})",
                self.poll_interval_ms
            )));
        }
        if self.port.trim().is_empty() {
            return Err(ConfigError::Invalid("port must not be empty".to_string()));
        }
        if self.min_gap_ms > 100 {
            return Err(ConfigError::Invalid(format!(
                "min_gap_ms must be <= 100 ms (got {})",
                self.min_gap_ms
            )));
        }
        let l = &self.limits;
        if !(l.max_voltage > 0.0 && l.max_voltage <= register::MAX_VOLTAGE) {
            return Err(ConfigError::Invalid(format!(
                "limits.max_voltage must be within (0, {}] V (got {})",
                register::MAX_VOLTAGE,
                l.max_voltage
            )));
        }
        if !(l.max_current > 0.0 && l.max_current <= register::MAX_CURRENT) {
            return Err(ConfigError::Invalid(format!(
                "limits.max_current must be within (0, {}] A (got {})",
                register::MAX_CURRENT,
                l.max_current
            )));
        }
        if self.logging.interval_ms == 0 {
            return Err(ConfigError::Invalid(
                "logging.interval_ms must be > 0".to_string(),
            ));
        }
        if matches!(&self.server.token, Some(t) if t.trim().is_empty()) {
            return Err(ConfigError::Invalid(
                "server.token must not be empty".to_string(),
            ));
        }
        Ok(())
    }
}

impl std::str::FromStr for Config {
    type Err = ConfigError;

    /// Parse and validate a configuration from a TOML string.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn defaults_match_device_standard() {
        let cfg = Config::default();
        assert_eq!(cfg.port, "/dev/ttyUSB0");
        assert_eq!(cfg.baud_rate, 115_200);
        assert_eq!(cfg.slave_id, 1);
        assert_eq!(cfg.timeout_ms, 500);
        assert_eq!(cfg.poll_interval_ms, 100);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.server.port, 8080);
        assert!(!cfg.logging.enabled);
    }

    #[test]
    fn empty_toml_yields_defaults() {
        let cfg = Config::from_str("").expect("empty config should parse");
        assert_eq!(cfg, Config::default());
    }

    #[test]
    fn parses_full_config() {
        let text = r#"
            port = "/dev/ttyUSB1"
            baud_rate = 9600
            slave_id = 7
            timeout_ms = 400
            poll_interval_ms = 200

            [server]
            host = "0.0.0.0"
            port = 9090

            [logging]
            enabled = true
            path = "/tmp/rd6030.jsonl"
            format = "jsonlines"
            interval_ms = 250
        "#;
        let cfg = Config::from_str(text).expect("should parse");
        assert_eq!(cfg.port, "/dev/ttyUSB1");
        assert_eq!(cfg.baud_rate, 9600);
        assert_eq!(cfg.slave_id, 7);
        assert_eq!(cfg.timeout_ms, 400);
        assert_eq!(cfg.poll_interval_ms, 200);
        assert_eq!(cfg.server.host, "0.0.0.0");
        assert_eq!(cfg.server.port, 9090);
        assert!(cfg.logging.enabled);
        assert_eq!(cfg.logging.path, "/tmp/rd6030.jsonl");
        assert_eq!(cfg.logging.format, LogFormat::JsonLines);
        assert_eq!(cfg.logging.interval_ms, 250);
    }

    #[test]
    fn rejects_out_of_range_slave_id() {
        assert!(Config {
            slave_id: 0,
            ..Config::default()
        }
        .validate()
        .is_err());
        assert!(Config {
            slave_id: 248,
            ..Config::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn rejects_too_short_timeout() {
        assert!(Config {
            timeout_ms: 200,
            ..Config::default()
        }
        .validate()
        .is_err());
    }

    #[test]
    fn rejects_limits_above_rating() {
        let mut cfg = Config::default();
        cfg.limits.max_voltage = 61.0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.limits.max_current = 0.0;
        assert!(cfg.validate().is_err());
        let mut cfg = Config::default();
        cfg.limits.max_current = f32::NAN;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn example_config_parses() {
        let text = include_str!("../../../config.example.toml");
        Config::from_str(text).expect("config.example.toml must stay valid");
    }

    #[test]
    fn discovery_order() {
        let dir = std::env::temp_dir().join(format!("rd6030-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.toml");
        let b = dir.join("b.toml");
        std::fs::write(&a, "slave_id = 2").unwrap();
        std::fs::write(&b, "slave_id = 3").unwrap();
        let missing = dir.join("missing.toml");

        // Explicit wins over env and implicit paths.
        let (cfg, src) = Config::discover_in(Some(&a), Some(&b), std::slice::from_ref(&b)).unwrap();
        assert_eq!(cfg.slave_id, 2);
        assert_eq!(src, ConfigSource::File(a.clone()));
        // Env wins over implicit.
        let (cfg, _) = Config::discover_in(None, Some(&b), std::slice::from_ref(&a)).unwrap();
        assert_eq!(cfg.slave_id, 3);
        // Missing explicit file is an error.
        assert!(Config::discover_in(Some(&missing), None, &[]).is_err());
        // Missing implicit files fall back to defaults.
        let (cfg, src) = Config::discover_in(None, None, std::slice::from_ref(&missing)).unwrap();
        assert_eq!(cfg, Config::default());
        assert_eq!(src, ConfigSource::Defaults);
        // First existing implicit file is used.
        let (cfg, _) = Config::discover_in(None, None, &[missing, b.clone(), a]).unwrap();
        assert_eq!(cfg.slave_id, 3);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_fields() {
        let text = "unknown_field = true";
        assert!(Config::from_str(text).is_err());
    }
}
