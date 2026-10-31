# Hardware-in-the-loop (HIL) checklist

The automated HIL suite (`crates/rd6030-core/tests/hil.rs`) and the manual checks below verify the
software against a real RD6030. They also settle the register-map details marked `VERIFY(HIL)` in
the source. Record the results (firmware version included) in an issue or in this file.

## Setup

1. Connect the RD6030 over USB. **Disconnect any load** from the output terminals.
2. Install the udev rule (`packaging/99-rd6030.rules`) or use `/dev/ttyUSB0` directly.
3. Check communication: `rd6030-cli --port /dev/rd6030 status`.

## Automated suite

```sh
# Read-only checks + volatile setpoint writes (restored afterwards):
RD6030_PORT=/dev/rd6030 cargo test -p rd6030-core --test hil -- --ignored --test-threads=1

# Include the output toggle (1 V / 0.1 A, no load!), RTC sync and a 1 h soak:
RD6030_PORT=/dev/rd6030 RD6030_HIL_OUTPUT=1 RD6030_HIL_RTC=1 RD6030_HIL_SOAK_SECS=3600 \
  cargo test -p rd6030-core --test hil -- --ignored --test-threads=1 --nocapture
```

| Test | Checks |
|------|--------|
| `hil_identity` | Model id decodes as RD6030, firmware/temperature/Vin plausible |
| `hil_setpoint_readback_restores_original` | 0x10 and 0x06 setpoint writes read back exactly; original restored |
| `hil_output_toggle_low_voltage` | Output on at 1 V reaches ~1 V; output off again |
| `hil_preset_bank_layout_is_plausible` | Registers 80..=119 decode as 10 × (V, I, OVP, OCP); M0 OVP/OCP = active protection; re-import writes nothing |
| `hil_rtc_and_battery` | RTC and battery blocks readable; optional sync round trip |
| `hil_soak_polling_at_20hz` | No timeouts, link stays connected, ≥ 15 samples/s at 50 ms polling |

## Manual checks (open register-map questions)

| # | Question | Procedure | Expected / decision |
|---|----------|-----------|---------------------|
| M1 | **Preset bank layout** (80 + 4·n) | `rd6030-cli preset export`, then compare with the M0..M9 values shown on the front panel | Matches → remove `VERIFY(HIL)` in `register.rs`/`preset.rs` |
| M2 | **Energy scale** (`ENERGY_SCALE`, 1 mWh vs 10 mWh) | Charge a battery in battery mode for a while; compare `rd6030-cli battery` Wh with the display | Matches the display → keep 1000; 10× too small → set 100 |
| M3 | **Calibration commit** (reg 54 = `0x1501`) | **Do not test casually.** Only with a known-good calibration backup (`rd6030-cli calibrate read` first) | Document the result; keep `allow_calibration = false` by default |
| M4 | **Does writing Vset/Iset also write M0 (EEPROM wear)?** | `rd6030-cli set --voltage 7.77`, power-cycle the PSU, then `rd6030-cli status` | Setpoint persisted → firmware mirrors into M0: document, and rate-limit ramps further |
| M5 | **OVP/OCP upper bounds** (`MAX_OVP`/`MAX_OCP`) | `rd6030-cli protection set --ovp 62 --ocp 31`, read back; then try slightly higher via the front panel | Adjust the constants to the firmware limits |
| M6 | **Reconnect on replug** | `rd6030-cli telemetry --watch`, unplug USB for 10 s, replug | Link goes `degraded` → `disconnected`, then recovers without restarting (with the udev symlink) |
| M7 | **Recall semantics of register 19** | `rd6030-cli preset select 3`, check that Vset/Iset and OVP/OCP follow M3 | Document; the simulator assumes V/I/OVP/OCP are loaded |
| M8 | **Keypad lock** | `rd6030-cli lock on`, then try the front panel; `lock off` | Panel locked/unlocked |
| M9 | **System options** (66..=72) | `rd6030-cli system get` vs the device settings menu; `system backlight 2` | Values match → remove `VERIFY(HIL)` in `system.rs` |

## Debugging a failing HIL run

* `RUST_LOG=rd6030_core=trace` prints every frame (`serial tx`/`serial rx` hex dumps).
* `rd6030-cli raw read 0 20` dumps the first registers.
* Timeouts on every request: wrong baud rate or slave id (device menu), brltty holding the port, or
  another process (GUI/server) has the port open. See the README troubleshooting table.

## Results: first hardware run (2026-10-31)

Device: RD6030, model id 60301, serial 9633, **firmware 1.47** (original Riden firmware), Vin 68 V.
Link: CH340 via usbip (Hyper-V host → Ubuntu VM), 115200 8N1, slave 1. No load on the output
(a motor controller was disconnected) except for the resistive-load tests.

| Area | Result |
|------|--------|
| Register map 0–19, 32–41, 48–53, 55–62, 66–72, 80–119 | Decoded identically by an independent pymodbus dump and by `rd6030-cli` |
| FC 0x03 / 0x06 / 0x10 | All supported. Invalid addresses (130, 300) return zeros, **no** Modbus exception |
| Latency | 70–150 ms per transaction over usbip → ~6.8 telemetry samples/s at `poll_interval_ms=50`. 0 errors in 30 s soak. `hil_soak` threshold is now `RD6030_HIL_MIN_RATE` (default 5) |
| Setpoints | Volatile. FC 0x06 and 0x10 writes to 8/9 do **not** change M0, neither immediately nor after 4 min idle (M4). M0 V/I did change once, most likely through front-panel interaction |
| Output on, no load | 1.00 V / 3.30 V reached exactly, Iout 0, CV |
| OVP (M5) | Writing 82/83 takes effect immediately; trip → output off, register 16 = 1 latched until output on. 62.00 V / 30.20 A accepted (upper bounds not probed further) |
| Preset bank (M1) | Layout 80 + 4·n confirmed. Factory values: M1–M9 = 5.00 V / **30.10 A**, OVP 62.00 V / OCP 30.20 A |
| Preset recall (M7) | Writing 19 = n (1..9) loads M*n*'s V/I **and activates M*n*'s OVP/OCP without updating 82/83**. Writing a slot's OVP while it is active has no effect until the next recall. Recalling M0 does not reload V/I |
| Keypad lock (M8) | Register 15 reads 1 while remote-controlled, writes are ignored, the panel shows the lock icon but the keys still work |
| System options (M9) | Backlight write + readback OK |
| RTC | Was 2019-01-01 (never set); `rtc sync` OK |
| Error handling | Wrong baud/slave → clear timeout after 0.5 s; port in use → "Device or resource busy" |
| REST / MCP | All endpoints and tools tested against hardware; limits (422 / `isError`), bearer token, SSE, `output_off_on_exit` OK |
| Resistive load (nominal 750 Ω ±10 %, 5 W) | 10/20/30/40 V → 0/20/30/40 mA, Pout = Vout·Iout exactly (32-bit power decoding OK). The device implies 1000 Ω: either the resistor is ~1 kΩ or the current reading is low (unresolved, no ohmmeter). Currents ≤ ~10 mA read as 0 |
| Resistive load 248.3 Ω (ohmmeter-measured, 15 W) | 5–40 V: Iout reads a constant **~10–11 mA below** V/R (e.g. 40 V: 150 mA vs 161.1 mA); slope matches 1/248.3 Ω, so the gain is right and the error is a zero offset of about one LSB in the device's current measurement (calibration, not software). Pout = Vout·Iout everywhere |
| Resistive load 187.8 Ω (measured, 30 W), 5–60 V | Full range to 60.01 V / 18 W without errors. Fit over 12 CV points: Iout = 0.994 · (V/R) − 12.8 mA. The error alternates between −10 and −19 mA every 5 V step: a constant offset of ~10–15 mA, quantised to the 10 mA resolution. Gain error (−0.6 %) is within the ohmmeter/lead uncertainty. CC at 100/200/300 mA → true 116/214/311 mA. OCP 0.20 A at 266 mA → trip |
| CC on 248.3 Ω | Iset 50/100/150 mA → true current (Vout/R) 63/114/159 mA, i.e. the regulation also sits ~10–14 mA above the setpoint, consistent with the same offset |
| OCP on 248.3 Ω | OCP 0.10 A at 121 mA → trip, `ocp` |
| CC | Iset 30 mA / 20 mA at 40 V → CC flag set, Vout 34.0 V / 26.4 V (regulation a few mA above the setpoint at this tiny current) |
| OCP | OCP 0.02 A at ~30–40 mA → output off, register 16 = 2 (`ocp`), latched until output on |
| Not tested | M2 energy scale (no battery), M3 calibration (deliberately), absolute accuracy (no multimeter), M6 replug, power-cycle behaviour, GUI on screen |

Bugs found and fixed by this run:

1. `select_preset` bypassed the software limits (recalling M9 set Iset = 30.10 A with a 0.5 A limit). The slot is now validated first.
2. `protection()` always reported 82/83, which is wrong after a preset recall. It now reads the recalled slot's OVP/OCP, and `set_protection` updates that slot as well. New HIL test `hil_recalled_preset_protection_is_reported_and_settable` (needs `RD6030_HIL_OUTPUT=1 RD6030_HIL_EEPROM=1`).
3. `import_presets` validated unchanged slots, so a bank exported from a factory device could not be re-imported. Only slots that change are validated now.
4. `hil_preset_bank_layout_is_plausible` assumed currents ≤ 30.0 A.
5. `rd6030-cli lock` printed the requested state; it now reads back and explains when the firmware ignores the request.
6. GUI: the output toggle was derived from telemetry that defaults to "off", so clicking it with stale data could switch the output **on**. It now has explicit On/Off buttons (On only with fresh telemetry). "Apply" is disabled until the device setpoint has been loaded (it could write 0 V / 0 A).
7. The simulator now models the firmware's preset/protection behaviour.
