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
