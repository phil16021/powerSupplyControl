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
pub mod system;

pub use battery::BatteryStatus;
pub use config::Config;
pub use controller::Controller;
pub use driver::{
    Connector, Device, DriverOptions, Fault, FnConnector, MockBackend, ModbusBackend, RtuBackend,
    SerialConnector,
};
pub use error::DeviceError;
pub use logging::TelemetryLogger;
pub use model::{
    DeviceInfo, DeviceTelemetry, LinkState, LinkStatus, ProtectionStatus, TelemetrySample,
};
pub use preset::{Preset, PresetBank, Protection, Setpoint, PRESET_COUNT};
pub use rtc::RtcTime;
pub use system::SystemOptions;
