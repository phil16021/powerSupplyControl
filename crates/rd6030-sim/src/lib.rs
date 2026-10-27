//! Behavioural simulator of a Riden RD6030 acting as a Modbus-RTU slave.
//!
//! The simulated device holds the documented register map and models:
//!
//! * CV/CC regulation into a resistive load (`load_ohms`; `None` = open
//!   output): `Vout = min(Vset, Iset·R)`, `Iout = Vout / R`;
//! * output power, input voltage and temperatures;
//! * over-voltage / over-current protection against the M0 thresholds
//!   (trips switch the output off; switching it on again clears the trip);
//! * preset recall (writing register 19 loads M*n* into the active
//!   setpoint and protection thresholds);
//! * Modbus exceptions for unsupported functions, illegal addresses and
//!   writes to read-only registers; requests with a bad CRC or another slave
//!   id are ignored, as on a real bus.
//!
//! [`serve`] runs the slave over any `AsyncRead + AsyncWrite` stream (an
//! in-memory duplex in tests, a PTY master in the `rd6030-sim` binary).

use std::sync::{Arc, Mutex};

use rd6030_core::register::{self, reg};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Number of simulated holding registers.
pub const REGISTER_COUNT: usize = 256;

/// Modbus exception codes used by the simulator.
const ILLEGAL_FUNCTION: u8 = 0x01;
const ILLEGAL_DATA_ADDRESS: u8 = 0x02;
const ILLEGAL_DATA_VALUE: u8 = 0x03;

/// The simulated device state.
#[derive(Debug, Clone)]
pub struct SimDevice {
    registers: Vec<u16>,
    /// Resistive load on the output in ohms (`None` = open circuit).
    pub load_ohms: Option<f32>,
    /// Number of requests served.
    pub requests: u64,
}

impl Default for SimDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl SimDevice {
    /// A factory-fresh RD6030 with 65 V input and an open output.
    pub fn new() -> Self {
        let mut r = vec![0u16; REGISTER_COUNT];
        r[reg::MODEL as usize] = 60301;
        r[reg::SERIAL_HI as usize] = 0;
        r[reg::SERIAL_LO as usize] = 4242;
        r[reg::FIRMWARE as usize] = 132;
        r[reg::TEMP_C as usize] = 28;
        r[reg::TEMP_F as usize] = 82;
        r[reg::VIN as usize] = 6500;
        r[reg::RTC_YEAR as usize] = 2026;
        r[reg::RTC_MONTH as usize] = 1;
        r[reg::RTC_DAY as usize] = 1;
        r[reg::OPT_BACKLIGHT as usize] = 4;
        r[reg::OPT_BUZZER as usize] = 1;
        for slot in 0..10u8 {
            let base = register::preset_base(slot) as usize;
            r[base] = 500 + 100 * u16::from(slot); // 5 V, 6 V, ...
            r[base + 1] = 100; // 1 A
            r[base + 2] = 6200; // 62 V
            r[base + 3] = 3100; // 31 A
        }
        r[reg::VSET as usize] = r[register::preset_base(0) as usize];
        r[reg::ISET as usize] = r[register::preset_base(0) as usize + 1];
        let mut dev = Self {
            registers: r,
            load_ohms: None,
            requests: 0,
        };
        dev.update();
        dev
    }

    /// Read a register.
    pub fn register(&self, addr: u16) -> u16 {
        self.registers[addr as usize]
    }

    /// Set a register directly (test setup; bypasses write rules).
    pub fn set_register(&mut self, addr: u16, value: u16) {
        self.registers[addr as usize] = value;
        self.update();
    }

    /// Recompute measurements and protection from setpoints and load.
    pub fn update(&mut self) {
        let r = &mut self.registers;
        let vset = f32::from(r[reg::VSET as usize]) / register::VOLTAGE_SCALE;
        let iset = f32::from(r[reg::ISET as usize]) / register::CURRENT_SCALE;
        let ovp = f32::from(r[reg::M0_OVP as usize]) / register::VOLTAGE_SCALE;
        let ocp = f32::from(r[reg::M0_OCP as usize]) / register::CURRENT_SCALE;
        let on = r[reg::OUTPUT as usize] == 1;

        let (mut v, mut i, mut cc) = (0.0f32, 0.0f32, false);
        if on {
            match self.load_ohms {
                Some(ohms) if ohms > 0.0 => {
                    if vset / ohms > iset {
                        cc = true;
                        i = iset;
                        v = iset * ohms;
                    } else {
                        v = vset;
                        i = vset / ohms;
                    }
                }
                _ => v = vset,
            }
        }
        let mut protection = 0;
        if on && v > ovp {
            protection = 1;
        } else if on && i > ocp {
            protection = 2;
        }
        if protection != 0 {
            r[reg::OUTPUT as usize] = 0;
            v = 0.0;
            i = 0.0;
            cc = false;
        }
        if protection != 0 || r[reg::PROTECTION as usize] == 0 || on {
            r[reg::PROTECTION as usize] = protection;
        }

        let raw_v = (v * register::VOLTAGE_SCALE).round() as u16;
        let raw_i = (i * register::CURRENT_SCALE).round() as u16;
        let raw_p = (v * i * register::POWER_SCALE).round() as u32;
        let (p_hi, p_lo) = register::split_u32(raw_p);
        r[reg::VOUT as usize] = raw_v;
        r[reg::IOUT as usize] = raw_i;
        r[reg::POUT_HI as usize] = p_hi;
        r[reg::POUT_LO as usize] = p_lo;
        r[reg::CV_CC as usize] = u16::from(cc);
    }

    /// Handle a holding-register read.
    pub fn read(&mut self, addr: u16, count: u16) -> Result<Vec<u16>, u8> {
        self.requests += 1;
        let (start, end) = (addr as usize, addr as usize + count as usize);
        if count == 0 || count > 125 {
            return Err(ILLEGAL_DATA_VALUE);
        }
        if end > REGISTER_COUNT {
            return Err(ILLEGAL_DATA_ADDRESS);
        }
        self.update();
        Ok(self.registers[start..end].to_vec())
    }

    /// Handle a holding-register write (0x06 / 0x10).
    pub fn write(&mut self, addr: u16, values: &[u16]) -> Result<(), u8> {
        self.requests += 1;
        let count = values.len() as u16;
        let commit =
            addr == reg::CALIBRATION_COMMIT && values == [register::CALIBRATION_COMMIT_VALUE];
        if !commit && !register::is_writable_range(addr, count) {
            return Err(ILLEGAL_DATA_ADDRESS);
        }
        for (i, &value) in values.iter().enumerate() {
            let a = addr + i as u16;
            if a == reg::OUTPUT && value == 1 {
                self.registers[reg::PROTECTION as usize] = 0;
            }
            if commit {
                continue;
            }
            self.registers[a as usize] = value;
            if a == reg::PRESET {
                if value > 9 {
                    return Err(ILLEGAL_DATA_VALUE);
                }
                let base = register::preset_base(value as u8) as usize;
                let slot = [
                    self.registers[base],
                    self.registers[base + 1],
                    self.registers[base + 2],
                    self.registers[base + 3],
                ];
                self.registers[reg::VSET as usize] = slot[0];
                self.registers[reg::ISET as usize] = slot[1];
                self.registers[reg::M0_OVP as usize] = slot[2];
                self.registers[reg::M0_OCP as usize] = slot[3];
            }
        }
        self.update();
        Ok(())
    }
}

/// CRC-16/Modbus (polynomial 0xA001, reflected).
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xA001
            } else {
                crc >> 1
            };
        }
    }
    crc
}

fn append_crc(frame: &mut Vec<u8>) {
    let crc = crc16(frame);
    frame.extend_from_slice(&crc.to_le_bytes());
}

/// Process one complete request frame (including CRC) and build the
/// response frame, or `None` if the request must be ignored.
pub fn handle_frame(device: &Mutex<SimDevice>, slave_id: u8, frame: &[u8]) -> Option<Vec<u8>> {
    if frame.len() < 4 {
        return None;
    }
    let (body, crc) = frame.split_at(frame.len() - 2);
    if crc16(body).to_le_bytes() != [crc[0], crc[1]] || body[0] != slave_id {
        return None;
    }
    let func = body[1];
    let word = |i: usize| u16::from_be_bytes([body[i], body[i + 1]]);
    let mut dev = device.lock().unwrap();
    let result = match func {
        0x03 => dev.read(word(2), word(4)).map(|regs| {
            let mut resp = vec![slave_id, func, (regs.len() * 2) as u8];
            for v in regs {
                resp.extend_from_slice(&v.to_be_bytes());
            }
            resp
        }),
        0x06 => dev.write(word(2), &[word(4)]).map(|()| body[0..6].to_vec()),
        0x10 => {
            let qty = word(4) as usize;
            let values: Vec<u16> = (0..qty).map(|i| word(7 + i * 2)).collect();
            dev.write(word(2), &values).map(|()| body[0..6].to_vec())
        }
        _ => Err(ILLEGAL_FUNCTION),
    };
    let mut resp = result.unwrap_or_else(|code| vec![slave_id, func | 0x80, code]);
    append_crc(&mut resp);
    Some(resp)
}

/// Expected request length for a function code, given the bytes read so
/// far (`None` = need more bytes to decide).
fn request_len(head: &[u8]) -> Option<usize> {
    match head.get(1)? {
        0x03 | 0x06 => Some(8),
        0x10 => head.get(6).map(|&n| 9 + n as usize),
        _ => Some(4), // unknown function: slave + func + crc
    }
}

/// Serve Modbus-RTU requests on `stream` until it closes.
pub async fn serve<S>(
    mut stream: S,
    device: Arc<Mutex<SimDevice>>,
    slave_id: u8,
) -> std::io::Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut chunk = [0u8; 256];
    loop {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        while let Some(len) = request_len(&buf) {
            if len > 256 {
                buf.clear(); // garbage; resynchronise on the next frame
                break;
            }
            if buf.len() < len {
                break;
            }
            let frame: Vec<u8> = buf.drain(..len).collect();
            match handle_frame(&device, slave_id, &frame) {
                Some(resp) => {
                    stream.write_all(&resp).await?;
                    stream.flush().await?;
                }
                // Bad CRC / other slave: drop buffered bytes to resync.
                None => buf.clear(),
            }
        }
    }
}

/// Spawn a simulator over an in-memory duplex and return the client end.
pub fn spawn_duplex(device: Arc<Mutex<SimDevice>>, slave_id: u8) -> tokio::io::DuplexStream {
    let (client, server) = tokio::io::duplex(4096);
    tokio::spawn(async move {
        let _ = serve(server, device, slave_id).await;
    });
    client
}

/// A [`rd6030_core::Connector`] that opens a fresh in-memory link to the
/// shared simulated device on every (re)connect.
pub fn connector(
    device: Arc<Mutex<SimDevice>>,
) -> impl rd6030_core::Connector<Backend = rd6030_core::RtuBackend> {
    rd6030_core::FnConnector(move || {
        Ok(rd6030_core::RtuBackend::attach(
            spawn_duplex(Arc::clone(&device), 1),
            1,
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(dev: &mut SimDevice, vset: u16, iset: u16) {
        dev.write(reg::VSET, &[vset, iset]).unwrap();
        dev.write(reg::OUTPUT, &[1]).unwrap();
    }

    #[test]
    fn cv_into_light_load() {
        let mut dev = SimDevice::new();
        dev.load_ohms = Some(10.0);
        on(&mut dev, 1200, 500); // 12 V, 5 A limit -> 1.2 A
        assert_eq!(dev.register(reg::VOUT), 1200);
        assert_eq!(dev.register(reg::IOUT), 120);
        assert_eq!(dev.register(reg::CV_CC), 0);
        assert_eq!(dev.register(reg::POUT_LO), 1440); // 14.40 W
    }

    #[test]
    fn cc_into_heavy_load() {
        let mut dev = SimDevice::new();
        dev.load_ohms = Some(1.0);
        on(&mut dev, 1200, 200); // 12 V, 2 A limit into 1 ohm -> CC, 2 V
        assert_eq!(dev.register(reg::IOUT), 200);
        assert_eq!(dev.register(reg::VOUT), 200);
        assert_eq!(dev.register(reg::CV_CC), 1);
    }

    #[test]
    fn ovp_trips_and_clears() {
        let mut dev = SimDevice::new();
        dev.write(reg::M0_OVP, &[1000, 3100]).unwrap(); // OVP 10 V
        on(&mut dev, 1200, 100);
        assert_eq!(dev.register(reg::PROTECTION), 1);
        assert_eq!(dev.register(reg::OUTPUT), 0);
        dev.write(reg::VSET, &[900]).unwrap();
        dev.write(reg::OUTPUT, &[1]).unwrap();
        assert_eq!(dev.register(reg::PROTECTION), 0);
        assert_eq!(dev.register(reg::VOUT), 900);
    }

    #[test]
    fn preset_recall_loads_slot() {
        let mut dev = SimDevice::new();
        dev.write(reg::PRESET, &[3]).unwrap();
        assert_eq!(dev.register(reg::VSET), 800);
        assert_eq!(dev.register(reg::ISET), 100);
    }

    #[test]
    fn read_only_and_bootloader_writes_are_rejected() {
        let mut dev = SimDevice::new();
        assert_eq!(dev.write(reg::VOUT, &[1]), Err(ILLEGAL_DATA_ADDRESS));
        assert_eq!(
            dev.write(reg::BOOTLOADER, &[0x1601]),
            Err(ILLEGAL_DATA_ADDRESS)
        );
        assert!(dev
            .write(
                reg::CALIBRATION_COMMIT,
                &[register::CALIBRATION_COMMIT_VALUE]
            )
            .is_ok());
        assert_eq!(dev.read(250, 10), Err(ILLEGAL_DATA_ADDRESS));
    }

    #[test]
    fn frames_with_bad_crc_or_other_slave_are_ignored() {
        let dev = Mutex::new(SimDevice::new());
        let mut req = vec![1, 0x03, 0, 0, 0, 1];
        append_crc(&mut req);
        let resp = handle_frame(&dev, 1, &req).unwrap();
        assert_eq!(&resp[..5], &[1, 0x03, 2, 0xEB, 0x8D]); // 60301 = 0xEB8D
        assert!(handle_frame(&dev, 2, &req).is_none());
        let n = req.len();
        req[n - 1] ^= 1;
        assert!(handle_frame(&dev, 1, &req).is_none());
    }
}
