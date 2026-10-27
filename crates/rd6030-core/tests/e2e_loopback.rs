//! End-to-end test: drive the real tokio-modbus RTU client against a minimal
//! standard-compliant Modbus-RTU server over an in-memory duplex loopback.
//!
//! This exercises the full client framing path (function codes 0x03/0x06/0x10,
//! CRC-16, big-endian register encoding) without requiring hardware or a PTY.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use rd6030_core::{Controller, Device, RtuBackend};

const REG_COUNT: usize = 64;

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

/// Minimal Modbus-RTU slave handling 0x03 (read holding), 0x06 (write single)
/// and 0x10 (write multiple) against a shared register RAM.
async fn rtu_slave(mut stream: tokio::io::DuplexStream, registers: Arc<Mutex<[u16; REG_COUNT]>>) {
    let mut buf = [0u8; 512];
    loop {
        // Header: slave id + function code.
        if stream.read_exact(&mut buf[0..2]).await.is_err() {
            return;
        }
        let slave = buf[0];
        let func = buf[1];

        let response = match func {
            0x03 => {
                if stream.read_exact(&mut buf[2..8]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]);
                let count = u16::from_be_bytes([buf[4], buf[5]]) as usize;
                let regs = registers.lock().unwrap();
                let mut resp = vec![slave, func, (count * 2) as u8];
                for i in 0..count {
                    let v = regs[addr as usize + i];
                    resp.push((v >> 8) as u8);
                    resp.push(v as u8);
                }
                append_crc(&mut resp);
                resp
            }
            0x06 => {
                if stream.read_exact(&mut buf[2..8]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]);
                let value = u16::from_be_bytes([buf[4], buf[5]]);
                registers.lock().unwrap()[addr as usize] = value;
                let mut resp = buf[0..6].to_vec();
                append_crc(&mut resp);
                resp
            }
            0x10 => {
                if stream.read_exact(&mut buf[2..7]).await.is_err() {
                    return;
                }
                let addr = u16::from_be_bytes([buf[2], buf[3]]);
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
                    let mut regs = registers.lock().unwrap();
                    for i in 0..qty {
                        let hi = buf[7 + i * 2];
                        let lo = buf[8 + i * 2];
                        regs[addr as usize + i] = u16::from_be_bytes([hi, lo]);
                    }
                }
                let mut resp = vec![slave, func, buf[2], buf[3], buf[4], buf[5]];
                append_crc(&mut resp);
                resp
            }
            _ => {
                let mut resp = vec![slave, func | 0x80, 0x01];
                append_crc(&mut resp);
                resp
            }
        };

        if stream.write_all(&response).await.is_err() || stream.flush().await.is_err() {
            return;
        }
    }
}

#[tokio::test]
async fn e2e_loopback_controls_registers() {
    let (client_stream, server_stream) = tokio::io::duplex(4096);

    let registers = Arc::new(Mutex::new([0u16; REG_COUNT]));
    let server_regs = Arc::clone(&registers);
    let server = tokio::spawn(async move {
        rtu_slave(server_stream, server_regs).await;
    });

    // Real tokio-modbus RTU client over the in-memory loopback.
    let device = Device::spawn(RtuBackend::attach(client_stream, 1), 10_000);
    let controller = Controller::from_device(device);

    // 0x06 writes.
    controller.set_voltage(12.0).await.unwrap();
    controller.set_current(2.5).await.unwrap();
    controller.set_output(true).await.unwrap();

    {
        let regs = registers.lock().unwrap();
        assert_eq!(regs[8], 1200); // Vset
        assert_eq!(regs[9], 250); // Iset
        assert_eq!(regs[18], 1); // Output on
    }

    // 0x10 write (e.g. RTC registers).
    controller
        .device()
        .write_multiple(48, &[2026, 10, 27, 12, 30, 45])
        .await
        .unwrap();
    {
        let regs = registers.lock().unwrap();
        assert_eq!(regs[48], 2026);
        assert_eq!(regs[49], 10);
        assert_eq!(regs[50], 27);
        assert_eq!(regs[53], 45);
    }

    // Seed telemetry in the "device" and read it back via 0x03.
    {
        let mut regs = registers.lock().unwrap();
        regs[10] = 1200; // Vout
        regs[11] = 250; // Iout
        regs[13] = 3000; // Pout low word
        regs[14] = 2400; // Vin
        regs[18] = 1;
    }
    let telem = controller.telemetry().await.unwrap();
    assert_eq!(telem.voltage_out, 12.0);
    assert_eq!(telem.current_out, 2.5);
    assert_eq!(telem.power_out, 30.0);
    assert_eq!(telem.voltage_in, 24.0);
    assert!(telem.output_enabled);

    controller.shutdown().await;
    let _ = server.await;
}
