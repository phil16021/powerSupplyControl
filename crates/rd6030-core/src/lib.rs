//! `rd6030-core` — asynchronous control library for the Riden RD6030
//! programmable DC power supply.
//!
//! The library is split into a pure register/decoding layer
//! ([`register`], [`model`]) and an actor-based device driver
//! ([`driver`], [`controller`]). Applications interact with the device
//! exclusively through [`Controller`].

pub mod battery;
pub mod calibration;
pub mod config;
pub mod controller;
pub mod driver;
pub mod error;
pub mod logging;
pub mod model;
pub mod preset;
pub mod register;
pub mod rtc;

pub use battery::BatteryStatus;
pub use controller::Controller;
pub use driver::{Device, MockBackend, ModbusBackend, RtuBackend};
pub use error::DeviceError;
pub use logging::TelemetryLogger;
pub use model::{DeviceInfo, DeviceTelemetry, ProtectionStatus};
pub use preset::{Setpoint, PRESET_COUNT};
pub use rtc::RtcTime;
