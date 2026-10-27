//! Real-time clock (RTC) access (registers 0x0030..=0x0035).

use chrono::{Datelike, Timelike};
use serde::{Deserialize, Serialize};

use crate::controller::Controller;
use crate::error::DeviceError;
use crate::register;

/// A calendar date and time as held by the device RTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RtcTime {
    pub year: u16,
    pub month: u8,
    pub day: u8,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

impl RtcTime {
    /// Decode an RTC snapshot read from registers 0x0030..=0x0035. Values
    /// that do not fit their field are rejected (no silent truncation).
    pub fn from_regs(regs: &[u16]) -> Option<Self> {
        if regs.len() < 6 {
            return None;
        }
        let b = |v: u16| u8::try_from(v).ok();
        Some(Self {
            year: regs[0],
            month: b(regs[1])?,
            day: b(regs[2])?,
            hour: b(regs[3])?,
            minute: b(regs[4])?,
            second: b(regs[5])?,
        })
    }

    /// Check that this is a real calendar date/time.
    pub fn validate(&self) -> Result<(), DeviceError> {
        let date = chrono::NaiveDate::from_ymd_opt(
            i32::from(self.year),
            u32::from(self.month),
            u32::from(self.day),
        );
        let time = chrono::NaiveTime::from_hms_opt(
            u32::from(self.hour),
            u32::from(self.minute),
            u32::from(self.second),
        );
        if date.is_none() || time.is_none() || !(2000..=2099).contains(&self.year) {
            return Err(DeviceError::OutOfRange(format!("invalid RTC time {self}")));
        }
        Ok(())
    }

    /// Encode as the six RTC registers (year, month, day, hour, minute, second).
    pub fn to_regs(self) -> [u16; 6] {
        [
            self.year,
            u16::from(self.month),
            u16::from(self.day),
            u16::from(self.hour),
            u16::from(self.minute),
            u16::from(self.second),
        ]
    }
}

impl std::fmt::Display for RtcTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

impl From<chrono::DateTime<chrono::Local>> for RtcTime {
    fn from(dt: chrono::DateTime<chrono::Local>) -> Self {
        Self {
            year: dt.year() as u16,
            month: dt.month() as u8,
            day: dt.day() as u8,
            hour: dt.hour() as u8,
            minute: dt.minute() as u8,
            second: dt.second() as u8,
        }
    }
}

impl Controller {
    /// Read the device RTC.
    pub async fn read_rtc(&self) -> Result<RtcTime, DeviceError> {
        let regs = self
            .device()
            .read_holding(register::reg::RTC_YEAR, 6)
            .await?;
        RtcTime::from_regs(&regs).ok_or(DeviceError::InvalidData)
    }

    /// Set the device RTC.
    pub async fn set_rtc(&self, time: RtcTime) -> Result<(), DeviceError> {
        time.validate()?;
        self.device()
            .write_multiple(register::reg::RTC_YEAR, &time.to_regs())
            .await
    }

    /// Synchronize the device RTC to the host's local clock.
    pub async fn sync_rtc(&self) -> Result<RtcTime, DeviceError> {
        let now: RtcTime = chrono::Local::now().into();
        self.set_rtc(now).await?;
        Ok(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rtc_round_trips_through_registers() {
        let t = RtcTime {
            year: 2026,
            month: 10,
            day: 27,
            hour: 12,
            minute: 30,
            second: 45,
        };
        let regs = t.to_regs();
        let decoded = RtcTime::from_regs(&regs).expect("valid");
        assert_eq!(t, decoded);
        assert_eq!(regs, [2026, 10, 27, 12, 30, 45]);
    }

    #[test]
    fn rtc_validation() {
        let ok = RtcTime {
            year: 2024,
            month: 2,
            day: 29,
            hour: 23,
            minute: 59,
            second: 59,
        };
        assert!(ok.validate().is_ok());
        assert!(RtcTime { month: 13, ..ok }.validate().is_err());
        assert!(RtcTime { year: 2023, ..ok }.validate().is_err()); // no Feb 29
        assert!(RtcTime { hour: 24, ..ok }.validate().is_err());
        assert!(RtcTime { year: 1999, ..ok }.validate().is_err());
        assert!(RtcTime::from_regs(&[2026, 300, 1, 0, 0, 0]).is_none());
    }

    #[test]
    fn rtc_display_format() {
        let t = RtcTime {
            year: 2026,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
        };
        assert_eq!(t.to_string(), "2026-01-02 03:04:05");
    }
}
