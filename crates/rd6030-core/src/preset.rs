//! Preset (memory slot) management.
//!
//! The RD6030 boots into profile M0. The *active* setpoint is held in the
//! volatile registers 8 (Vset) and 9 (Iset), and the active slot is selected
//! through register 19. Writing these registers affects the running profile
//! but is lost on a cold start unless persisted through the device-specific
//! non-volatile preset bank.
//!
//! # Non-volatile preset bank (M0..M9)
//!
//! The layout of the non-volatile preset bank is **not** part of the
//! documented holding-register map; it is accessed through a vendor-specific
//! memory command reverse-engineered by `rssdev10/riden-flashtool`
//! (`memory-export` / `memory-import`). Because the exact address/command
//! scheme is not published in the RD6030 register documentation, reading and
//! writing that bank is intentionally out of scope here: this module operates
//! exclusively on the documented volatile registers.

use serde::{Deserialize, Serialize};

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// The number of user preset slots (M0..M9).
pub const PRESET_COUNT: u8 = 10;

/// A user-editable voltage/current setpoint.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Setpoint {
    /// Voltage setpoint in volts.
    pub voltage: f32,
    /// Current setpoint in amperes.
    pub current: f32,
}

impl Setpoint {
    /// Serialize as pretty JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from JSON.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

impl Controller {
    /// Select an active preset slot (0..=9, corresponding to M0..M9).
    pub async fn select_preset(&self, slot: u8) -> Result<(), DeviceError> {
        if slot >= PRESET_COUNT {
            return Err(DeviceError::OutOfRange(format!(
                "preset slot {slot} outside 0..={}",
                PRESET_COUNT - 1
            )));
        }
        self.device()
            .write_single(register::reg::PRESET, u16::from(slot))
            .await
    }

    /// Read the currently selected preset slot.
    pub async fn active_preset(&self) -> Result<u8, DeviceError> {
        let regs = self.device().read_holding(register::reg::PRESET, 1).await?;
        Ok(regs.first().copied().ok_or(DeviceError::InvalidData)? as u8)
    }

    /// Read the active voltage/current setpoint.
    pub async fn setpoint(&self) -> Result<Setpoint, DeviceError> {
        let regs = self.device().read_holding(register::reg::VSET, 2).await?;
        if regs.len() < 2 {
            return Err(DeviceError::InvalidData);
        }
        Ok(Setpoint {
            voltage: register::raw_to_voltage(regs[0]),
            current: register::raw_to_current(regs[1]),
        })
    }

    /// Set the active voltage/current setpoint atomically (volatile registers
    /// 8 and 9 via one multi-register write).
    pub async fn set_setpoint(&self, setpoint: Setpoint) -> Result<(), DeviceError> {
        if !(0.0..=register::MAX_VOLTAGE).contains(&setpoint.voltage) {
            return Err(DeviceError::OutOfRange(format!(
                "voltage {:.3} V outside 0..={:.1} V",
                setpoint.voltage,
                register::MAX_VOLTAGE
            )));
        }
        if !(0.0..=register::MAX_CURRENT).contains(&setpoint.current) {
            return Err(DeviceError::OutOfRange(format!(
                "current {:.3} A outside 0..={:.1} A",
                setpoint.current,
                register::MAX_CURRENT
            )));
        }
        let values = [
            register::voltage_to_raw(setpoint.voltage),
            register::current_to_raw(setpoint.current),
        ];
        self.device()
            .write_multiple(register::reg::VSET, &values)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setpoint_json_round_trips() {
        let sp = Setpoint {
            voltage: 12.0,
            current: 2.5,
        };
        let json = sp.to_json().expect("serialize");
        let parsed = Setpoint::from_json(&json).expect("deserialize");
        assert_eq!(sp, parsed);
    }

    #[test]
    fn preset_count_is_ten() {
        assert_eq!(PRESET_COUNT, 10);
    }
}
