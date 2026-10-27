# powerSupplyControl

Full control software for the **Riden RD6030** programmable DC power supply,
written entirely in Rust. It talks to the device over USB (CH340 UART bridge)
using **Modbus RTU** and exposes control through a CLI, a REST API, an MCP
server, and an egui GUI.

## Workspace layout

| Crate | Kind | Purpose |
|-------|------|---------|
| `rd6030-core` | library | Register map, unit scaling, typed models, async actor device driver, controller, presets/RTC/battery/calibration/logging |
| `rd6030-cli` | binary | Command-line interface (`info`, `telemetry`, `set`, `output`, `preset`, `rtc`, `battery`, `calibrate`, `log`, `serve`, `mcp`, `gui`) |
| `rd6030-server` | lib + binary | REST API (axum) and MCP server (`rmcp`), stdio + streamable HTTP |
| `rd6030-gui` | binary | egui/eframe desktop GUI with live telemetry and plotting |

## Features

- Read device identity (model, serial number, firmware) and temperatures.
- Live telemetry (Vout, Iout, Pout, Vin, CV/CC mode, protection, output state)
  polled in the background at a configurable 10–20 Hz.
- Set voltage/current setpoints (volatile registers 8/9) and toggle the output.
- Preset slot selection (M0–M9) and setpoint export/import as JSON.
- Real-time clock read/set/sync.
- Read-only battery charging status.
- Calibration (NVRAM commit `0x1501`), guarded behind an explicit confirmation.
- CSV / JSON-lines telemetry logging.
- REST API and MCP tool server (stdio + streamable HTTP).

## Building

Prerequisites:

- Rust **1.95+** (`rustup` recommended).
- On Linux, the GUI needs the usual X11/Wayland/EGL development libraries
  (e.g. `libxkbcommon-dev`, `libgl1-mesa-dev`, `libwayland-dev`, `libx11-dev`).

```sh
cargo build --workspace --release
```

The binaries land in `target/release/`:

- `rd6030-cli` — the main entry point (also dispatches `serve`/`mcp`/`gui`).
- `rd6030-server` — REST + MCP server.
- `rd6030-gui` — desktop GUI.

## Configuration

Copy [`config.example.toml`](config.example.toml) to `config.toml` and adjust the
serial port. All fields are optional and fall back to the documented defaults:

```toml
port = "/dev/ttyUSB0"   # Windows: "COM3", macOS: "/dev/tty.usbserial-*"
baud_rate = 115200      # 9600..=115200
slave_id = 1            # 1..=247
timeout_ms = 500        # >= 300 ms recommended
poll_interval_ms = 100  # 50..=1000 ms (10..=20 Hz)

[server]
host = "127.0.0.1"
port = 8080

[logging]
enabled = false
path = "rd6030-telemetry.csv"
format = "csv"          # "csv" or "jsonlines"
interval_ms = 1000
```

## CLI

```sh
rd6030-cli --port /dev/ttyUSB0 info
rd6030-cli telemetry --watch
rd6030-cli set --voltage 12.0 --current 2.5
rd6030-cli output on
rd6030-cli preset select 2
rd6030-cli rtc sync
rd6030-cli battery
rd6030-cli log start --path telemetry.csv
```

Dangerous operations require an explicit confirmation flag:

```sh
rd6030-cli calibrate commit --yes
```

## REST API

Start the server with `rd6030-cli serve` (or `rd6030-server serve`), then:

| Method | Path | Description |
|--------|------|-------------|
| GET | `/api/info` | Device identity, serial, firmware, temperatures |
| GET | `/api/telemetry` | One telemetry snapshot |
| POST | `/api/output` | `{"enabled": bool}` |
| POST | `/api/set` | `{"voltage": f32?, "current": f32?}` |
| GET | `/api/presets` | Active slot and setpoint |
| POST | `/api/presets/select` | `{"slot": 0..9}` |
| GET | `/api/battery` | Battery charging status |
| GET/PUT | `/api/rtc` | Read/set the RTC |
| POST | `/api/rtc/sync` | Sync RTC to host clock |
| POST | `/api/calibrate/commit` | `{"confirm": true}` (NVRAM) |

## MCP

Run the MCP server over stdio (for Claude Desktop / IDEs):

```sh
rd6030-cli mcp
```

The streamable-HTTP MCP endpoint is mounted at `/mcp` on the same HTTP server
as the REST API (`rd6030-cli serve`). Tools: `get_telemetry`, `get_info`,
`set_output`, `set_voltage`, `set_current`.

## Register reference

32-bit values combine two consecutive registers in **big-endian** order (high
word at the lower address).

| Address (hex) | Register | Type | Scale | Access |
|---------------|----------|------|-------|--------|
| `0x0000` | Model id | u16 | raw | R |
| `0x0001–0x0002` | Serial number | u32 | raw | R |
| `0x0003` | Firmware version | u16 | ×100 | R |
| `0x0004–0x0007` | Internal temperature (°C / °F) | sign + value | 1 °C/°F | R |
| `0x0008` | Voltage setpoint (Vset) | u16 | 10 mV | R/W |
| `0x0009` | Current setpoint (Iset) | u16 | 10 mA | R/W |
| `0x000A` | Output voltage (Vout) | u16 | 10 mV | R |
| `0x000B` | Output current (Iout) | u16 | 10 mA | R |
| `0x000C–0x000D` | Output power (Pout) | u32 | 10 mW | R |
| `0x000E` | Input voltage (Vin) | u16 | 10 mV | R |
| `0x000F` | Keypad lock | u16 | 0/1 | R/W |
| `0x0010` | Protection status | u16 | 0=OK, 1=OVP, 2=OCP | R |
| `0x0011` | CV/CC mode | u16 | 0=CV, 1=CC | R |
| `0x0012` | Output enable | u16 | 0/1 | R/W |
| `0x0013` | Preset slot | u16 | 0–9 (M0–M9) | R/W |
| `0x0020–0x0029` | Battery status / voltage / temps / capacity / energy | mixed | see code | R |
| `0x0030–0x0035` | RTC (year, month, day, hour, minute, second) | 6×u16 | — | R/W |
| `0x0036` | Calibration commit | u16 | `0x1501` | W |

## Hardware notes

- The RD6030 is a Modbus-RTU **slave**; the device is the master here.
- Poll the telemetry block (registers 10–18) in one 0x03 read; keep the poll
  interval at 10–20 Hz to avoid overloading the device MCU.
- Setpoints (registers 8/9) are **volatile** and lost on cold start. The
  non-volatile preset bank (M0–M9) uses a vendor-specific memory command that
  is not part of the documented register map and is out of scope here (see
  `rssdev10/riden-flashtool`).
- Do not persist presets or calibration from automated sweep/ramp loops — this
  degrades the device's internal flash/EEPROM.

## License

[GPL-2.0-or-later](LICENSE).
