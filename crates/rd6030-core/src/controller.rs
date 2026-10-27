//! High-level, type-safe control interface for the RD6030.

use std::time::Duration;

use tokio::sync::watch;
use tracing::warn;

use crate::config::{Config, LimitsConfig, SafetyConfig};
use crate::driver::{Device, DriverOptions};
use crate::error::DeviceError;
use crate::model::{DeviceInfo, DeviceTelemetry, LinkStatus, TelemetrySample};
use crate::register;

/// Shortest allowed interval between two ramp steps.
pub const MIN_RAMP_STEP_INTERVAL: Duration = Duration::from_millis(50);

/// Cloneable controller handle. All methods issue serialized requests through
/// the underlying [`Device`] actor.
#[derive(Clone, Debug)]
pub struct Controller {
    device: Device,
    limits: LimitsConfig,
}

impl Controller {
    /// Connect to a real device using the given configuration (fails if the
    /// device does not answer the identity handshake).
    pub async fn connect(config: &Config) -> Result<Self, DeviceError> {
        Self::connect_with(config, DriverOptions::from_config(config)).await
    }

    /// Like [`connect`](Self::connect) with explicit driver options (e.g.
    /// polling disabled for one-shot commands).
    pub async fn connect_with(
        config: &Config,
        options: DriverOptions,
    ) -> Result<Self, DeviceError> {
        Ok(
            Self::from_device(Device::connect_with(config, options).await?)
                .with_limits(config.limits),
        )
    }

    /// Start the driver without requiring the device to be present; the
    /// actor keeps reconnecting in the background (servers, GUI).
    pub fn start(config: &Config) -> Self {
        let device = Device::start(
            crate::driver::SerialConnector::from_config(config),
            DriverOptions::from_config(config),
        );
        Self::from_device(device).with_limits(config.limits)
    }

    /// Wrap an existing [`Device`] handle (device-rating limits).
    pub fn from_device(device: Device) -> Self {
        Self {
            device,
            limits: LimitsConfig::default(),
        }
    }

    /// Replace the software limits.
    pub fn with_limits(mut self, limits: LimitsConfig) -> Self {
        self.limits = limits;
        self
    }

    /// The active software limits.
    pub fn limits(&self) -> LimitsConfig {
        self.limits
    }

    /// Access the underlying device handle.
    pub fn device(&self) -> &Device {
        &self.device
    }

    /// Current link state and driver statistics.
    pub fn link(&self) -> LinkStatus {
        self.device.link()
    }

    /// Subscribe to link-state changes.
    pub fn subscribe_link(&self) -> watch::Receiver<LinkStatus> {
        self.device.subscribe_link()
    }

    /// Read device identity and temperatures in a single register block.
    pub async fn info(&self) -> Result<DeviceInfo, DeviceError> {
        let regs = self
            .device
            .read_holding(register::INFO_START, register::INFO_COUNT)
            .await?;
        DeviceInfo::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    async fn read_one(&self, addr: u16) -> Result<u16, DeviceError> {
        let regs = self.device.read_holding(addr, 1).await?;
        regs.first().copied().ok_or(DeviceError::InvalidData)
    }

    /// Read the configured voltage setpoint in volts.
    pub async fn voltage_setpoint(&self) -> Result<f32, DeviceError> {
        Ok(register::raw_to_voltage(
            self.read_one(register::reg::VSET).await?,
        ))
    }

    /// Read the configured current setpoint in amperes.
    pub async fn current_setpoint(&self) -> Result<f32, DeviceError> {
        Ok(register::raw_to_current(
            self.read_one(register::reg::ISET).await?,
        ))
    }

    /// Validate a voltage against the software limit (and the rating).
    pub fn check_voltage(&self, volts: f32) -> Result<u16, DeviceError> {
        check_range("voltage", "V", volts, self.limits.max_voltage)?;
        Ok(register::voltage_to_raw(volts))
    }

    /// Validate a current against the software limit (and the rating).
    pub fn check_current(&self, amperes: f32) -> Result<u16, DeviceError> {
        check_range("current", "A", amperes, self.limits.max_current)?;
        Ok(register::current_to_raw(amperes))
    }

    /// Set the voltage setpoint in volts (volatile register 8).
    pub async fn set_voltage(&self, volts: f32) -> Result<(), DeviceError> {
        let raw = self.check_voltage(volts)?;
        self.warn_if_above_input(volts);
        self.device.write_single(register::reg::VSET, raw).await
    }

    /// Set the current setpoint in amperes (volatile register 9).
    pub async fn set_current(&self, amperes: f32) -> Result<(), DeviceError> {
        let raw = self.check_current(amperes)?;
        self.device.write_single(register::reg::ISET, raw).await
    }

    /// The RD60xx cannot regulate above roughly Vin / 1.1.
    pub(crate) fn warn_if_above_input(&self, volts: f32) {
        if let Some(sample) = self.device.latest() {
            let vin = sample.telemetry.voltage_in;
            if vin > 0.0 && volts > vin / 1.1 {
                warn!(
                    setpoint = volts,
                    vin, "voltage setpoint exceeds Vin/1.1; the output will not reach it"
                );
            }
        }
    }

    /// Whether the output is currently enabled.
    pub async fn output_enabled(&self) -> Result<bool, DeviceError> {
        Ok(self.read_one(register::reg::OUTPUT).await? == 1)
    }

    /// Enable or disable the output.
    pub async fn set_output(&self, enabled: bool) -> Result<(), DeviceError> {
        self.device
            .write_single(register::reg::OUTPUT, u16::from(enabled))
            .await
    }

    /// Whether the front-panel keypad is locked.
    pub async fn keypad_locked(&self) -> Result<bool, DeviceError> {
        Ok(self.read_one(register::reg::KEYPAD_LOCK).await? == 1)
    }

    /// Lock or unlock the front-panel keypad.
    pub async fn set_keypad_lock(&self, locked: bool) -> Result<(), DeviceError> {
        self.device
            .write_single(register::reg::KEYPAD_LOCK, u16::from(locked))
            .await
    }

    /// A telemetry snapshot. Returns the background-polled sample when it is
    /// fresh (younger than two poll intervals), otherwise reads the bus.
    pub async fn telemetry_sample(&self) -> Result<TelemetrySample, DeviceError> {
        if let (Some(sample), Some(interval)) = (self.device.latest(), self.device.poll_interval())
        {
            if sample.age() <= interval * 2 {
                return Ok(sample);
            }
        }
        self.device.read_telemetry().await
    }

    /// Like [`telemetry_sample`](Self::telemetry_sample), without metadata.
    pub async fn telemetry(&self) -> Result<DeviceTelemetry, DeviceError> {
        Ok(self.telemetry_sample().await?.telemetry)
    }

    /// Subscribe to background telemetry updates (`None` until the first
    /// sample arrives).
    pub fn subscribe(&self) -> watch::Receiver<Option<TelemetrySample>> {
        self.device.subscribe()
    }

    /// The most recently polled telemetry sample (no I/O).
    pub fn latest(&self) -> Option<TelemetrySample> {
        self.device.latest()
    }

    /// Read raw holding registers (debugging).
    pub async fn read_raw(&self, addr: u16, count: u16) -> Result<Vec<u16>, DeviceError> {
        if count == 0 || count > 125 {
            return Err(DeviceError::OutOfRange(format!(
                "register count {count} outside 1..=125"
            )));
        }
        self.device.read_holding(addr, count).await
    }

    /// Write raw holding registers (debugging). Only documented writable
    /// registers are accepted (see [`register::is_writable_range`]); this
    /// bypasses the software limits, so setpoint registers are rejected here
    /// unless they pass the limit check.
    pub async fn write_raw(&self, addr: u16, values: &[u16]) -> Result<(), DeviceError> {
        let count = u16::try_from(values.len()).unwrap_or(u16::MAX);
        if !register::is_writable_range(addr, count) || count > 123 {
            return Err(DeviceError::Forbidden(format!(
                "raw write of {count} register(s) at {addr} is not allowed"
            )));
        }
        for (i, &value) in values.iter().enumerate() {
            let a = addr + i as u16;
            if a == register::reg::VSET {
                self.check_voltage(register::raw_to_voltage(value))?;
            } else if a == register::reg::ISET {
                self.check_current(register::raw_to_current(value))?;
            }
        }
        match values {
            [value] => self.device.write_single(addr, *value).await,
            _ => self.device.write_multiple(addr, values).await,
        }
    }

    /// Ramp the voltage setpoint to `target` in steps of at most `step` volts,
    /// one step per `interval` (>= 50 ms). Uses only the volatile setpoint
    /// register, never the preset bank.
    pub async fn ramp_voltage(
        &self,
        target: f32,
        step: f32,
        interval: Duration,
    ) -> Result<(), DeviceError> {
        self.check_voltage(target)?;
        let start = self.voltage_setpoint().await?;
        let raws = ramp_steps(
            register::voltage_to_raw(start),
            register::voltage_to_raw(target),
            step_raw(step, register::VOLTAGE_SCALE)?,
        );
        self.run_ramp(register::reg::VSET, raws, interval).await
    }

    /// Ramp the current setpoint (see [`ramp_voltage`](Self::ramp_voltage)).
    pub async fn ramp_current(
        &self,
        target: f32,
        step: f32,
        interval: Duration,
    ) -> Result<(), DeviceError> {
        self.check_current(target)?;
        let start = self.current_setpoint().await?;
        let raws = ramp_steps(
            register::current_to_raw(start),
            register::current_to_raw(target),
            step_raw(step, register::CURRENT_SCALE)?,
        );
        self.run_ramp(register::reg::ISET, raws, interval).await
    }

    async fn run_ramp(
        &self,
        addr: u16,
        raws: Vec<u16>,
        interval: Duration,
    ) -> Result<(), DeviceError> {
        if interval < MIN_RAMP_STEP_INTERVAL {
            return Err(DeviceError::OutOfRange(format!(
                "ramp interval {interval:?} below {MIN_RAMP_STEP_INTERVAL:?}"
            )));
        }
        let mut ticker = tokio::time::interval(interval);
        for raw in raws {
            ticker.tick().await;
            self.device.write_single(addr, raw).await?;
        }
        Ok(())
    }

    /// Stop the underlying device actor.
    pub async fn shutdown(&self) {
        self.device.shutdown().await;
    }

    /// Apply the exit-time safety policy (`output_off_on_exit`), then stop
    /// the driver.
    pub async fn shutdown_safely(&self, safety: &SafetyConfig) {
        if safety.output_off_on_exit {
            match self.set_output(false).await {
                Ok(()) => tracing::info!("output switched off on exit"),
                Err(err) => warn!(error = %err, "could not switch output off on exit"),
            }
        }
        self.shutdown().await;
    }
}

/// Reject NaN/negative values and values above `max`.
pub(crate) fn check_range(name: &str, unit: &str, value: f32, max: f32) -> Result<(), DeviceError> {
    if !(0.0..=max).contains(&value) {
        return Err(DeviceError::OutOfRange(format!(
            "{name} {value:.3} {unit} outside 0..={max:.2} {unit}"
        )));
    }
    Ok(())
}

fn step_raw(step: f32, scale: f32) -> Result<u16, DeviceError> {
    let raw = (step * scale).round();
    if !(1.0..=u16::MAX as f32).contains(&raw) {
        return Err(DeviceError::OutOfRange(format!(
            "ramp step {step} must be at least {}",
            1.0 / scale
        )));
    }
    Ok(raw as u16)
}

/// Intermediate raw values from `start` (exclusive) to `target` (inclusive).
fn ramp_steps(start: u16, target: u16, step: u16) -> Vec<u16> {
    let mut out = Vec::new();
    let mut v = start;
    while v != target {
        v = if target > v {
            v.saturating_add(step).min(target)
        } else {
            v.saturating_sub(step).max(target)
        };
        out.push(v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::MockBackend;

    fn controller(mock: &MockBackend) -> Controller {
        let options = DriverOptions {
            poll_interval: None,
            ..DriverOptions::default()
        };
        Controller::from_device(Device::start(mock.connector(), options))
    }

    #[tokio::test]
    async fn output_off_on_exit() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        c.set_output(true).await.unwrap();
        c.shutdown_safely(&SafetyConfig {
            output_off_on_exit: true,
        })
        .await;
        assert_eq!(mock.register(register::reg::OUTPUT), 0);
    }

    #[test]
    fn ramp_step_sequence() {
        assert_eq!(ramp_steps(0, 500, 200), vec![200, 400, 500]);
        assert_eq!(ramp_steps(500, 0, 200), vec![300, 100, 0]);
        assert!(ramp_steps(7, 7, 1).is_empty());
    }

    #[tokio::test]
    async fn limits_are_enforced() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock).with_limits(LimitsConfig {
            max_voltage: 13.0,
            max_current: 2.0,
        });
        c.set_voltage(12.5).await.unwrap();
        assert_eq!(mock.register(register::reg::VSET), 1250);
        assert!(matches!(
            c.set_voltage(13.5).await,
            Err(DeviceError::OutOfRange(_))
        ));
        assert!(c.set_current(2.5).await.is_err());
        assert!(c.set_voltage(f32::NAN).await.is_err());
        assert!(c.set_voltage(-1.0).await.is_err());
        // Raw writes cannot bypass the limits.
        assert!(c.write_raw(register::reg::VSET, &[1400]).await.is_err());
        assert_eq!(mock.register(register::reg::VSET), 1250);
    }

    #[tokio::test]
    async fn raw_write_denylist() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        assert!(matches!(
            c.write_raw(register::reg::BOOTLOADER, &[0x1601]).await,
            Err(DeviceError::Forbidden(_))
        ));
        assert!(c.write_raw(register::reg::MODEL, &[1]).await.is_err());
        c.write_raw(register::reg::OUTPUT, &[1]).await.unwrap();
        assert_eq!(mock.register(register::reg::OUTPUT), 1);
    }

    #[tokio::test]
    async fn keypad_lock_round_trips() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        c.set_keypad_lock(true).await.unwrap();
        assert!(c.keypad_locked().await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn ramp_writes_every_step() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        c.set_voltage(1.0).await.unwrap();
        c.ramp_voltage(2.0, 0.25, Duration::from_millis(50))
            .await
            .unwrap();
        assert_eq!(mock.register(register::reg::VSET), 200);
        assert!(c
            .ramp_voltage(3.0, 0.25, Duration::from_millis(10))
            .await
            .is_err());
        assert!(c
            .ramp_voltage(3.0, 0.0, Duration::from_millis(50))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn telemetry_uses_fresh_cache() {
        let mock = MockBackend::rd6030();
        let options = DriverOptions {
            poll_interval: Some(Duration::from_secs(3600)),
            ..DriverOptions::default()
        };
        let c = Controller::from_device(Device::start(mock.connector(), options));
        let first = c.telemetry_sample().await.unwrap();
        let before = mock.request_count();
        let second = c.telemetry_sample().await.unwrap();
        assert_eq!(first.seq, second.seq);
        assert_eq!(mock.request_count(), before);
    }
}
