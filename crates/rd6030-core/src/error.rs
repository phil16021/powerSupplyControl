//! Error types for the RD6030 device driver.

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

    /// Modbus transport/protocol error (timeout, framing, mismatch).
    #[error("modbus transport error: {0}")]
    Modbus(#[from] tokio_modbus::Error),

    /// Modbus exception returned by the device.
    #[error("modbus exception: {0}")]
    Exception(#[from] tokio_modbus::ExceptionCode),

    /// The response data was invalid or incomplete.
    #[error("response data invalid or incomplete")]
    InvalidData,

    /// A requested value was outside the permitted range.
    #[error("value out of range: {0}")]
    OutOfRange(String),

    /// An operation timed out (e.g. a serial read timeout; note that
    /// tokio-modbus wraps serial timeouts as transport `io::Error`s).
    #[error("operation timed out: {0}")]
    Timeout(String),

    /// The device actor is no longer running.
    #[error("device disconnected")]
    Disconnected,
}
