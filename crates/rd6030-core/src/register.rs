//! Register map and unit scaling for the Riden RD6030.
//!
//! All addresses are 16-bit Modbus holding-register addresses. Values are
//! exchanged as unsigned 16-bit words; 32-bit quantities combine two
//! consecutive registers in big-endian order (high word at the lower address).

/// Rated limits of the RD6030 (60 V / 30 A), used for range validation.
pub const MAX_VOLTAGE: f32 = 60.0;
pub const MAX_CURRENT: f32 = 30.0;

/// Holding-register addresses.
pub mod reg {
    // Identity / info (read-only).
    pub const MODEL: u16 = 0x0000;
    pub const SERIAL_HI: u16 = 0x0001;
    pub const SERIAL_LO: u16 = 0x0002;
    pub const FIRMWARE: u16 = 0x0003;
    pub const TEMP_C_SIGN: u16 = 0x0004;
    pub const TEMP_C: u16 = 0x0005;
    pub const TEMP_F_SIGN: u16 = 0x0006;
    pub const TEMP_F: u16 = 0x0007;

    // Setpoints (read/write).
    pub const VSET: u16 = 0x0008;
    pub const ISET: u16 = 0x0009;

    // Measurements (read-only).
    pub const VOUT: u16 = 0x000A;
    pub const IOUT: u16 = 0x000B;
    pub const POUT_HI: u16 = 0x000C;
    pub const POUT_LO: u16 = 0x000D;
    pub const VIN: u16 = 0x000E;
    pub const KEYPAD_LOCK: u16 = 0x000F;
    pub const PROTECTION: u16 = 0x0010;
    pub const CV_CC: u16 = 0x0011;
    pub const OUTPUT: u16 = 0x0012;
    pub const PRESET: u16 = 0x0013;

    // Battery / charging (read-only).
    pub const BATTERY_STATUS: u16 = 0x0020;
    pub const VBAT: u16 = 0x0021;
    pub const BAT_TEMP_C_SIGN: u16 = 0x0022;
    pub const BAT_TEMP_C: u16 = 0x0023;
    pub const BAT_TEMP_F_SIGN: u16 = 0x0024;
    pub const BAT_TEMP_F: u16 = 0x0025;
    pub const CBAT_HI: u16 = 0x0026;
    pub const CBAT_LO: u16 = 0x0027;
    pub const EBAT_HI: u16 = 0x0028;
    pub const EBAT_LO: u16 = 0x0029;

    // Real-time clock (read/write).
    pub const RTC_YEAR: u16 = 0x0030;
    pub const RTC_MONTH: u16 = 0x0031;
    pub const RTC_DAY: u16 = 0x0032;
    pub const RTC_HOUR: u16 = 0x0033;
    pub const RTC_MINUTE: u16 = 0x0034;
    pub const RTC_SECOND: u16 = 0x0035;

    // Calibration (write-only commit).
    pub const CALIBRATION_COMMIT: u16 = 0x0036;
}

/// Magic value written to [`reg::CALIBRATION_COMMIT`] to persist calibration
/// changes to NVRAM.
pub const CALIBRATION_COMMIT_VALUE: u16 = 0x1501;

/// Start/count of a contiguous telemetry snapshot (0x000A..=0x0012).
pub const TELEMETRY_START: u16 = reg::VOUT;
pub const TELEMETRY_COUNT: u16 = 9;

/// Start/count of a contiguous device-info snapshot (0x0000..=0x0007).
pub const INFO_START: u16 = reg::MODEL;
pub const INFO_COUNT: u16 = 8;

// Scaling factors (raw u16/u32 -> physical unit).
pub const VOLTAGE_SCALE: f32 = 100.0; // 10 mV  -> V
pub const CURRENT_SCALE: f32 = 100.0; // 10 mA  -> A
pub const POWER_SCALE: f32 = 100.0; // 10 mW  -> W
pub const CAPACITY_SCALE: f32 = 1000.0; // 1 mAh  -> Ah
pub const ENERGY_SCALE: f32 = 100.0; // 10 mWh -> Wh
pub const FIRMWARE_SCALE: f32 = 100.0; // e.g. 132 -> 1.32

/// Convert a raw voltage register value to volts.
pub fn raw_to_voltage(raw: u16) -> f32 {
    raw as f32 / VOLTAGE_SCALE
}

/// Convert a raw current register value to amperes.
pub fn raw_to_current(raw: u16) -> f32 {
    raw as f32 / CURRENT_SCALE
}

/// Convert a raw 32-bit power value to watts.
pub fn raw_to_power(raw: u32) -> f32 {
    raw as f32 / POWER_SCALE
}

/// Convert a raw 32-bit capacity value to ampere-hours.
pub fn raw_to_capacity(raw: u32) -> f32 {
    raw as f32 / CAPACITY_SCALE
}

/// Convert a raw 32-bit energy value to watt-hours.
pub fn raw_to_energy(raw: u32) -> f32 {
    raw as f32 / ENERGY_SCALE
}

/// Convert a raw firmware value (e.g. `132`) to a version (e.g. `1.32`).
pub fn raw_to_firmware(raw: u16) -> f32 {
    raw as f32 / FIRMWARE_SCALE
}

/// Convert volts to a raw voltage register value (rounded to 10 mV).
pub fn voltage_to_raw(volts: f32) -> u16 {
    (volts * VOLTAGE_SCALE).round() as u16
}

/// Convert amperes to a raw current register value (rounded to 10 mA).
pub fn current_to_raw(amperes: f32) -> u16 {
    (amperes * CURRENT_SCALE).round() as u16
}

/// Combine two consecutive registers into a 32-bit value (big-endian:
/// `high` sits at the lower register address).
pub fn combine_u32(high: u16, low: u16) -> u32 {
    ((high as u32) << 16) | low as u32
}

/// Split a 32-bit value into `(high, low)` words.
pub fn split_u32(value: u32) -> (u16, u16) {
    ((value >> 16) as u16, (value & 0xFFFF) as u16)
}

/// Decode a signed temperature from a sign register (0 = positive,
/// 1 = negative) and a value register.
pub fn raw_to_temperature(sign: u16, value: u16) -> f32 {
    if sign == 0 {
        value as f32
    } else {
        -(value as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_scaling_round_trips() {
        assert_eq!(raw_to_voltage(1200), 12.0);
        assert_eq!(voltage_to_raw(12.0), 1200);
        assert_eq!(voltage_to_raw(25.505), 2551); // rounds to 10 mV
    }

    #[test]
    fn current_scaling_matches_documented_example() {
        // "Ein Registerwert von 2550 in Register 9 repräsentiert exakt 25,50 A"
        assert_eq!(raw_to_current(2550), 25.5);
        assert_eq!(current_to_raw(25.5), 2550);
    }

    #[test]
    fn power_is_big_endian_high_word_first() {
        // 10.00 W -> raw 1000 -> high 0, low 1000
        assert_eq!(combine_u32(0, 1000), 1000);
        assert_eq!(raw_to_power(1000), 10.0);
        // 65.536 kW-like large value still round-trips
        let raw = combine_u32(0x0001, 0x0000);
        assert_eq!(split_u32(raw), (0x0001, 0x0000));
    }

    #[test]
    fn capacity_and_energy_scaling() {
        assert_eq!(raw_to_capacity(1500), 1.5); // 1500 mAh -> 1.5 Ah
        assert_eq!(raw_to_energy(1234), 12.34); // 1234 x 10 mWh -> 12.34 Wh
    }

    #[test]
    fn firmware_scaling() {
        assert_eq!(raw_to_firmware(132), 1.32);
    }

    #[test]
    fn temperature_sign_handling() {
        assert_eq!(raw_to_temperature(0, 42), 42.0);
        assert_eq!(raw_to_temperature(1, 42), -42.0);
    }

    #[test]
    fn register_addresses_are_unique_and_in_expected_ranges() {
        // Spot-check the documented addresses.
        assert_eq!(reg::MODEL, 0);
        assert_eq!(reg::VSET, 8);
        assert_eq!(reg::ISET, 9);
        assert_eq!(reg::VOUT, 10);
        assert_eq!(reg::OUTPUT, 18);
        assert_eq!(reg::PRESET, 19);
        assert_eq!(reg::BATTERY_STATUS, 32);
        assert_eq!(reg::CALIBRATION_COMMIT, 54);
        assert_eq!(CALIBRATION_COMMIT_VALUE, 0x1501);
    }
}
