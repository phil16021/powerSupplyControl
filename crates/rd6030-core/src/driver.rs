//! Actor-based device driver.
//!
//! A dedicated tokio task owns the exclusive Modbus-RTU transport and
//! serializes every request through an `mpsc` command channel. This prevents
//! interleaved frames from concurrent callers and paces background telemetry
//! polling to a configurable interval (10..=20 Hz recommended).
//!
//! Robustness guarantees of the actor:
//!
//! * every transaction is bounded by [`DriverOptions::timeout`]; a serial
//!   port read never blocks the actor forever;
//! * after a timeout or transport error the backend is dropped and reopened
//!   through its [`Connector`], so stale bytes in the RTU framing buffer can
//!   never be matched to a later request;
//! * reconnects back off exponentially while the device is unreachable, and
//!   the link state is published on a `watch` channel ([`LinkStatus`]);
//! * a minimum idle gap separates consecutive transactions, and background
//!   polling is scheduled ahead of queued commands once it is due, so neither
//!   a command flood nor polling can starve the other.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::{sleep_until, Instant};
use tokio_modbus::client::{rtu, Context};
use tokio_modbus::prelude::*;
use tokio_modbus::Slave;
use tokio_serial::{
    ClearBuffer, DataBits, Parity, SerialPort, SerialPortBuilderExt, SerialStream, StopBits,
};
use tracing::{debug, info, trace, warn};

use crate::config::Config;
use crate::error::DeviceError;
use crate::model::{DeviceInfo, DeviceTelemetry, LinkState, LinkStatus, TelemetrySample};
use crate::register::{self, Model};

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

/// Opens (and re-opens) a backend. The actor calls this initially and after
/// every transport failure.
pub trait Connector: Send + 'static {
    /// The backend produced by this connector.
    type Backend: ModbusBackend;

    /// Open a fresh backend.
    fn connect(&mut self) -> Result<Self::Backend, DeviceError>;
}

/// Connector opening a serial port with fixed 8N1 framing.
#[derive(Debug, Clone)]
pub struct SerialConnector {
    /// Serial port path.
    pub port: String,
    /// Baud rate.
    pub baud_rate: u32,
    /// Modbus slave id.
    pub slave_id: u8,
}

impl SerialConnector {
    /// Build a connector from the configuration.
    pub fn from_config(config: &Config) -> Self {
        Self {
            port: config.port.clone(),
            baud_rate: config.baud_rate,
            slave_id: config.slave_id,
        }
    }
}

impl Connector for SerialConnector {
    type Backend = RtuBackend;

    fn connect(&mut self) -> Result<RtuBackend, DeviceError> {
        RtuBackend::open(&self.port, self.baud_rate, self.slave_id)
    }
}

/// Connector built from a closure (tests, simulators, custom transports).
pub struct FnConnector<F>(pub F);

impl<B, F> Connector for FnConnector<F>
where
    B: ModbusBackend,
    F: FnMut() -> Result<B, DeviceError> + Send + 'static,
{
    type Backend = B;

    fn connect(&mut self) -> Result<B, DeviceError> {
        (self.0)()
    }
}

/// Connector that hands out one pre-built backend and cannot reopen it.
struct OnceConnector<B>(Option<B>);

impl<B: ModbusBackend> Connector for OnceConnector<B> {
    type Backend = B;

    fn connect(&mut self) -> Result<B, DeviceError> {
        self.0
            .take()
            .ok_or_else(|| DeviceError::NotConnected("backend cannot be reopened".to_string()))
    }
}

/// Wraps a [`SerialStream`] and discards stale input bytes before each
/// request, and traces raw frames at `trace` level.
struct FlushTransport {
    inner: SerialStream,
    /// Set once the current request frame has started; cleared on read.
    writing: bool,
}

impl AsyncRead for FlushTransport {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        this.writing = false;
        let before = buf.filled().len();
        let res = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = res {
            trace!(rx = %hex(&buf.filled()[before..]), "serial rx");
        }
        res
    }
}

impl AsyncWrite for FlushTransport {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if !this.writing {
            // Discard stale bytes left over from a previous exchange before
            // sending the next request frame (once per frame).
            let _ = this.inner.clear(ClearBuffer::Input);
            this.writing = true;
        }
        let res = Pin::new(&mut this.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            trace!(tx = %hex(&buf[..n]), "serial tx");
        }
        res
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Real Modbus-RTU backend over a serial port.
pub struct RtuBackend {
    ctx: Context,
}

impl RtuBackend {
    /// Open the serial port (8N1, exclusive) and attach the Modbus-RTU client.
    pub fn open(port: &str, baud_rate: u32, slave_id: u8) -> Result<Self, DeviceError> {
        let stream = tokio_serial::new(port, baud_rate)
            .data_bits(DataBits::Eight)
            .stop_bits(StopBits::One)
            .parity(Parity::None)
            .open_native_async()?;
        // Clear any residue from a previous session before attaching.
        let _ = stream.clear(ClearBuffer::All);
        Ok(Self::attach_serial(stream, slave_id))
    }

    /// Open the port described by `config`.
    pub fn connect(config: &Config) -> Result<Self, DeviceError> {
        Self::open(&config.port, config.baud_rate, config.slave_id)
    }

    /// Attach the RTU client to an already opened serial stream.
    pub fn attach_serial(stream: SerialStream, slave_id: u8) -> Self {
        let transport = FlushTransport {
            inner: stream,
            writing: false,
        };
        Self::attach(transport, slave_id)
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
        async move {
            let regs = self.ctx.read_holding_registers(addr, count).await??;
            if regs.len() != count as usize {
                return Err(DeviceError::InvalidData);
            }
            Ok(regs)
        }
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

/// A fault the [`MockBackend`] injects into the next transaction.
#[derive(Debug, Clone)]
pub enum Fault {
    /// Never answer (exercises the timeout path).
    Hang,
    /// Answer after a delay.
    Delay(Duration),
    /// Fail with an I/O error (e.g. USB unplugged).
    Io,
    /// Answer with a Modbus exception.
    Exception(tokio_modbus::ExceptionCode),
}

#[derive(Debug, Default)]
struct MockState {
    registers: Mutex<Vec<u16>>,
    faults: Mutex<VecDeque<Fault>>,
    offline: AtomicBool,
    requests: AtomicU64,
    request_times: Mutex<Vec<Instant>>,
}

/// In-memory backend for tests and simulation: a flat holding-register RAM
/// with fault injection.
///
/// Clones share the same state, so a test can keep one handle while the
/// actor owns another (and reconnects produce the same "device").
#[derive(Debug, Clone, Default)]
pub struct MockBackend {
    state: Arc<MockState>,
}

impl MockBackend {
    /// Create an empty backend with `size` zero-initialised registers.
    pub fn new(size: usize) -> Self {
        Self::with_registers(vec![0; size])
    }

    /// Create a backend pre-loaded with the given register contents.
    pub fn with_registers(registers: Vec<u16>) -> Self {
        let state = MockState {
            registers: Mutex::new(registers),
            ..Default::default()
        };
        Self {
            state: Arc::new(state),
        }
    }

    /// A 128-register RD6030 image: model 60301, serial 12345, firmware
    /// 1.32, 25 °C, Vin 24.00 V.
    pub fn rd6030() -> Self {
        let mock = Self::new(128);
        mock.set_register(register::reg::MODEL, 60301);
        mock.set_register(register::reg::SERIAL_LO, 12345);
        mock.set_register(register::reg::FIRMWARE, 132);
        mock.set_register(register::reg::TEMP_C, 25);
        mock.set_register(register::reg::TEMP_F, 77);
        mock.set_register(register::reg::VIN, 2400);
        mock
    }

    /// Snapshot of the current register contents.
    pub fn registers(&self) -> Vec<u16> {
        self.state.registers.lock().unwrap().clone()
    }

    /// Read a register directly (bypasses the async interface).
    pub fn register(&self, addr: u16) -> u16 {
        self.state.registers.lock().unwrap()[addr as usize]
    }

    /// Set a register value directly (bypasses the async interface).
    pub fn set_register(&self, addr: u16, value: u16) {
        if let Some(slot) = self.state.registers.lock().unwrap().get_mut(addr as usize) {
            *slot = value;
        }
    }

    /// Queue a fault for an upcoming transaction.
    pub fn inject(&self, fault: Fault) {
        self.state.faults.lock().unwrap().push_back(fault);
    }

    /// Simulate an unplugged device: transactions and reconnects fail.
    pub fn set_offline(&self, offline: bool) {
        self.state.offline.store(offline, Ordering::SeqCst);
    }

    /// Number of transactions served (including failed ones).
    pub fn request_count(&self) -> u64 {
        self.state.requests.load(Ordering::SeqCst)
    }

    /// Start times of all transactions.
    pub fn request_times(&self) -> Vec<Instant> {
        self.state.request_times.lock().unwrap().clone()
    }

    /// A connector that reopens this mock (and fails while it is offline).
    pub fn connector(&self) -> impl Connector<Backend = MockBackend> {
        let mock = self.clone();
        FnConnector(move || {
            if mock.state.offline.load(Ordering::SeqCst) {
                Err(DeviceError::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "mock device offline",
                )))
            } else {
                Ok(mock.clone())
            }
        })
    }

    /// Common prologue: count the request and apply a pending fault.
    async fn begin(state: Arc<MockState>) -> Result<(), DeviceError> {
        state.requests.fetch_add(1, Ordering::SeqCst);
        state.request_times.lock().unwrap().push(Instant::now());
        if state.offline.load(Ordering::SeqCst) {
            return Err(DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "mock device offline",
            )));
        }
        let fault = state.faults.lock().unwrap().pop_front();
        match fault {
            None => Ok(()),
            Some(Fault::Hang) => std::future::pending().await,
            Some(Fault::Delay(d)) => {
                tokio::time::sleep(d).await;
                Ok(())
            }
            Some(Fault::Io) => Err(DeviceError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "injected I/O error",
            ))),
            Some(Fault::Exception(code)) => Err(DeviceError::Exception(code)),
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
        let state = self.state.clone();
        async move {
            Self::begin(state.clone()).await?;
            let regs = state.registers.lock().unwrap();
            let start = addr as usize;
            let end = start + count as usize;
            if end > regs.len() {
                Err(DeviceError::Exception(
                    tokio_modbus::ExceptionCode::IllegalDataAddress,
                ))
            } else {
                Ok(regs[start..end].to_vec())
            }
        }
    }

    fn write_single(
        &mut self,
        addr: u16,
        value: u16,
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        Self::write_impl(self.state.clone(), addr, vec![value])
    }

    fn write_multiple(
        &mut self,
        addr: u16,
        values: &[u16],
    ) -> impl Future<Output = Result<(), DeviceError>> + Send {
        Self::write_impl(self.state.clone(), addr, values.to_vec())
    }
}

impl MockBackend {
    async fn write_impl(
        state: Arc<MockState>,
        addr: u16,
        values: Vec<u16>,
    ) -> Result<(), DeviceError> {
        Self::begin(state.clone()).await?;
        let mut regs = state.registers.lock().unwrap();
        let start = addr as usize;
        let end = start + values.len();
        if end > regs.len() {
            return Err(DeviceError::Exception(
                tokio_modbus::ExceptionCode::IllegalDataAddress,
            ));
        }
        regs[start..end].copy_from_slice(&values);
        Ok(())
    }
}

/// Tuning knobs of the device actor.
#[derive(Debug, Clone)]
pub struct DriverOptions {
    /// Per-transaction timeout.
    pub timeout: Duration,
    /// Background telemetry polling interval; `None` disables polling
    /// (one-shot CLI commands).
    pub poll_interval: Option<Duration>,
    /// Minimum idle gap between two transactions.
    pub min_gap: Duration,
    /// First reconnect back-off step.
    pub reconnect_min: Duration,
    /// Maximum reconnect back-off.
    pub reconnect_max: Duration,
    /// Read and verify the device identity after every (re)connect.
    pub handshake: bool,
    /// Accept a non-RD6030 model id during the handshake.
    pub allow_unknown_model: bool,
}

impl Default for DriverOptions {
    fn default() -> Self {
        Self {
            timeout: Duration::from_millis(500),
            poll_interval: Some(Duration::from_millis(100)),
            min_gap: Duration::from_millis(5),
            reconnect_min: Duration::from_millis(250),
            reconnect_max: Duration::from_secs(5),
            handshake: true,
            allow_unknown_model: false,
        }
    }
}

impl DriverOptions {
    /// Options derived from the configuration.
    pub fn from_config(config: &Config) -> Self {
        Self {
            timeout: config.timeout(),
            poll_interval: Some(config.poll_interval()),
            min_gap: Duration::from_millis(config.min_gap_ms),
            allow_unknown_model: config.allow_unknown_model,
            ..Self::default()
        }
    }

    /// Disable background polling.
    pub fn without_polling(mut self) -> Self {
        self.poll_interval = None;
        self
    }

    /// Back-off before reconnect attempt number `failures` (1-based). The
    /// first failure reconnects immediately: reopening the port is cheap and
    /// resynchronises the framing.
    fn backoff(&self, failures: u32) -> Duration {
        if failures <= 1 {
            return Duration::ZERO;
        }
        let exp = (failures - 2).min(16);
        self.reconnect_min
            .saturating_mul(1 << exp)
            .min(self.reconnect_max)
    }
}

enum Op {
    Read { addr: u16, count: u16 },
    WriteSingle { addr: u16, value: u16 },
    WriteMultiple { addr: u16, values: Vec<u16> },
}

enum Command {
    Exec {
        op: Op,
        reply: oneshot::Sender<Result<Vec<u16>, DeviceError>>,
    },
    Telemetry {
        reply: oneshot::Sender<Result<TelemetrySample, DeviceError>>,
    },
    Connect {
        reply: oneshot::Sender<Result<(), DeviceError>>,
    },
    Shutdown,
}

/// Cloneable handle to the device actor.
#[derive(Clone, Debug)]
pub struct Device {
    tx: mpsc::Sender<Command>,
    telemetry_rx: watch::Receiver<Option<TelemetrySample>>,
    link_rx: watch::Receiver<LinkStatus>,
    poll_interval: Option<Duration>,
}

impl Device {
    /// Open the configured serial port, spawn the actor and perform the
    /// identity handshake. Fails if the device cannot be reached.
    pub async fn connect(config: &Config) -> Result<Self, DeviceError> {
        Self::connect_with(config, DriverOptions::from_config(config)).await
    }

    /// Like [`connect`](Self::connect) with explicit driver options.
    pub async fn connect_with(
        config: &Config,
        options: DriverOptions,
    ) -> Result<Self, DeviceError> {
        let device = Self::start(SerialConnector::from_config(config), options);
        if let Err(err) = device.ensure_connected().await {
            device.shutdown().await;
            return Err(err);
        }
        Ok(device)
    }

    /// Spawn the actor with a connector. The first connection attempt is
    /// made by the first poll or command; the actor keeps retrying in the
    /// background, so this never fails.
    pub fn start<C: Connector>(connector: C, options: DriverOptions) -> Self {
        let (tx, rx) = mpsc::channel(64);
        let (telemetry_tx, telemetry_rx) = watch::channel(None);
        let (link_tx, link_rx) = watch::channel(LinkStatus::default());
        let poll_interval = options.poll_interval;
        let actor = Actor {
            connector,
            backend: None,
            options,
            telemetry_tx,
            link_tx,
            seq: 0,
            last_tx_end: None,
            next_connect_at: Instant::now(),
        };
        tokio::spawn(actor.run(rx));
        Self {
            tx,
            telemetry_rx,
            link_rx,
            poll_interval,
        }
    }

    /// Spawn the actor driving an already opened `backend` (it cannot be
    /// reopened after a transport failure).
    pub fn spawn<B: ModbusBackend>(backend: B, options: DriverOptions) -> Self {
        Self::start(OnceConnector(Some(backend)), options)
    }

    async fn request<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T, DeviceError>>) -> Command,
    ) -> Result<T, DeviceError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(make(reply))
            .await
            .map_err(|_| DeviceError::Disconnected)?;
        rx.await.map_err(|_| DeviceError::Disconnected)?
    }

    /// Make sure the link is up (opening the port and running the handshake
    /// if necessary).
    pub async fn ensure_connected(&self) -> Result<(), DeviceError> {
        self.request(|reply| Command::Connect { reply }).await
    }

    /// Read `count` contiguous holding registers starting at `addr`.
    pub async fn read_holding(&self, addr: u16, count: u16) -> Result<Vec<u16>, DeviceError> {
        self.request(|reply| Command::Exec {
            op: Op::Read { addr, count },
            reply,
        })
        .await
    }

    /// Write a single holding register.
    pub async fn write_single(&self, addr: u16, value: u16) -> Result<(), DeviceError> {
        self.request(|reply| Command::Exec {
            op: Op::WriteSingle { addr, value },
            reply,
        })
        .await
        .map(drop)
    }

    /// Write multiple contiguous holding registers.
    pub async fn write_multiple(&self, addr: u16, values: &[u16]) -> Result<(), DeviceError> {
        let values = values.to_vec();
        self.request(|reply| Command::Exec {
            op: Op::WriteMultiple { addr, values },
            reply,
        })
        .await
        .map(drop)
    }

    /// Read a telemetry snapshot from the bus now (also published to
    /// subscribers).
    pub async fn read_telemetry(&self) -> Result<TelemetrySample, DeviceError> {
        self.request(|reply| Command::Telemetry { reply }).await
    }

    /// Subscribe to telemetry updates (`None` until the first sample).
    pub fn subscribe(&self) -> watch::Receiver<Option<TelemetrySample>> {
        self.telemetry_rx.clone()
    }

    /// Get the most recent telemetry sample (no I/O).
    pub fn latest(&self) -> Option<TelemetrySample> {
        *self.telemetry_rx.borrow()
    }

    /// Subscribe to link-state updates.
    pub fn subscribe_link(&self) -> watch::Receiver<LinkStatus> {
        self.link_rx.clone()
    }

    /// Current link state and statistics.
    pub fn link(&self) -> LinkStatus {
        self.link_rx.borrow().clone()
    }

    /// Background polling interval, if polling is enabled.
    pub fn poll_interval(&self) -> Option<Duration> {
        self.poll_interval
    }

    /// Stop the actor task.
    pub async fn shutdown(&self) {
        let _ = self.tx.send(Command::Shutdown).await;
    }
}

struct Actor<C: Connector> {
    connector: C,
    backend: Option<C::Backend>,
    options: DriverOptions,
    telemetry_tx: watch::Sender<Option<TelemetrySample>>,
    link_tx: watch::Sender<LinkStatus>,
    seq: u64,
    last_tx_end: Option<Instant>,
    next_connect_at: Instant,
}

impl<C: Connector> Actor<C> {
    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut next_poll = Instant::now();
        loop {
            let poll = self.options.poll_interval;
            tokio::select! {
                biased;
                // Once due, a poll runs before any queued command.
                _ = sleep_until(next_poll), if poll.is_some() => {
                    let started = Instant::now();
                    let _ = self.telemetry().await;
                    next_poll = (started + poll.unwrap_or_default()).max(Instant::now());
                }
                cmd = rx.recv() => match cmd {
                    None | Some(Command::Shutdown) => break,
                    Some(Command::Exec { op, reply }) => {
                        let res = self.exec(&op).await;
                        let _ = reply.send(res);
                    }
                    Some(Command::Telemetry { reply }) => {
                        let res = self.telemetry().await;
                        let _ = reply.send(res);
                    }
                    Some(Command::Connect { reply }) => {
                        let res = self.ensure_backend().await;
                        let _ = reply.send(res);
                    }
                },
            }
        }
        debug!("device actor stopped");
    }

    /// Read and publish one telemetry snapshot.
    async fn telemetry(&mut self) -> Result<TelemetrySample, DeviceError> {
        let regs = self
            .exec(&Op::Read {
                addr: register::TELEMETRY_START,
                count: register::TELEMETRY_COUNT,
            })
            .await?;
        let telemetry = DeviceTelemetry::from_regs(&regs).ok_or(DeviceError::InvalidData)?;
        self.seq += 1;
        let sample = TelemetrySample {
            telemetry,
            timestamp: chrono::Utc::now(),
            seq: self.seq,
            received_at: Instant::now(),
        };
        self.telemetry_tx.send_replace(Some(sample));
        Ok(sample)
    }

    /// Run one transaction with connection management and bookkeeping.
    async fn exec(&mut self, op: &Op) -> Result<Vec<u16>, DeviceError> {
        self.ensure_backend().await?;
        let result = self.raw_exec(op).await;
        self.record(&result);
        result
    }

    /// Pace, then run one bounded transaction on the current backend.
    async fn raw_exec(&mut self, op: &Op) -> Result<Vec<u16>, DeviceError> {
        if let Some(end) = self.last_tx_end {
            sleep_until(end + self.options.min_gap).await;
        }
        let timeout = self.options.timeout;
        let Some(backend) = self.backend.as_mut() else {
            return Err(DeviceError::NotConnected("no backend".to_string()));
        };
        let fut = async {
            match op {
                Op::Read { addr, count } => backend.read_holding(*addr, *count).await,
                Op::WriteSingle { addr, value } => backend
                    .write_single(*addr, *value)
                    .await
                    .map(|()| Vec::new()),
                Op::WriteMultiple { addr, values } => backend
                    .write_multiple(*addr, values)
                    .await
                    .map(|()| Vec::new()),
            }
        };
        let result = match tokio::time::timeout(timeout, fut).await {
            Ok(result) => result,
            Err(_) => Err(DeviceError::Timeout(timeout)),
        };
        self.last_tx_end = Some(Instant::now());
        result
    }

    /// Open the backend if necessary (respecting the reconnect back-off) and
    /// run the identity handshake.
    async fn ensure_backend(&mut self) -> Result<(), DeviceError> {
        if self.backend.is_some() {
            return Ok(());
        }
        if Instant::now() < self.next_connect_at {
            let reason = self
                .link_tx
                .borrow()
                .last_error
                .clone()
                .unwrap_or_else(|| "reconnecting".to_string());
            return Err(DeviceError::NotConnected(reason));
        }
        let result = match self.connector.connect() {
            Ok(backend) => {
                self.backend = Some(backend);
                self.handshake().await
            }
            Err(err) => Err(err),
        };
        match result {
            Ok(info) => {
                self.link_tx.send_modify(|l| {
                    l.connects += 1;
                    l.consecutive_errors = 0;
                    if info.is_some() {
                        l.info = info;
                    }
                });
                self.set_state(LinkState::Connected, None);
                Ok(())
            }
            Err(err) => {
                self.backend = None;
                self.fail_link(&err);
                Err(err)
            }
        }
    }

    async fn handshake(&mut self) -> Result<Option<DeviceInfo>, DeviceError> {
        if !self.options.handshake {
            return Ok(None);
        }
        let regs = self
            .raw_exec(&Op::Read {
                addr: register::INFO_START,
                count: register::INFO_COUNT,
            })
            .await?;
        let info = DeviceInfo::from_regs(&regs).ok_or(DeviceError::InvalidData)?;
        match info.decoded_model() {
            Model::Rd6030 => {}
            Model::Other(raw) if self.options.allow_unknown_model => {
                warn!(
                    model = raw,
                    "unknown model accepted; RD6030 scaling applied"
                );
            }
            Model::Other(raw) => return Err(DeviceError::UnsupportedModel(raw)),
        }
        debug!(
            model = info.model,
            serial = info.serial_number,
            "handshake ok"
        );
        Ok(Some(info))
    }

    /// Update statistics after a transaction.
    fn record(&mut self, result: &Result<Vec<u16>, DeviceError>) {
        self.link_tx.send_modify(|l| l.requests += 1);
        match result {
            Ok(_) => {
                self.link_tx.send_modify(|l| l.consecutive_errors = 0);
                self.set_state(LinkState::Connected, None);
            }
            Err(err) if err.is_transport() => {
                debug!(error = %err, "transport error; reopening port");
                self.backend = None;
                self.fail_link(err);
            }
            Err(err) => {
                // The device answered (e.g. with an exception): link is fine.
                self.link_tx.send_modify(|l| {
                    l.errors += 1;
                    l.consecutive_errors = 0;
                    l.last_error = Some(err.to_string());
                });
                self.set_state(LinkState::Connected, None);
            }
        }
    }

    /// Record a link failure and schedule the next reconnect.
    fn fail_link(&mut self, err: &DeviceError) {
        let mut failures = 0;
        self.link_tx.send_modify(|l| {
            l.errors += 1;
            l.consecutive_errors += 1;
            if matches!(err, DeviceError::Timeout(_)) {
                l.timeouts += 1;
            }
            l.last_error = Some(err.to_string());
            failures = l.consecutive_errors;
        });
        self.next_connect_at = Instant::now() + self.options.backoff(failures);
        let state = if failures >= 3 || matches!(err, DeviceError::UnsupportedModel(_)) {
            LinkState::Disconnected
        } else {
            LinkState::Degraded
        };
        self.set_state(state, Some(err));
    }

    /// Change the link state, logging only on transitions.
    fn set_state(&mut self, state: LinkState, err: Option<&DeviceError>) {
        let previous = self.link_tx.borrow().state;
        if previous == state {
            return;
        }
        self.link_tx.send_modify(|l| l.state = state);
        match (state, err) {
            (LinkState::Connected, _) => info!("device link up"),
            (LinkState::Degraded, Some(err)) => warn!(error = %err, "device link degraded"),
            (LinkState::Disconnected, Some(err)) => {
                warn!(error = %err, "device link down; retrying in background")
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quiet() -> DriverOptions {
        DriverOptions {
            poll_interval: None,
            ..DriverOptions::default()
        }
    }

    #[tokio::test]
    async fn actor_serializes_requests() {
        let mock = MockBackend::rd6030();
        let device = Device::start(mock.connector(), quiet());

        device
            .write_single(register::reg::VSET, 1200)
            .await
            .unwrap();
        device.write_single(register::reg::ISET, 250).await.unwrap();
        device.write_single(register::reg::OUTPUT, 1).await.unwrap();

        assert_eq!(
            device.read_holding(register::reg::VSET, 2).await.unwrap(),
            vec![1200, 250]
        );
        assert_eq!(
            device.read_holding(register::reg::OUTPUT, 1).await.unwrap(),
            vec![1]
        );
        assert_eq!(device.link().state, LinkState::Connected);
        assert_eq!(device.link().info.unwrap().model, 60301);

        device.shutdown().await;
    }

    #[tokio::test]
    async fn telemetry_is_published() {
        let mock = MockBackend::rd6030();
        mock.set_register(register::reg::VOUT, 1200);
        mock.set_register(register::reg::IOUT, 250);
        mock.set_register(register::reg::POUT_LO, 3000);
        mock.set_register(register::reg::OUTPUT, 1);

        let options = DriverOptions {
            poll_interval: Some(Duration::from_millis(10)),
            ..DriverOptions::default()
        };
        let device = Device::start(mock.connector(), options);
        let mut rx = device.subscribe();
        let sample = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                rx.changed().await.unwrap();
                if let Some(s) = *rx.borrow_and_update() {
                    return s;
                }
            }
        })
        .await
        .expect("sample within 2 s");

        assert_eq!(sample.seq, 1);
        assert_eq!(sample.telemetry.voltage_out, 12.0);
        assert_eq!(sample.telemetry.current_out, 2.5);
        assert_eq!(sample.telemetry.power_out, 30.0);
        assert_eq!(sample.telemetry.voltage_in, 24.0);
        assert!(sample.telemetry.output_enabled);

        device.shutdown().await;
    }

    #[tokio::test]
    async fn no_telemetry_before_first_sample() {
        let device = Device::start(MockBackend::rd6030().connector(), quiet());
        assert!(device.latest().is_none());
        let s = device.read_telemetry().await.unwrap();
        assert_eq!(device.latest(), Some(s));
        device.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_device_times_out_and_recovers() {
        let mock = MockBackend::rd6030();
        let device = Device::start(mock.connector(), quiet());
        device.ensure_connected().await.unwrap();
        let connects = device.link().connects;

        mock.inject(Fault::Hang);
        let err = device.read_holding(0, 1).await.unwrap_err();
        assert!(matches!(err, DeviceError::Timeout(_)), "{err}");
        let link = device.link();
        assert_eq!(link.timeouts, 1);
        assert_eq!(link.state, LinkState::Degraded);

        // First failure reconnects immediately with a fresh backend.
        assert_eq!(device.read_holding(0, 1).await.unwrap(), vec![60301]);
        let link = device.link();
        assert_eq!(link.state, LinkState::Connected);
        assert_eq!(link.connects, connects + 1);
        device.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn exception_keeps_link_up() {
        let mock = MockBackend::rd6030();
        let device = Device::start(mock.connector(), quiet());
        device.ensure_connected().await.unwrap();
        mock.inject(Fault::Exception(
            tokio_modbus::ExceptionCode::IllegalDataAddress,
        ));
        let err = device.read_holding(0, 1).await.unwrap_err();
        assert!(matches!(err, DeviceError::Exception(_)));
        assert_eq!(device.link().state, LinkState::Connected);
        assert_eq!(device.link().connects, 1);
        device.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn unplug_backs_off_and_reconnects() {
        let mock = MockBackend::rd6030();
        let device = Device::start(mock.connector(), quiet());
        device.ensure_connected().await.unwrap();

        mock.set_offline(true);
        // Transport error -> immediate reconnect fails -> back-off.
        assert!(device.read_holding(0, 1).await.is_err());
        assert!(device.read_holding(0, 1).await.is_err());
        assert_eq!(device.link().state, LinkState::Degraded);
        // While backing off, requests fail fast without touching the port.
        let before = mock.request_count();
        assert!(matches!(
            device.read_holding(0, 1).await.unwrap_err(),
            DeviceError::NotConnected(_)
        ));
        assert_eq!(mock.request_count(), before);
        // Further failed attempts mark the link as down.
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_secs(1)).await;
            let _ = device.read_holding(0, 1).await;
        }
        assert_eq!(device.link().state, LinkState::Disconnected);

        mock.set_offline(false);
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(device.read_holding(0, 1).await.unwrap(), vec![60301]);
        assert_eq!(device.link().state, LinkState::Connected);
        device.shutdown().await;
    }

    #[tokio::test]
    async fn unsupported_model_is_rejected() {
        let mock = MockBackend::rd6030();
        mock.set_register(register::reg::MODEL, 60065); // RD6006P
        let device = Device::start(mock.connector(), quiet());
        let err = device.ensure_connected().await.unwrap_err();
        assert!(matches!(err, DeviceError::UnsupportedModel(60065)), "{err}");
        assert_eq!(device.link().state, LinkState::Disconnected);

        let device = Device::start(
            mock.connector(),
            DriverOptions {
                allow_unknown_model: true,
                ..quiet()
            },
        );
        device.ensure_connected().await.unwrap();
        device.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn transactions_respect_min_gap() {
        let mock = MockBackend::rd6030();
        let options = DriverOptions {
            min_gap: Duration::from_millis(20),
            ..quiet()
        };
        let device = Device::start(mock.connector(), options);
        for _ in 0..5 {
            device.read_holding(0, 1).await.unwrap();
        }
        let times = mock.request_times();
        assert_eq!(times.len(), 6); // handshake + 5
        for pair in times.windows(2) {
            assert!(pair[1] - pair[0] >= Duration::from_millis(20));
        }
        device.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn polling_is_not_starved_by_command_flood() {
        let mock = MockBackend::rd6030();
        let options = DriverOptions {
            poll_interval: Some(Duration::from_millis(100)),
            min_gap: Duration::from_millis(5),
            ..DriverOptions::default()
        };
        let device = Device::start(mock.connector(), options);
        let flood = {
            let device = device.clone();
            tokio::spawn(async move {
                for _ in 0..400 {
                    let _ = device.write_single(register::reg::VSET, 100).await;
                }
            })
        };
        tokio::time::sleep(Duration::from_secs(1)).await;
        let seq = device.latest().map(|s| s.seq).unwrap_or(0);
        assert!(seq >= 8, "only {seq} polls in 1 s under load");
        flood.abort();
        device.shutdown().await;
    }

    #[test]
    fn backoff_grows_and_saturates() {
        let o = DriverOptions::default();
        assert_eq!(o.backoff(1), Duration::ZERO);
        assert_eq!(o.backoff(2), Duration::from_millis(250));
        assert_eq!(o.backoff(3), Duration::from_millis(500));
        assert_eq!(o.backoff(10), Duration::from_secs(5));
        assert_eq!(o.backoff(u32::MAX), Duration::from_secs(5));
    }
}
