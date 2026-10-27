//! Calibration support — **dangerous**, writes non-volatile NVRAM.
//!
//! Calibration offset/gain words live in registers 55..=62 (V_OUT, V_BACK,
//! I_OUT and I_BACK zero/scale). Raw access is restricted to exactly that
//! range. Repeated NVRAM writes degrade the device's internal flash, so
//! calibration writes should be used sparingly and never from sweep/ramp
//! automation.
//!
//! The commit sequence (magic `0x1501` written to register 54) comes from the
//! Gemini report and is **not** part of the community register map.
//
// VERIFY(HIL): commit register/magic before relying on `commit_calibration`.

use std::ops::RangeInclusive;

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// Registers that hold calibration words.
pub const CALIBRATION_REGISTERS: RangeInclusive<u16> =
    register::reg::CALIBRATION_FIRST..=register::reg::CALIBRATION_LAST;

/// Names of the calibration words, in register order.
pub const CALIBRATION_NAMES: [&str; 8] = [
    "v_out_zero",
    "v_out_scale",
    "v_back_zero",
    "v_back_scale",
    "i_out_zero",
    "i_out_scale",
    "i_back_zero",
    "i_back_scale",
];

fn check_addr(addr: u16) -> Result<(), DeviceError> {
    if !CALIBRATION_REGISTERS.contains(&addr) {
        return Err(DeviceError::OutOfRange(format!(
            "calibration register {addr} outside {}..={}",
            CALIBRATION_REGISTERS.start(),
            CALIBRATION_REGISTERS.end()
        )));
    }
    Ok(())
}

impl Controller {
    /// Persist pending calibration changes to NVRAM by writing the commit
    /// magic `0x1501` to register 54.
    ///
    /// Call this exactly once at the end of a calibration session.
    pub async fn commit_calibration(&self) -> Result<(), DeviceError> {
        self.device()
            .write_single(
                register::reg::CALIBRATION_COMMIT,
                register::CALIBRATION_COMMIT_VALUE,
            )
            .await
    }

    /// Read all calibration words (registers 55..=62) as `(name, addr, value)`.
    pub async fn read_calibration(&self) -> Result<Vec<(&'static str, u16, u16)>, DeviceError> {
        let first = *CALIBRATION_REGISTERS.start();
        let regs = self
            .device()
            .read_holding(first, CALIBRATION_NAMES.len() as u16)
            .await?;
        if regs.len() != CALIBRATION_NAMES.len() {
            return Err(DeviceError::InvalidData);
        }
        Ok(CALIBRATION_NAMES
            .iter()
            .zip(regs)
            .enumerate()
            .map(|(i, (name, value))| (*name, first + i as u16, value))
            .collect())
    }

    /// Read a raw calibration register (55..=62).
    pub async fn read_calibration_register(&self, addr: u16) -> Result<u16, DeviceError> {
        check_addr(addr)?;
        let regs = self.device().read_holding(addr, 1).await?;
        regs.first().copied().ok_or(DeviceError::InvalidData)
    }

    /// Write a raw calibration register (55..=62). Not committed to NVRAM
    /// until [`commit_calibration`](Self::commit_calibration) is called.
    pub async fn write_calibration_register(
        &self,
        addr: u16,
        value: u16,
    ) -> Result<(), DeviceError> {
        check_addr(addr)?;
        self.device().write_single(addr, value).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Device, DriverOptions, MockBackend};

    #[tokio::test]
    async fn calibration_access_is_bounded() {
        let mock = MockBackend::rd6030();
        let c = Controller::from_device(Device::start(
            mock.connector(),
            DriverOptions::default().without_polling(),
        ));
        c.write_calibration_register(55, 7).await.unwrap();
        assert_eq!(c.read_calibration_register(55).await.unwrap(), 7);
        for addr in [54, 63, 80, register::reg::BOOTLOADER] {
            assert!(c.write_calibration_register(addr, 1).await.is_err());
            assert!(c.read_calibration_register(addr).await.is_err());
        }
        let all = c.read_calibration().await.unwrap();
        assert_eq!(all[0], ("v_out_zero", 55, 7));
        assert_eq!(all.len(), 8);
    }
}
