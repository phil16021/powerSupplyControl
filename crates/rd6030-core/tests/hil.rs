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
//! * `RD6030_HIL_SOAK_SECS=3600` — soak duration (default 30 s);
//! * `RD6030_HIL_MIN_RATE=5` — minimum telemetry samples/s in the soak
//!   (default 5; fw 1.47 answers in ~70–150 ms, so ~7/s is the ceiling over
//!   usbip — raise it on native USB if the device is faster);
//! * `RD6030_HIL_EEPROM=1` — allow the preset-recall/protection test, which
//!   writes M9 and registers 82/83 (EEPROM) and restores them afterwards.
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
    // Factory presets on fw 1.47 hold 30.10 A / OCP 30.20 A, i.e. slightly
    // above the 30 A rating: bound by the protection maxima instead.
    for (i, p) in bank.slots.iter().enumerate() {
        assert!(p.voltage <= register::MAX_OVP, "M{i} V {}", p.voltage);
        assert!(p.current <= register::MAX_OCP, "M{i} I {}", p.current);
        assert!(p.ovp <= register::MAX_OVP, "M{i} {p:?}");
        assert!(p.ocp <= register::MAX_OCP, "M{i} {p:?}");
    }
    // Re-importing the exported bank must not write anything.
    let roundtrip = PresetBank::from_json(&bank.to_json().unwrap()).unwrap();
    assert!(c.import_presets(&roundtrip).await.unwrap().is_empty());
    if c.active_preset().await.unwrap() == 0 {
        let prot = c.protection().await.unwrap();
        assert_eq!((prot.ovp, prot.ocp), (bank.slots[0].ovp, bank.slots[0].ocp));
    }
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
    let min_rate: f64 = std::env::var("RD6030_HIL_MIN_RATE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5.0);
    let rate = samples as f64 / secs as f64;
    eprintln!("rate {rate:.1} samples/s (min {min_rate})");
    assert!(rate >= min_rate, "only {rate:.1} samples/s");
    c.shutdown().await;
}

/// Recalling a preset activates *its* OVP/OCP (fw 1.47), and
/// `set_protection` must change the thresholds that are actually active.
/// Output on at <= 4 V, **no load**. Writes M9 + 82/83 and restores them.
#[tokio::test]
#[ignore = "requires RD6030 hardware, RD6030_HIL_OUTPUT=1 and RD6030_HIL_EEPROM=1"]
async fn hil_recalled_preset_protection_is_reported_and_settable() {
    use rd6030_core::{Preset, Protection};
    if !flag("RD6030_HIL_OUTPUT") || !flag("RD6030_HIL_EEPROM") {
        eprintln!("RD6030_HIL_OUTPUT/RD6030_HIL_EEPROM != 1; skipping");
        return;
    }
    let Some(c) = connect(None).await else { return };
    c.set_output(false).await.unwrap();
    let original_slot = c.active_preset().await.unwrap();
    let original_m9 = c.read_preset(9).await.unwrap();
    let original_m0_prot = c.read_raw(register::reg::M0_OVP, 2).await.unwrap();
    let original_sp = c.setpoint().await.unwrap();

    let trips_at_4v = |c: Controller| async move {
        c.set_voltage(4.0).await.unwrap();
        c.set_output(true).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let t = c.device().read_telemetry().await.unwrap().telemetry;
        c.set_output(false).await.unwrap();
        eprintln!("4 V test: {t:?}");
        t.protection == rd6030_core::ProtectionStatus::Ovp
    };

    c.write_preset(
        9,
        Preset {
            voltage: 2.0,
            current: 0.1,
            ovp: 3.0,
            ocp: 1.0,
        },
    )
    .await
    .unwrap();
    c.select_preset(9).await.unwrap();
    let reported = c.protection().await.unwrap();
    let tripped_m9 = trips_at_4v(c.clone()).await;
    c.set_protection(Protection {
        ovp: 10.0,
        ocp: 1.0,
    })
    .await
    .unwrap();
    let reported_after_set = c.protection().await.unwrap();
    let tripped_after_set = trips_at_4v(c.clone()).await;

    // Restore: slot, M9, 82/83 and the setpoint.
    c.select_preset(0).await.ok();
    c.device()
        .write_multiple(register::preset_base(9), &original_m9.to_regs())
        .await
        .unwrap();
    c.device()
        .write_multiple(register::reg::M0_OVP, &original_m0_prot)
        .await
        .unwrap();
    if original_slot != 0 {
        c.device()
            .write_single(register::reg::PRESET, original_slot.into())
            .await
            .unwrap();
    }
    c.device()
        .write_multiple(
            register::reg::VSET,
            &[
                register::voltage_to_raw(original_sp.voltage),
                register::current_to_raw(original_sp.current),
            ],
        )
        .await
        .unwrap();

    assert_eq!(
        reported,
        Protection { ovp: 3.0, ocp: 1.0 },
        "protection() after recall"
    );
    assert!(tripped_m9, "M9's 3 V OVP should be active after recall");
    assert_eq!(
        reported_after_set,
        Protection {
            ovp: 10.0,
            ocp: 1.0
        }
    );
    assert!(
        !tripped_after_set,
        "set_protection must change the active OVP"
    );
    c.shutdown().await;
}
