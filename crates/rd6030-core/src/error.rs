//! Error types for the RD6030 device driver.

use std::time::Duration;

use thiserror::Error;

/// Errors produced by device operations.
#[derive(Debug, Error)]
pub enum DeviceError {
    /// Serial port I/O error.
    #[error("serial I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Serial port open/configure error (from the `serialport`/`mio-serial` layer).
    #[error("serial port error: {0}")]
    Serial(#[from] tokio_serial::Error),

    /// Modbus transport/protocol error (framing, header/function mismatch).
    #[error("modbus transport error: {0}")]
    Modbus(#[from] tokio_modbus::Error),

    /// Modbus exception returned by the device.
    #[error("modbus exception: {0}")]
    Exception(#[from] tokio_modbus::ExceptionCode),

    /// The response data was invalid or incomplete.
    #[error("response data invalid or incomplete")]
    InvalidData,

    /// A requested value was outside the permitted range (device rating or
    /// configured software limit).
    #[error("value out of range: {0}")]
    OutOfRange(String),

    /// The operation is not permitted (e.g. a write to a protected register).
    #[error("operation not permitted: {0}")]
    Forbidden(String),

    /// The device did not answer within the configured timeout.
    #[error("device did not respond within {0:?}")]
    Timeout(Duration),

    /// The connected device is not a supported model.
    #[error(
        "unsupported device model id {0} (expected an RD6030; set allow_unknown_model to override)"
    )]
    UnsupportedModel(u16),

    /// The serial link is currently down; the driver keeps retrying in the
    /// background.
    #[error("device not connected: {0}")]
    NotConnected(String),

    /// The device actor is no longer running.
    #[error("device driver stopped")]
    Disconnected,
}

impl DeviceError {
    /// Whether this error indicates a broken transport, after which the
    /// Modbus framing state can no longer be trusted and the port must be
    /// reopened.
    pub fn is_transport(&self) -> bool {
        matches!(
            self,
            Self::Io(_) | Self::Serial(_) | Self::Modbus(_) | Self::Timeout(_)
        )
    }
}
