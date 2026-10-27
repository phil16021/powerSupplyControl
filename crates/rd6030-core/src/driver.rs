//! Actor-based device driver.
//!
//! A dedicated tokio task owns the exclusive Modbus-RTU transport and
//! serializes every request through an `mpsc` command channel. This prevents
//! interleaved frames from concurrent callers and paces background telemetry
//! polling to a configurable interval (10..=20 Hz recommended).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_modbus::client::{rtu, Context};
use tokio_modbus::prelude::*;
use tokio_modbus::Slave;
use tokio_serial::{
    ClearBuffer, DataBits, Parity, SerialPort, SerialPortBuilderExt, SerialStream, StopBits,
};
use tracing::{debug, warn};

use crate::config::Config;
use crate::error::DeviceError;
use crate::model::DeviceTelemetry;
use crate::register;

/// Backend abstraction over the Modbus-RTU transport.
///
/// The actor drives a single implementation; tests use [`MockBackend`]. The
/// returned futures are explicitly `Send` so the actor task can run on the
/// tokio multi-threaded runtime.
#[allow(clippy::manual_async_fn)] // explicit `+ Send` is required for tokio::spawn
pub trait ModbusBackend: Send + 'static {
    /// Read `count` contiguous holding registers starting at `addr`.
    fn read_holding(
        &mut self,
        addr: u16,
        count: u16,
    ) -> impl Future<Output = Result<Vec<u16>, DeviceError>> + Send;

    /// Write a single holding register.
    fn write_single(
        &mut self,
        addr: u16,
        value: u16,
    ) -> impl Future<Output = Result<(), DeviceError>> + Send;

    /// Write multiple contiguous holding registers.
    fn write_multiple(
        &mut self,
        addr: u16,
        values: &[u16],
    ) -> impl Future<Output = Result<(), DeviceError>> + Send;
}

/// Wraps a [`SerialStream`] and discards stale input bytes before each write,
/// preventing frame desynchronization after bus errors or reconnects.
struct FlushTransport {
    inner: SerialStream,
}

impl AsyncRead for FlushTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for FlushTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // Discard stale bytes left over from a previous (possibly timed-out)
        // exchange before sending the next request frame.
        let this = self.get_mut();
        let _ = this.inner.clear(ClearBuffer::Input);
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Real Modbus-RTU backend over a serial port.
pub struct RtuBackend {
    ctx: Context,
}

impl RtuBackend {
    /// Open the serial port and attach the Modbus-RTU client.
    pub fn connect(config: &Config) -> Result<Self, DeviceError> {
        let stream = tokio_serial::new(&config.port, config.baud_rate)
            .data_bits(DataBits::Eight)
            .stop_bits(StopBits::One)
            .parity(Parity::None)
            .timeout(Duration::from_millis(config.timeout_ms))
            .open_native_async()?;
        // Clear any residue from a previous session before attaching.
        let _ = stream.clear(ClearBuffer::All);

        let transport = FlushTransport { inner: stream };
        let ctx = rtu::attach_slave(transport, Slave(config.slave_id));
        Ok(Self { ctx })
    }

    /// Attach the RTU client to an arbitrary byte transport (used for tests
    /// and software loopbacks).
    pub fn attach<T>(transport: T, slave_id: u8) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        Self {
            ctx: rtu::attach_slave(transport, Slave(slave_id)),
        }
    }
}

#[allow(clippy::manual_async_fn)] // explicit `+ Send` is required for tokio::spawn
impl ModbusBackend for RtuBackend {
    fn read_holding(
        &mut self,
        addr: u16,
        count: u16,
    ) -> impl Future<Output = Result<Vec<u16>, DeviceError>> + Send {
        async move { Ok(self.ctx.read_holding_registers(addr, count).await??) }
    }

    fn write_single(
        &mut self,
        addr: u16,
        value: u16,
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        async move { Ok(self.ctx.write_single_register(addr, value).await??) }
    }

    fn write_multiple(
        &mut self,
        addr: u16,
        values: &[u16],
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        let values = values.to_vec();
        async move { Ok(self.ctx.write_multiple_registers(addr, &values).await??) }
    }
}

/// In-memory backend for tests and simulation: a flat holding-register RAM.
#[derive(Debug, Default)]
pub struct MockBackend {
    registers: Vec<u16>,
}

impl MockBackend {
    /// Create an empty backend with `size` zero-initialised registers.
    pub fn new(size: usize) -> Self {
        Self {
            registers: vec![0; size],
        }
    }

    /// Create a backend pre-loaded with the given register contents.
    pub fn with_registers(registers: Vec<u16>) -> Self {
        Self { registers }
    }

    /// Borrow the current register contents.
    pub fn registers(&self) -> &[u16] {
        &self.registers
    }

    /// Set a register value directly (bypasses the async interface).
    pub fn set_register(&mut self, addr: u16, value: u16) {
        if let Some(slot) = self.registers.get_mut(addr as usize) {
            *slot = value;
        }
    }
}

#[allow(clippy::manual_async_fn)] // explicit `+ Send` is required for tokio::spawn
impl ModbusBackend for MockBackend {
    fn read_holding(
        &mut self,
        addr: u16,
        count: u16,
    ) -> impl Future<Output = Result<Vec<u16>, DeviceError>> + Send {
        let start = addr as usize;
        let end = start + count as usize;
        if end > self.registers.len() {
            std::future::ready(Err(DeviceError::InvalidData))
        } else {
            std::future::ready(Ok(self.registers[start..end].to_vec()))
        }
    }

    fn write_single(
        &mut self,
        addr: u16,
        value: u16,
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        let result = match self.registers.get_mut(addr as usize) {
            Some(slot) => {
                *slot = value;
                Ok(())
            }
            None => Err(DeviceError::InvalidData),
        };
        std::future::ready(result)
    }

    fn write_multiple(
        &mut self,
        addr: u16,
        values: &[u16],
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        let start = addr as usize;
        let end = start + values.len();
        let result = if end > self.registers.len() {
            Err(DeviceError::InvalidData)
        } else {
            self.registers[start..end].copy_from_slice(values);
            Ok(())
        };
        std::future::ready(result)
    }
}

enum Command {
    ReadHolding {
        addr: u16,
        count: u16,
        reply: oneshot::Sender<Result<Vec<u16>, DeviceError>>,
    },
    WriteSingle {
        addr: u16,
        value: u16,
        reply: oneshot::Sender<Result<(), DeviceError>>,
    },
    WriteMultiple {
        addr: u16,
        values: Vec<u16>,
        reply: oneshot::Sender<Result<(), DeviceError>>,
    },
    Shutdown,
}

/// Cloneable handle to the device actor.
#[derive(Clone, Debug)]
pub struct Device {
    tx: mpsc::Sender<Command>,
    telemetry_rx: watch::Receiver<DeviceTelemetry>,
}

impl Device {
    /// Connect to a real device and spawn the actor task.
    pub fn connect(config: &Config) -> Result<Self, DeviceError> {
        let backend = RtuBackend::connect(config)?;
        Ok(Self::spawn(backend, config.poll_interval_ms))
    }

    /// Spawn the actor task driving `backend`.
    pub fn spawn<B: ModbusBackend>(backend: B, poll_interval_ms: u64) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let (telemetry_tx, telemetry_rx) = watch::channel(DeviceTelemetry::default());
        tokio::spawn(actor::run(backend, rx, telemetry_tx, poll_interval_ms));
        Self { tx, telemetry_rx }
    }

    /// Read `count` contiguous holding registers starting at `addr`.
    pub async fn read_holding(&self, addr: u16, count: u16) -> Result<Vec<u16>, DeviceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::ReadHolding { addr, count, reply })
            .await
            .map_err(|_| DeviceError::Disconnected)?;
        rx.await.map_err(|_| DeviceError::Disconnected)?
    }

    /// Write a single holding register.
    pub async fn write_single(&self, addr: u16, value: u16) -> Result<(), DeviceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::WriteSingle { addr, value, reply })
            .await
            .map_err(|_| DeviceError::Disconnected)?;
        rx.await.map_err(|_| DeviceError::Disconnected)?
    }

    /// Write multiple contiguous holding registers.
    pub async fn write_multiple(&self, addr: u16, values: &[u16]) -> Result<(), DeviceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::WriteMultiple {
                addr,
                values: values.to_vec(),
                reply,
            })
            .await
            .map_err(|_| DeviceError::Disconnected)?;
        rx.await.map_err(|_| DeviceError::Disconnected)?
    }

    /// Subscribe to background telemetry updates.
    pub fn subscribe(&self) -> watch::Receiver<DeviceTelemetry> {
        self.telemetry_rx.clone()
    }

    /// Get the most recently polled telemetry value (no I/O).
    pub fn latest(&self) -> DeviceTelemetry {
        *self.telemetry_rx.borrow()
    }

    /// Stop the actor task.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown).await;
    }
}

mod actor {
    use super::*;

    /// The single task that owns the backend and serializes all requests.
    pub async fn run<B: ModbusBackend>(
        mut backend: B,
        mut rx: mpsc::Receiver<Command>,
        telemetry_tx: watch::Sender<DeviceTelemetry>,
        poll_interval_ms: u64,
    ) {
        let mut ticker = interval(Duration::from_millis(poll_interval_ms));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                biased;
                cmd = rx.recv() => {
                    match cmd {
                        None | Some(Command::Shutdown) => break,
                        Some(Command::ReadHolding { addr, count, reply }) => {
                            let res = backend.read_holding(addr, count).await;
                            let _ = reply.send(res);
                        }
                        Some(Command::WriteSingle { addr, value, reply }) => {
                            let res = backend.write_single(addr, value).await;
                            let _ = reply.send(res);
                        }
                        Some(Command::WriteMultiple { addr, values, reply }) => {
                            let res = backend.write_multiple(addr, &values).await;
                            let _ = reply.send(res);
                        }
                    }
                }
                _ = ticker.tick() => {
                    match backend
                        .read_holding(register::TELEMETRY_START, register::TELEMETRY_COUNT)
                        .await
                    {
                        Ok(regs) => {
                            if let Some(telem) = DeviceTelemetry::from_regs(&regs) {
                                let _ = telemetry_tx.send(telem);
                            }
                        }
                        Err(err) => {
                            warn!(error = %err, "telemetry poll failed");
                        }
                    }
                }
            }
        }
        debug!("device actor stopped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn actor_serializes_requests() {
        let backend = MockBackend::new(64);
        let device = Device::spawn(backend, 10_000);

        device
            .write_single(register::reg::VSET, 1200)
            .await
            .unwrap();
        device.write_single(register::reg::ISET, 250).await.unwrap();
        device.write_single(register::reg::OUTPUT, 1).await.unwrap();

        let vset = device.read_holding(register::reg::VSET, 1).await.unwrap();
        let iset = device.read_holding(register::reg::ISET, 1).await.unwrap();
        let out = device.read_holding(register::reg::OUTPUT, 1).await.unwrap();

        assert_eq!(vset, vec![1200]);
        assert_eq!(iset, vec![250]);
        assert_eq!(out, vec![1]);

        device.shutdown().await;
    }

    #[tokio::test]
    async fn telemetry_is_published() {
        let mut backend = MockBackend::new(64);
        // Fill the telemetry block: Vout 12.00, Iout 2.50, Pout 30.00, Vin 24, out on.
        backend.set_register(register::reg::VOUT, 1200);
        backend.set_register(register::reg::IOUT, 250);
        backend.set_register(register::reg::POUT_LO, 3000);
        backend.set_register(register::reg::VIN, 2400);
        backend.set_register(register::reg::OUTPUT, 1);

        let device = Device::spawn(backend, 10);
        let mut rx = device.subscribe();

        // Wait for the first background poll to arrive.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut saw_output = false;
        while tokio::time::Instant::now() < deadline {
            if rx.has_changed().unwrap_or(false) {
                let t = *rx.borrow_and_update();
                saw_output |= t.output_enabled;
                if t.voltage_out == 12.0 && t.current_out == 2.5 {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let t = *rx.borrow();
        assert_eq!(t.voltage_out, 12.0);
        assert_eq!(t.current_out, 2.5);
        assert_eq!(t.power_out, 30.0);
        assert_eq!(t.voltage_in, 24.0);
        assert!(saw_output);

        device.shutdown().await;
    }
}
