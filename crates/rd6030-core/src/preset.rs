//! Preset (memory slot) management and protection thresholds.
//!
//! The RD6030 boots into profile M0. The *active* setpoint is held in the
//! volatile registers 8 (Vset) and 9 (Iset), and the active slot is selected
//! through register 19.
//!
//! # Non-volatile preset bank (M0..M9)
//!
//! Each slot occupies four registers starting at `80 + 4·n`: voltage,
//! current, over-voltage and over-current protection (same scaling as the
//! setpoints). M0's OVP/OCP words are the *active* protection thresholds.
//!
//! Writing the bank stores to the device's EEPROM/flash, which has limited
//! write endurance: never write presets from sweep/ramp loops. The import
//! helpers only write slots whose contents actually change.
//
// VERIFY(HIL): layout taken from the community register map (Baldanos/rd6006).

use serde::{Deserialize, Serialize};

use crate::controller::{check_range, Controller};
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

/// One preset slot of the non-volatile bank, in physical units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preset {
    /// Voltage setpoint in volts.
    pub voltage: f32,
    /// Current setpoint in amperes.
    pub current: f32,
    /// Over-voltage protection threshold in volts.
    pub ovp: f32,
    /// Over-current protection threshold in amperes.
    pub ocp: f32,
}

impl Preset {
    /// Decode from the four slot registers.
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < register::PRESET_STRIDE as usize {
            return None;
        }
        Some(Self {
            voltage: register::raw_to_voltage(regs[0]),
            current: register::raw_to_current(regs[1]),
            ovp: register::raw_to_voltage(regs[2]),
            ocp: register::raw_to_current(regs[3]),
        })
    }

    /// Encode as the four slot registers.
    pub fn to_regs(self) -> [u16; 4] {
        [
            register::voltage_to_raw(self.voltage),
            register::current_to_raw(self.current),
            register::voltage_to_raw(self.ovp),
            register::current_to_raw(self.ocp),
        ]
    }
}

/// A full preset bank export (slots M0..M9).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresetBank {
    /// Slots indexed by number (`slots[0]` is M0).
    pub slots: Vec<Preset>,
}

impl PresetBank {
    /// Serialize as pretty JSON.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Parse from JSON.
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }
}

/// Active protection thresholds (M0 OVP/OCP).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Protection {
    /// Over-voltage protection threshold in volts.
    pub ovp: f32,
    /// Over-current protection threshold in amperes.
    pub ocp: f32,
}

fn check_slot(slot: u8) -> Result<(), DeviceError> {
    if slot >= PRESET_COUNT {
        return Err(DeviceError::OutOfRange(format!(
            "preset slot {slot} outside 0..={}",
            PRESET_COUNT - 1
        )));
    }
    Ok(())
}

impl Controller {
    /// Select an active preset slot (0..=9, corresponding to M0..M9).
    pub async fn select_preset(&self, slot: u8) -> Result<(), DeviceError> {
        check_slot(slot)?;
        self.device()
            .write_single(register::reg::PRESET, u16::from(slot))
            .await
    }

    /// Read the currently selected preset slot.
    pub async fn active_preset(&self) -> Result<u8, DeviceError> {
        let regs = self.device().read_holding(register::reg::PRESET, 1).await?;
        let raw = regs.first().copied().ok_or(DeviceError::InvalidData)?;
        u8::try_from(raw).map_err(|_| DeviceError::InvalidData)
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
        let values = [
            self.check_voltage(setpoint.voltage)?,
            self.check_current(setpoint.current)?,
        ];
        self.warn_if_above_input(setpoint.voltage);
        self.device()
            .write_multiple(register::reg::VSET, &values)
            .await
    }

    /// Read one slot of the non-volatile preset bank.
    pub async fn read_preset(&self, slot: u8) -> Result<Preset, DeviceError> {
        check_slot(slot)?;
        let regs = self
            .device()
            .read_holding(register::preset_base(slot), register::PRESET_STRIDE)
            .await?;
        Preset::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    /// Validate a preset against the software limits and protection bounds.
    pub fn check_preset(&self, preset: &Preset) -> Result<(), DeviceError> {
        self.check_voltage(preset.voltage)?;
        self.check_current(preset.current)?;
        check_range("ovp", "V", preset.ovp, register::MAX_OVP)?;
        check_range("ocp", "A", preset.ocp, register::MAX_OCP)?;
        Ok(())
    }

    /// Write one slot of the non-volatile preset bank (EEPROM write — use
    /// sparingly). Skips the write if the slot already holds these values.
    /// Returns whether a write happened.
    pub async fn write_preset(&self, slot: u8, preset: Preset) -> Result<bool, DeviceError> {
        check_slot(slot)?;
        self.check_preset(&preset)?;
        let wanted = preset.to_regs();
        let current = self
            .device()
            .read_holding(register::preset_base(slot), register::PRESET_STRIDE)
            .await?;
        if current == wanted {
            return Ok(false);
        }
        self.device()
            .write_multiple(register::preset_base(slot), &wanted)
            .await?;
        Ok(true)
    }

    /// Export the whole preset bank (one 40-register read).
    pub async fn export_presets(&self) -> Result<PresetBank, DeviceError> {
        let regs = self
            .device()
            .read_holding(
                register::reg::PRESET_BANK,
                register::PRESET_STRIDE * u16::from(PRESET_COUNT),
            )
            .await?;
        let slots = regs
            .chunks(register::PRESET_STRIDE as usize)
            .map(|c| Preset::from_regs(c).ok_or(DeviceError::InvalidData))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PresetBank { slots })
    }

    /// Import a preset bank, writing only slots that differ. All slots are
    /// validated before anything is written. Returns the written slot numbers.
    pub async fn import_presets(&self, bank: &PresetBank) -> Result<Vec<u8>, DeviceError> {
        if bank.slots.len() > PRESET_COUNT as usize {
            return Err(DeviceError::OutOfRange(format!(
                "{} slots given, at most {PRESET_COUNT} allowed",
                bank.slots.len()
            )));
        }
        for preset in &bank.slots {
            self.check_preset(preset)?;
        }
        let mut written = Vec::new();
        for (slot, preset) in bank.slots.iter().enumerate() {
            let slot = slot as u8;
            if self.write_preset(slot, *preset).await? {
                written.push(slot);
            }
        }
        Ok(written)
    }

    /// Read the active protection thresholds (M0 OVP/OCP).
    pub async fn protection(&self) -> Result<Protection, DeviceError> {
        let regs = self.device().read_holding(register::reg::M0_OVP, 2).await?;
        if regs.len() < 2 {
            return Err(DeviceError::InvalidData);
        }
        Ok(Protection {
            ovp: register::raw_to_voltage(regs[0]),
            ocp: register::raw_to_current(regs[1]),
        })
    }

    /// Set the active protection thresholds (writes M0 OVP/OCP, EEPROM).
    pub async fn set_protection(&self, protection: Protection) -> Result<(), DeviceError> {
        check_range("ovp", "V", protection.ovp, register::MAX_OVP)?;
        check_range("ocp", "A", protection.ocp, register::MAX_OCP)?;
        let values = [
            register::voltage_to_raw(protection.ovp),
            register::current_to_raw(protection.ocp),
        ];
        self.device()
            .write_multiple(register::reg::M0_OVP, &values)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{Device, DriverOptions, MockBackend};

    fn controller(mock: &MockBackend) -> Controller {
        Controller::from_device(Device::start(
            mock.connector(),
            DriverOptions::default().without_polling(),
        ))
    }

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

    #[test]
    fn preset_register_round_trip() {
        let p = Preset {
            voltage: 5.0,
            current: 1.25,
            ovp: 5.5,
            ocp: 1.5,
        };
        assert_eq!(p.to_regs(), [500, 125, 550, 150]);
        assert_eq!(Preset::from_regs(&p.to_regs()), Some(p));
    }

    #[tokio::test]
    async fn preset_bank_export_import_writes_only_changes() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        let mut bank = c.export_presets().await.unwrap();
        assert_eq!(bank.slots.len(), 10);

        bank.slots[3] = Preset {
            voltage: 12.0,
            current: 2.0,
            ovp: 13.0,
            ocp: 2.5,
        };
        let json = bank.to_json().unwrap();
        let parsed = PresetBank::from_json(&json).unwrap();
        assert_eq!(c.import_presets(&parsed).await.unwrap(), vec![3]);
        assert_eq!(mock.register(register::preset_base(3)), 1200);
        // Re-importing the same bank writes nothing.
        assert!(c.import_presets(&parsed).await.unwrap().is_empty());
        assert_eq!(c.read_preset(3).await.unwrap(), parsed.slots[3]);
    }

    #[tokio::test]
    async fn invalid_preset_aborts_import_before_writing() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        let good = Preset {
            voltage: 1.0,
            current: 1.0,
            ovp: 2.0,
            ocp: 2.0,
        };
        let bad = Preset {
            voltage: 70.0,
            ..good
        };
        let bank = PresetBank {
            slots: vec![good, bad],
        };
        assert!(c.import_presets(&bank).await.is_err());
        assert_eq!(mock.register(register::preset_base(0)), 0);
        assert!(c.read_preset(10).await.is_err());
    }

    #[tokio::test]
    async fn protection_round_trips() {
        let mock = MockBackend::rd6030();
        let c = controller(&mock);
        let p = Protection {
            ovp: 13.2,
            ocp: 3.1,
        };
        c.set_protection(p).await.unwrap();
        assert_eq!(mock.register(register::reg::M0_OVP), 1320);
        assert_eq!(c.protection().await.unwrap(), p);
        assert!(c
            .set_protection(Protection {
                ovp: 70.0,
                ocp: 1.0
            })
            .await
            .is_err());
    }
}
