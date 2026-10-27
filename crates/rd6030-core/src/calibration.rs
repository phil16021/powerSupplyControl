//! Calibration support — **dangerous**, writes non-volatile NVRAM.
//!
//! Calibration offset/gain values live in registers 55 and above; the exact
//! layout is device-specific ("interne DAC/ADC-Stützpunkte"). Changes only
//! take effect permanently after writing the commit magic `0x1501` to
//! register 54. Repeated NVRAM writes degrade the device's internal flash, so
//! calibration writes should be used sparingly and never from sweep/ramp
//! automation.

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// First calibration offset/gain register address.
pub const CALIBRATION_FIRST_REGISTER: u16 = 55;

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

    /// Read a raw calibration register (addresses >= 55).
    pub async fn read_calibration_register(&self, addr: u16) -> Result<u16, DeviceError> {
        if addr < CALIBRATION_FIRST_REGISTER {
            return Err(DeviceError::OutOfRange(format!(
                "calibration register {addr} is below {CALIBRATION_FIRST_REGISTER}"
            )));
        }
        let regs = self.device().read_holding(addr, 1).await?;
        regs.first().copied().ok_or(DeviceError::InvalidData)
    }

    /// Write a raw calibration register (addresses >= 55). Not committed to
    /// NVRAM until [`commit_calibration`](Self::commit_calibration) is called.
    pub async fn write_calibration_register(
        &self,
        addr: u16,
        value: u16,
    ) -> Result<(), DeviceError> {
        if addr < CALIBRATION_FIRST_REGISTER {
            return Err(DeviceError::OutOfRange(format!(
                "calibration register {addr} is below {CALIBRATION_FIRST_REGISTER}"
            )));
        }
        self.device().write_single(addr, value).await
    }
}
