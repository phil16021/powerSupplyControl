//! Typed device state models, decoded from raw Modbus register snapshots.

use serde::Serialize;

use crate::register;

/// Device identity and internal temperature readings.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct DeviceInfo {
    /// Raw model identifier (e.g. `6030` or `60301`).
    pub model: u16,
    /// Device serial number.
    pub serial_number: u32,
    /// Firmware version (e.g. `1.32`).
    pub firmware: f32,
    /// Internal device temperature in degrees Celsius.
    pub temperature_c: f32,
    /// Internal device temperature in degrees Fahrenheit.
    pub temperature_f: f32,
}

impl DeviceInfo {
    /// Decode an info snapshot read from registers 0x0000..=0x0007.
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < register::INFO_COUNT as usize {
            return None;
        }
        Some(Self {
            model: regs[0],
            serial_number: register::combine_u32(regs[1], regs[2]),
            firmware: register::raw_to_firmware(regs[3]),
            temperature_c: register::raw_to_temperature(regs[4], regs[5]),
            temperature_f: register::raw_to_temperature(regs[6], regs[7]),
        })
    }
}

/// Protection status decoded from register 0x0010.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProtectionStatus {
    /// No protection active.
    #[default]
    Ok,
    /// Over-voltage protection.
    Ovp,
    /// Over-current protection.
    Ocp,
    /// Unknown / undocumented status code.
    Other,
}

impl ProtectionStatus {
    /// Decode a raw protection register value (0 = OK, 1 = OVP, 2 = OCP).
    pub fn from_raw(code: u16) -> Self {
        match code {
            0 => Self::Ok,
            1 => Self::Ovp,
            2 => Self::Ocp,
            _ => Self::Other,
        }
    }
}

/// A live telemetry snapshot (registers 0x000A..=0x0012).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize)]
pub struct DeviceTelemetry {
    /// Measured output voltage in volts.
    pub voltage_out: f32,
    /// Measured output current in amperes.
    pub current_out: f32,
    /// Measured output power in watts (32-bit).
    pub power_out: f32,
    /// Measured input voltage in volts.
    pub voltage_in: f32,
    /// Whether the front-panel keypad is locked.
    pub keypad_locked: bool,
    /// Decoded protection status.
    pub protection: ProtectionStatus,
    /// Whether the supply is in constant-current (rather than constant-voltage)
    /// regulation.
    pub constant_current: bool,
    /// Whether the output is enabled.
    pub output_enabled: bool,
}

impl DeviceTelemetry {
    /// Decode a telemetry snapshot read from registers 0x000A..=0x0012.
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < register::TELEMETRY_COUNT as usize {
            return None;
        }
        Some(Self {
            voltage_out: register::raw_to_voltage(regs[0]),
            current_out: register::raw_to_current(regs[1]),
            power_out: register::raw_to_power(register::combine_u32(regs[2], regs[3])),
            voltage_in: register::raw_to_voltage(regs[4]),
            keypad_locked: regs[5] == 1,
            protection: ProtectionStatus::from_raw(regs[6]),
            constant_current: regs[7] == 1,
            output_enabled: regs[8] == 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_decodes_contiguous_block() {
        let regs = [
            6030, // model
            0x0000, 0x1234, // serial = 0x1234
            132,    // firmware 1.32
            0, 41, // +41 C
            1, 5, // -5 F
        ];
        let info = DeviceInfo::from_regs(&regs).expect("valid snapshot");
        assert_eq!(info.model, 6030);
        assert_eq!(info.serial_number, 0x1234);
        assert_eq!(info.firmware, 1.32);
        assert_eq!(info.temperature_c, 41.0);
        assert_eq!(info.temperature_f, -5.0);
    }

    #[test]
    fn info_rejects_short_snapshot() {
        assert!(DeviceInfo::from_regs(&[1, 2, 3]).is_none());
    }

    #[test]
    fn telemetry_decodes_block() {
        // Vout=12.00, Iout=2.50, Pout=30.00, Vin=24.00, keypad=0, prot=1,
        // cv/cc=1 (CC), output=1.
        let regs = [1200, 250, 0x0000, 3000, 2400, 0, 1, 1, 1];
        let t = DeviceTelemetry::from_regs(&regs).expect("valid snapshot");
        assert_eq!(t.voltage_out, 12.0);
        assert_eq!(t.current_out, 2.5);
        assert_eq!(t.power_out, 30.0);
        assert_eq!(t.voltage_in, 24.0);
        assert!(!t.keypad_locked);
        assert_eq!(t.protection, ProtectionStatus::Ovp);
        assert!(t.constant_current);
        assert!(t.output_enabled);
    }

    #[test]
    fn telemetry_rejects_short_snapshot() {
        assert!(DeviceTelemetry::from_regs(&[1, 2, 3]).is_none());
    }

    #[test]
    fn protection_decodes_known_and_unknown() {
        assert_eq!(ProtectionStatus::from_raw(0), ProtectionStatus::Ok);
        assert_eq!(ProtectionStatus::from_raw(1), ProtectionStatus::Ovp);
        assert_eq!(ProtectionStatus::from_raw(2), ProtectionStatus::Ocp);
        assert_eq!(ProtectionStatus::from_raw(9), ProtectionStatus::Other);
    }
}
