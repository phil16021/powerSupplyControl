//! High-level, type-safe control interface for the RD6030.

use tokio::sync::watch;

use crate::config::Config;
use crate::driver::Device;
use crate::error::DeviceError;
use crate::model::{DeviceInfo, DeviceTelemetry};
use crate::register;

/// Cloneable controller handle. All methods issue serialized requests through
/// the underlying [`Device`] actor.
#[derive(Clone, Debug)]
pub struct Controller {
    device: Device,
}

impl Controller {
    /// Connect to a real device using the given configuration.
    pub fn connect(config: &Config) -> Result<Self, DeviceError> {
        Ok(Self {
            device: Device::connect(config)?,
        })
    }

    /// Wrap an existing [`Device`] handle.
    pub fn from_device(device: Device) -> Self {
        Self { device }
    }

    /// Access the underlying device handle (raw register access).
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Read device identity and temperatures in a single register block.
    pub async fn info(&self) -> Result<DeviceInfo, DeviceError> {
        let regs = self
            .device
            .read_holding(register::INFO_START, register::INFO_COUNT)
            .await?;
        DeviceInfo::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    /// Read the raw model identifier.
    pub async fn model(&self) -> Result<u16, DeviceError> {
        Ok(self.info().await?.model)
    }

    /// Read the serial number.
    pub async fn serial_number(&self) -> Result<u32, DeviceError> {
        Ok(self.info().await?.serial_number)
    }

    /// Read the firmware version.
    pub async fn firmware_version(&self) -> Result<f32, DeviceError> {
        Ok(self.info().await?.firmware)
    }

    /// Read the internal temperature in degrees Celsius.
    pub async fn temperature_c(&self) -> Result<f32, DeviceError> {
        Ok(self.info().await?.temperature_c)
    }

    /// Read the internal temperature in degrees Fahrenheit.
    pub async fn temperature_f(&self) -> Result<f32, DeviceError> {
        Ok(self.info().await?.temperature_f)
    }

    /// Read the configured voltage setpoint in volts.
    pub async fn voltage_setpoint(&self) -> Result<f32, DeviceError> {
        let regs = self.device.read_holding(register::reg::VSET, 1).await?;
        Ok(register::raw_to_voltage(
            regs.first().copied().ok_or(DeviceError::InvalidData)?,
        ))
    }

    /// Read the configured current setpoint in amperes.
    pub async fn current_setpoint(&self) -> Result<f32, DeviceError> {
        let regs = self.device.read_holding(register::reg::ISET, 1).await?;
        Ok(register::raw_to_current(
            regs.first().copied().ok_or(DeviceError::InvalidData)?,
        ))
    }

    /// Set the voltage setpoint in volts (0..=60 V).
    pub async fn set_voltage(&self, volts: f32) -> Result<(), DeviceError> {
        if !(0.0..=register::MAX_VOLTAGE).contains(&volts) {
            return Err(DeviceError::OutOfRange(format!(
                "voltage {volts:.3} V outside 0..={:.1} V",
                register::MAX_VOLTAGE
            )));
        }
        self.device
            .write_single(register::reg::VSET, register::voltage_to_raw(volts))
            .await
    }

    /// Set the current setpoint in amperes (0..=30 A).
    pub async fn set_current(&self, amperes: f32) -> Result<(), DeviceError> {
        if !(0.0..=register::MAX_CURRENT).contains(&amperes) {
            return Err(DeviceError::OutOfRange(format!(
                "current {amperes:.3} A outside 0..={:.1} A",
                register::MAX_CURRENT
            )));
        }
        self.device
            .write_single(register::reg::ISET, register::current_to_raw(amperes))
            .await
    }

    /// Whether the output is currently enabled.
    pub async fn output_enabled(&self) -> Result<bool, DeviceError> {
        let regs = self.device.read_holding(register::reg::OUTPUT, 1).await?;
        Ok(regs.first().copied().ok_or(DeviceError::InvalidData)? == 1)
    }

    /// Enable or disable the output.
    pub async fn set_output(&self, enabled: bool) -> Result<(), DeviceError> {
        self.device
            .write_single(register::reg::OUTPUT, u16::from(enabled))
            .await
    }

    /// Read a single telemetry snapshot (one register-block read).
    pub async fn telemetry(&self) -> Result<DeviceTelemetry, DeviceError> {
        let regs = self
            .device
            .read_holding(register::TELEMETRY_START, register::TELEMETRY_COUNT)
            .await?;
        DeviceTelemetry::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    /// Subscribe to background telemetry updates.
    pub fn subscribe(&self) -> watch::Receiver<DeviceTelemetry> {
        self.device.subscribe()
    }

    /// Get the most recently polled telemetry value (no I/O).
    pub fn latest(&self) -> DeviceTelemetry {
        self.device.latest()
    }

    /// Stop the underlying device actor.
    pub async fn shutdown(&self) {
        self.device.shutdown().await;
    }
}
