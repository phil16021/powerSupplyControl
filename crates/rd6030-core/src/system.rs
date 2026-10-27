//! System options (registers 66..=72): confirmation behaviour, power-on
//! output state, buzzer, boot logo, language and backlight.
//!
//! These are persisted by the device (EEPROM) — change them sparingly.
//
// VERIFY(HIL): option semantics and the backlight range taken from the
// community register map.

use serde::{Deserialize, Serialize};

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// Highest accepted backlight level.
pub const MAX_BACKLIGHT: u8 = 5;

/// Snapshot of the system options.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemOptions {
    /// Ask for confirmation when recalling a preset ("take ok").
    pub take_ok: bool,
    /// Switch the output on after recalling a preset ("take out").
    pub take_out: bool,
    /// Restore the output state at power-on.
    pub boot_power: bool,
    /// Key beep enabled.
    pub buzzer: bool,
    /// Boot logo shown.
    pub logo: bool,
    /// UI language index.
    pub language: u16,
    /// Display backlight level (0..=5).
    pub backlight: u8,
}

impl SystemOptions {
    /// Decode registers 66..=72.
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < 7 {
            return None;
        }
        Some(Self {
            take_ok: regs[0] == 1,
            take_out: regs[1] == 1,
            boot_power: regs[2] == 1,
            buzzer: regs[3] == 1,
            logo: regs[4] == 1,
            language: regs[5],
            backlight: u8::try_from(regs[6]).unwrap_or(u8::MAX),
        })
    }
}

impl Controller {
    /// Read the system options.
    pub async fn system_options(&self) -> Result<SystemOptions, DeviceError> {
        let regs = self
            .device()
            .read_holding(register::reg::OPT_TAKE_OK, 7)
            .await?;
        SystemOptions::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    /// Enable or disable the key beep.
    pub async fn set_buzzer(&self, enabled: bool) -> Result<(), DeviceError> {
        self.device()
            .write_single(register::reg::OPT_BUZZER, u16::from(enabled))
            .await
    }

    /// Set the display backlight level (0..=5).
    pub async fn set_backlight(&self, level: u8) -> Result<(), DeviceError> {
        if level > MAX_BACKLIGHT {
            return Err(DeviceError::OutOfRange(format!(
                "backlight {level} outside 0..={MAX_BACKLIGHT}"
            )));
        }
        self.device()
            .write_single(register::reg::OPT_BACKLIGHT, u16::from(level))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Device, DriverOptions, MockBackend};

    #[tokio::test]
    async fn options_round_trip() {
        let mock = MockBackend::rd6030();
        let c = Controller::from_device(Device::start(
            mock.connector(),
            DriverOptions::default().without_polling(),
        ));
        c.set_buzzer(true).await.unwrap();
        c.set_backlight(4).await.unwrap();
        assert!(c.set_backlight(6).await.is_err());
        let o = c.system_options().await.unwrap();
        assert!(o.buzzer);
        assert_eq!(o.backlight, 4);
        assert!(!o.take_ok);
    }
}
