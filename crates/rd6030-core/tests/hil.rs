//! Hardware-in-the-loop tests against a real RD6030.
//!
//! All tests are `#[ignore]`d and additionally require `RD6030_PORT`:
//!
//! ```sh
//! RD6030_PORT=/dev/rd6030 cargo test -p rd6030-core --test hil -- --ignored --test-threads=1
//! ```
//!
//! Optional switches (see `docs/hil.md`):
//! * `RD6030_HIL_OUTPUT=1` — allow switching the output on (1 V / 0.1 A,
//!   **disconnect any load first**);
//! * `RD6030_HIL_RTC=1` — allow syncing the RTC;
//! * `RD6030_HIL_SOAK_SECS=3600` — soak duration (default 30 s).
//!
//! The tests restore the original setpoints. They never write the preset
//! bank, protection thresholds or calibration (EEPROM/NVRAM).

use std::time::Duration;

use rd6030_core::config::Config;
use rd6030_core::register::{self, Model};
use rd6030_core::{Controller, DriverOptions, LinkState, PresetBank, Setpoint};

fn port() -> Option<String> {
    std::env::var("RD6030_PORT").ok()
}

fn flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1")
}

async fn connect(poll_ms: Option<u64>) -> Option<Controller> {
    let Some(port) = port() else {
        eprintln!("RD6030_PORT not set; skipping");
        return None;
    };
    let config = Config {
        port,
        ..Config::default()
    };
    let mut options = DriverOptions::from_config(&config);
    options.poll_interval = poll_ms.map(Duration::from_millis);
    Some(
        Controller::connect_with(&config, options)
            .await
            .expect("connect"),
    )
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT)"]
async fn hil_identity() {
    let Some(c) = connect(None).await else { return };
    let info = c.info().await.unwrap();
    eprintln!("identity: {info:?}");
    assert_eq!(Model::from_raw(info.model), Model::Rd6030);
    assert!((1.0..10.0).contains(&info.firmware));
    assert!((-20.0..90.0).contains(&info.temperature_c));
    let t = c.telemetry().await.unwrap();
    eprintln!("telemetry: {t:?}");
    assert!(t.voltage_in > 5.0, "Vin {} V looks wrong", t.voltage_in);
    c.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT)"]
async fn hil_setpoint_readback_restores_original() {
    let Some(c) = connect(None).await else { return };
    let original = c.setpoint().await.unwrap();
    let test = Setpoint {
        voltage: 3.21,
        current: 0.43,
    };
    c.set_setpoint(test).await.unwrap();
    assert_eq!(c.setpoint().await.unwrap(), test);
    c.set_voltage(4.56).await.unwrap();
    assert_eq!(c.voltage_setpoint().await.unwrap(), 4.56);
    c.set_setpoint(original).await.unwrap();
    assert_eq!(c.setpoint().await.unwrap(), original);
    c.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT) and RD6030_HIL_OUTPUT=1"]
async fn hil_output_toggle_low_voltage() {
    if !flag("RD6030_HIL_OUTPUT") {
        eprintln!("RD6030_HIL_OUTPUT!=1; skipping");
        return;
    }
    let Some(c) = connect(Some(100)).await else {
        return;
    };
    let original = c.setpoint().await.unwrap();
    c.set_output(false).await.unwrap();
    c.set_setpoint(Setpoint {
        voltage: 1.0,
        current: 0.1,
    })
    .await
    .unwrap();
    c.set_output(true).await.unwrap();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let t = c.device().read_telemetry().await.unwrap().telemetry;
    eprintln!("output on: {t:?}");
    c.set_output(false).await.unwrap();
    c.set_setpoint(original).await.unwrap();
    assert!(t.output_enabled);
    assert!((t.voltage_out - 1.0).abs() < 0.1, "Vout {}", t.voltage_out);
    c.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT)"]
async fn hil_preset_bank_layout_is_plausible() {
    let Some(c) = connect(None).await else { return };
    let bank = c.export_presets().await.unwrap();
    eprintln!("{}", bank.to_json().unwrap());
    for (i, p) in bank.slots.iter().enumerate() {
        assert!(p.voltage <= register::MAX_VOLTAGE, "M{i} V {}", p.voltage);
        assert!(p.current <= register::MAX_CURRENT, "M{i} I {}", p.current);
        assert!(p.ovp <= 70.0 && p.ocp <= 35.0, "M{i} {p:?}");
    }
    // Re-importing the exported bank must not write anything.
    let roundtrip = PresetBank::from_json(&bank.to_json().unwrap()).unwrap();
    assert!(c.import_presets(&roundtrip).await.unwrap().is_empty());
    let prot = c.protection().await.unwrap();
    assert_eq!((prot.ovp, prot.ocp), (bank.slots[0].ovp, bank.slots[0].ocp));
    c.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT)"]
async fn hil_rtc_and_battery() {
    let Some(c) = connect(None).await else { return };
    let rtc = c.read_rtc().await.unwrap();
    eprintln!(
        "rtc: {rtc}, battery: {:?}",
        c.battery_status().await.unwrap()
    );
    if flag("RD6030_HIL_RTC") {
        let synced = c.sync_rtc().await.unwrap();
        let back = c.read_rtc().await.unwrap();
        assert_eq!(
            (back.year, back.month, back.day),
            (synced.year, synced.month, synced.day)
        );
    }
    c.shutdown().await;
}

#[tokio::test]
#[ignore = "requires RD6030 hardware (RD6030_PORT)"]
async fn hil_soak_polling_at_20hz() {
    let Some(c) = connect(Some(50)).await else {
        return;
    };
    let secs = std::env::var("RD6030_HIL_SOAK_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30u64);
    let start = c.latest().map_or(0, |s| s.seq);
    tokio::time::sleep(Duration::from_secs(secs)).await;
    let link = c.link();
    let samples = c.latest().map_or(0, |s| s.seq) - start;
    eprintln!("soak {secs}s: {samples} samples, {link:?}");
    assert_eq!(link.state, LinkState::Connected);
    assert_eq!(link.timeouts, 0);
    assert!(
        samples as f64 >= secs as f64 * 15.0,
        "only {samples} samples"
    );
    c.shutdown().await;
}
