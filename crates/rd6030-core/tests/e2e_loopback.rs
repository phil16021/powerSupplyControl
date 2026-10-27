//! End-to-end tests: drive the real tokio-modbus RTU client against a minimal
//! standard-compliant Modbus-RTU server, over an in-memory duplex loopback
//! and over a real PTY pair (`SerialStream` + the driver's flush transport).
//!
//! These exercise the full client framing path (function codes 0x03/0x06/0x10,
//! CRC-16, big-endian register encoding) plus error recovery (exception
//! responses, corrupted CRC, silence) without requiring hardware.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use rd6030_core::{
    Controller, Device, DeviceError, DriverOptions, FnConnector, LinkState, RtuBackend,
};

const REG_COUNT: usize = 128;

/// CRC-16/Modbus (polynomial 0xA001, reflected).
fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &byte in data {
        crc ^= u16::from(byte);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

/// Append the low byte followed by the high byte (Modbus order).
fn append_crc(frame: &mut Vec<u8>) {
    let crc = crc16(frame);
    frame.push(crc as u8);
    frame.push((crc >> 8) as u8);
}

/// Misbehaviour applied to the next response.
#[derive(Debug, Clone, Copy)]
enum SlaveFault {
    /// Do not answer at all.
    Silent,
    /// Answer with a corrupted CRC.
    BadCrc,
    /// Answer with an exception code.
    Exception(u8),
}

#[derive(Default)]
struct SlaveState {
    registers: Mutex<Vec<u16>>,
    faults: Mutex<VecDeque<SlaveFault>>,
}

impl SlaveState {
    fn new() -> Arc<Self> {
        let state = Self {
            registers: Mutex::new(vec![0; REG_COUNT]),
            ..Default::default()
        };
        state.registers.lock().unwrap()[0] = 60301; // RD6030 model id
        Arc::new(state)
    }

    fn reg(&self, addr: usize) -> u16 {
        self.registers.lock().unwrap()[addr]
    }
}

/// Minimal Modbus-RTU slave handling 0x03 (read holding), 0x06 (write single)
/// and 0x10 (write multiple) against a shared register RAM.
async fn rtu_slave<S: AsyncRead + AsyncWrite + Unpin>(mut stream: S, state: Arc<SlaveState>) {
    let mut buf = [0u8; 512];
    loop {
        // Header: slave id + function code.
        if stream.read_exact(&mut buf[0..2]).await.is_err() {
            return;
        }
        let slave = buf[0];
        let func = buf[1];

        let mut response = match func {
            0x03 => {
                if stream.read_exact(&mut buf[2..8]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                let count = u16::from_be_bytes([buf[4], buf[5]]) as usize;
                let regs = state.registers.lock().unwrap();
                if addr + count > regs.len() {
                    vec![slave, func | 0x80, 0x02]
                } else {
                    let mut resp = vec![slave, func, (count * 2) as u8];
                    for v in &regs[addr..addr + count] {
                        resp.extend_from_slice(&v.to_be_bytes());
                    }
                    resp
                }
            }
            0x06 => {
                if stream.read_exact(&mut buf[2..8]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                let value = u16::from_be_bytes([buf[4], buf[5]]);
                state.registers.lock().unwrap()[addr] = value;
                buf[0..6].to_vec()
            }
            0x10 => {
                if stream.read_exact(&mut buf[2..7]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]) as usize;
                let qty = u16::from_be_bytes([buf[4], buf[5]]) as usize;
                let byte_count = buf[6] as usize;
                if stream
                    .read_exact(&mut buf[7..7 + byte_count + 2])
                    .await
                    .is_err()
                {
                    return;
                }
                {
                    let mut regs = state.registers.lock().unwrap();
                    for i in 0..qty {
                        regs[addr + i] = u16::from_be_bytes([buf[7 + i * 2], buf[8 + i * 2]]);
                    }
                }
                vec![slave, func, buf[2], buf[3], buf[4], buf[5]]
            }
            _ => vec![slave, func | 0x80, 0x01],
        };

        let fault = state.faults.lock().unwrap().pop_front();
        match fault {
            Some(SlaveFault::Silent) => continue,
            Some(SlaveFault::Exception(code)) => response = vec![slave, func | 0x80, code],
            Some(SlaveFault::BadCrc) | None => {}
        }
        append_crc(&mut response);
        if matches!(fault, Some(SlaveFault::BadCrc)) {
            let n = response.len();
            response[n - 1] ^= 0xFF;
        }
        if stream.write_all(&response).await.is_err() || stream.flush().await.is_err() {
            return;
        }
    }
}

fn options() -> DriverOptions {
    DriverOptions {
        timeout: Duration::from_millis(300),
        ..DriverOptions::default().without_polling()
    }
}

/// A controller whose connector creates a fresh loopback (and slave task)
/// on every (re)connect, all sharing `state`.
fn loopback_controller(state: &Arc<SlaveState>) -> Controller {
    let state = Arc::clone(state);
    let connector = FnConnector(move || {
        let (client, server) = tokio::io::duplex(4096);
        tokio::spawn(rtu_slave(server, Arc::clone(&state)));
        Ok(RtuBackend::attach(client, 1))
    });
    Controller::from_device(Device::start(connector, options()))
}

#[tokio::test]
async fn e2e_loopback_controls_registers() {
    let state = SlaveState::new();
    let controller = loopback_controller(&state);

    // 0x06 writes.
    controller.set_voltage(12.0).await.unwrap();
    controller.set_current(2.5).await.unwrap();
    controller.set_output(true).await.unwrap();
    assert_eq!(state.reg(8), 1200); // Vset
    assert_eq!(state.reg(9), 250); // Iset
    assert_eq!(state.reg(18), 1); // Output on

    // 0x10 write (RTC registers).
    controller
        .set_rtc(rd6030_core::RtcTime {
            year: 2026,
            month: 10,
            day: 27,
            hour: 12,
            minute: 30,
            second: 45,
        })
        .await
        .unwrap();
    assert_eq!(state.reg(48), 2026);
    assert_eq!(state.reg(53), 45);

    // Seed telemetry in the "device" and read it back via 0x03.
    {
        let mut regs = state.registers.lock().unwrap();
        regs[10] = 1200; // Vout
        regs[11] = 250; // Iout
        regs[13] = 3000; // Pout low word
        regs[14] = 2400; // Vin
    }
    let telem = controller.telemetry().await.unwrap();
    assert_eq!(telem.voltage_out, 12.0);
    assert_eq!(telem.current_out, 2.5);
    assert_eq!(telem.power_out, 30.0);
    assert_eq!(telem.voltage_in, 24.0);
    assert!(telem.output_enabled);

    // Preset bank via 0x10 + 0x03.
    let preset = rd6030_core::Preset {
        voltage: 5.0,
        current: 1.0,
        ovp: 5.5,
        ocp: 1.2,
    };
    assert!(controller.write_preset(2, preset).await.unwrap());
    assert_eq!(controller.read_preset(2).await.unwrap(), preset);

    controller.shutdown().await;
}

#[tokio::test]
async fn e2e_recovers_from_exception_bad_crc_and_silence() {
    let state = SlaveState::new();
    let controller = loopback_controller(&state);
    controller.device().ensure_connected().await.unwrap();

    // Exception response: reported, link stays up.
    state
        .faults
        .lock()
        .unwrap()
        .push_back(SlaveFault::Exception(0x02));
    let err = controller.read_raw(0, 1).await.unwrap_err();
    assert!(matches!(err, DeviceError::Exception(_)), "{err}");
    assert_eq!(controller.link().state, LinkState::Connected);
    assert_eq!(controller.read_raw(0, 1).await.unwrap(), vec![60301]);

    // Silence: times out, then the reopened link works.
    state.faults.lock().unwrap().push_back(SlaveFault::Silent);
    let err = controller.read_raw(0, 1).await.unwrap_err();
    assert!(matches!(err, DeviceError::Timeout(_)), "{err}");
    assert_eq!(controller.read_raw(0, 1).await.unwrap(), vec![60301]);

    // Corrupted CRC: rejected (error or timeout), then recovery.
    state.faults.lock().unwrap().push_back(SlaveFault::BadCrc);
    assert!(controller.read_raw(0, 1).await.is_err());
    assert_eq!(controller.read_raw(0, 1).await.unwrap(), vec![60301]);
    assert_eq!(controller.link().state, LinkState::Connected);

    controller.shutdown().await;
}

#[cfg(unix)]
#[tokio::test]
async fn e2e_over_pty_serial_stream() {
    let (master, slave) = tokio_serial::SerialStream::pair().expect("open pty pair");
    let state = SlaveState::new();
    tokio::spawn(rtu_slave(master, Arc::clone(&state)));

    let device = Device::spawn(RtuBackend::attach_serial(slave, 1), options());
    let controller = Controller::from_device(device);
    controller.set_voltage(5.5).await.unwrap();
    assert_eq!(state.reg(8), 550);
    assert_eq!(controller.info().await.unwrap().model, 60301);
    controller.shutdown().await;
}
