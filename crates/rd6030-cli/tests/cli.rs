//! CLI integration tests: run the real `rd6030-cli` binary against the
//! behavioural simulator served on a PTY.
#![cfg(unix)]

use std::io::Write;
use std::sync::{Arc, Mutex};

use assert_cmd::Command;
use rd6030_core::register::reg;
use rd6030_sim::SimDevice;
use tokio_serial::SerialPort;

/// Serve a simulator on a fresh PTY; returns its path and the device.
fn start_sim(rt: &tokio::runtime::Runtime) -> (String, Arc<Mutex<SimDevice>>, std::fs::File) {
    let _guard = rt.enter();
    let (master, slave) = tokio_serial::SerialStream::pair().expect("pty");
    let path = slave.name().expect("pty name");
    let keepalive = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap();
    drop(slave);
    let mut dev = SimDevice::new();
    dev.load_ohms = Some(10.0);
    let dev = Arc::new(Mutex::new(dev));
    rt.spawn(rd6030_sim::serve(master, Arc::clone(&dev), 1));
    (path, dev, keepalive)
}

fn cli(port: &str) -> Command {
    let mut cmd = Command::cargo_bin("rd6030-cli").unwrap();
    cmd.env("RUST_BACKTRACE", "0")
        .env_remove("RD6030_CONFIG")
        .env("XDG_CONFIG_HOME", "/nonexistent")
        .current_dir(std::env::temp_dir())
        .args(["--port", port]);
    cmd
}

fn json(cmd: &mut Command) -> serde_json::Value {
    let out = cmd.arg("--json").output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json stdout")
}

#[test]
fn cli_end_to_end_against_simulator() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (port, dev, _keepalive) = start_sim(&rt);

    let info = json(cli(&port).arg("info"));
    assert_eq!(info["model"], 60301);

    let sp = json(cli(&port).args(["set", "--voltage", "12", "--current", "2"]));
    assert_eq!(sp["voltage"], 12.0);
    cli(&port).args(["output", "on"]).assert().success();

    let t = json(cli(&port).arg("telemetry"));
    assert_eq!(t["voltage_out"], 12.0);
    assert_eq!(t["current_out"], 1.2);
    assert_eq!(t["output_enabled"], true);

    // Limits and the register denylist are enforced; nothing is written.
    fails_with(cli(&port).args(["set", "--voltage", "70"]), "out of range");
    fails_with(
        cli(&port).args(["raw", "write", "256", "5633", "--yes"]),
        "not allowed",
    );
    assert_eq!(dev.lock().unwrap().register(reg::VSET), 1200);

    // Preset export -> edit -> import round trip writes only changed slots.
    let bank = json(cli(&port).args(["preset", "export"]));
    let mut edited = bank.clone();
    edited["slots"][7]["voltage"] = serde_json::json!(3.3);
    let mut file = tempfile_in_temp("presets.json");
    file.1.write_all(edited.to_string().as_bytes()).unwrap();
    let res = json(cli(&port).args(["preset", "import", file.0.to_str().unwrap()]));
    assert_eq!(res["written"], serde_json::json!([7]));
    std::fs::remove_file(&file.0).ok();

    // A config with stricter limits is honoured.
    let mut cfg = tempfile_in_temp("cfg.toml");
    cfg.1
        .write_all(b"[limits]\nmax_voltage = 5.0\nmax_current = 1.0\n")
        .unwrap();
    cli(&port)
        .args(["--config", cfg.0.to_str().unwrap(), "set", "--voltage", "6"])
        .assert()
        .failure();
    std::fs::remove_file(&cfg.0).ok();

    // An explicit missing config file is an error.
    cli(&port)
        .args(["--config", "/nonexistent/rd6030.toml", "info"])
        .assert()
        .failure();
}

#[test]
fn unreachable_port_fails_cleanly() {
    fails_with(
        cli("/dev/rd6030-does-not-exist").arg("info"),
        "connect to RD6030",
    );
}

fn tempfile_in_temp(name: &str) -> (std::path::PathBuf, std::fs::File) {
    let path = std::env::temp_dir().join(format!("rd6030-cli-{}-{name}", std::process::id()));
    let file = std::fs::File::create(&path).unwrap();
    (path, file)
}

fn fails_with(cmd: &mut Command, needle: &str) {
    let out = cmd.output().unwrap();
    assert!(!out.status.success(), "command unexpectedly succeeded");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains(needle), "stderr lacks {needle:?}: {stderr}");
}
