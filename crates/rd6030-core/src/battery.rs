//! Battery charging mode status (read-only; registers 0x0020..=0x0029).

use serde::Serialize;

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// A snapshot of the battery charging mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct BatteryStatus {
    /// Whether the battery charging mode is active.
    pub charging: bool,
    /// External battery voltage in volts.
    pub voltage: f32,
    /// External temperature probe reading in degrees Celsius.
    pub temperature_c: f32,
    /// External temperature probe reading in degrees Fahrenheit.
    pub temperature_f: f32,
    /// Accumulated charged capacity in ampere-hours.
    pub capacity_ah: f32,
    /// Accumulated charged energy in watt-hours.
    pub energy_wh: f32,
}

impl BatteryStatus {
    /// Decode a battery snapshot read from registers 0x0020..=0x0029.
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < 10 {
            return None;
        }
        Some(Self {
            charging: regs[0] == 1,
            voltage: register::raw_to_voltage(regs[1]),
            temperature_c: register::raw_to_temperature(regs[2], regs[3]),
            temperature_f: register::raw_to_temperature(regs[4], regs[5]),
            capacity_ah: register::raw_to_capacity(register::combine_u32(regs[6], regs[7])),
            energy_wh: register::raw_to_energy(register::combine_u32(regs[8], regs[9])),
        })
    }
}

impl Controller {
    /// Read battery charging status (a single 10-register block read).
    pub async fn battery_status(&self) -> Result<BatteryStatus, DeviceError> {
        let regs = self
            .device()
            .read_holding(register::reg::BATTERY_STATUS, 10)
            .await?;
        BatteryStatus::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn battery_decodes_block() {
        // charging=1, vbat 13.2V, temp +25C / -5F, capacity 1500mAh, energy 12340 mWh
        let regs = [1, 1320, 0, 25, 1, 5, 0x0000, 1500, 0x0000, 12340];
        let b = BatteryStatus::from_regs(&regs).expect("valid");
        assert!(b.charging);
        assert_eq!(b.voltage, 13.2);
        assert_eq!(b.temperature_c, 25.0);
        assert_eq!(b.temperature_f, -5.0);
        assert_eq!(b.capacity_ah, 1.5);
        assert_eq!(b.energy_wh, 12.34);
    }

    #[test]
    fn battery_rejects_short_snapshot() {
        assert!(BatteryStatus::from_regs(&[1, 2, 3]).is_none());
    }
}
