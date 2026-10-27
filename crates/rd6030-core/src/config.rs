//! Application configuration, loaded from a TOML file.
//!
//! All fields have sane defaults (see [`Config::default`]) so that a minimal
//! or empty configuration file is accepted. The device is always operated
//! with the fixed serial framing 8N1, so data bits, parity and stop bits are
//! *not* part of the configuration.

use std::path::Path;

use serde::{Deserialize, Serialize};

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
            server: ServerConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
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
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: 8080,
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
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            path: "rd6030-telemetry.csv".to_string(),
            format: LogFormat::Csv,
            interval_ms: 1000,
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

impl Config {
    /// Load and validate a configuration from a TOML file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)?;
        text.parse()
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
    fn rejects_unknown_fields() {
        let text = "unknown_field = true";
        assert!(Config::from_str(text).is_err());
    }
}
