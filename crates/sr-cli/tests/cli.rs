use assert_cmd::Command;
use predicates::prelude::*;
use std::io::Write;

fn cli() -> Command {
    Command::cargo_bin("sr-cli").unwrap()
}

fn file(content: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().unwrap();
    f.write_all(content.as_bytes()).unwrap();
    f
}

/// The CLI with a scratch HOME, so a SoundRouter.app running on this machine cannot hold its locks.
fn isolated() -> (Command, tempfile::TempDir) {
    let home = tempfile::tempdir().unwrap();
    let mut c = cli();
    c.env("HOME", home.path());
    (c, home)
}

fn driver_installed() -> bool {
    cli().arg("doctor").output().unwrap().status.success()
}

const GOOD_DEV: &str = r#"{"id":"v1","name":"V","enabled":true,"channels":2,"volume_db":0.0,
 "muted":false,"sources":[],"wires":[],"monitors":[]}"#;

#[test]
fn help_exits_0() {
    cli()
        .arg("--help")
        .assert()
        .code(0)
        .stdout(predicate::str::contains("doctor"));
}

#[test]
fn unknown_subcommand_exits_2() {
    cli().arg("frobnicate").assert().code(2);
}

#[test]
fn missing_arg_exits_2() {
    cli()
        .arg("run")
        .assert()
        .code(2)
        .stderr(predicate::str::contains("CONFIG"));
    cli().arg("meter").assert().code(2);
}

#[test]
fn run_missing_config_file_exits_1() {
    let (mut c, _home) = isolated();
    c.args(["run", "/nonexistent/sr-config.json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "reading /nonexistent/sr-config.json",
        ));
}

#[test]
fn run_bad_json_exits_1() {
    let f = file("{not json");
    let (mut c, _home) = isolated();
    c.arg("run")
        .arg(f.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parsing config"));
}

#[test]
fn run_invalid_config_surfaces_validate_error() {
    let f = file(&format!(
        r#"{{"version":1,"devices":[{},{}]}}"#,
        GOOD_DEV, GOOD_DEV
    ));
    let (mut c, _home) = isolated();
    c.arg("run")
        .arg(f.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains(
            "invalid config: duplicate device id 'v1'",
        ));
}

#[test]
fn driver_set_bad_json_exits_1() {
    let f = file("[{\"uid\":");
    let (mut c, _home) = isolated();
    c.args(["driver", "set"])
        .arg(f.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("parsing device list"));
}

#[test]
fn driver_set_missing_file_exits_1() {
    let (mut c, _home) = isolated();
    c.args(["driver", "set", "/nonexistent/devs.json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("reading /nonexistent/devs.json"));
}

#[test]
fn driver_read_ahead_rejects_an_unknown_mode_and_lists_the_modes() {
    cli()
        .args(["driver", "read-ahead", "maybe"])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("possible values: on, off"));
}

/// Without the driver: a clear "not installed" error. With it: one `read-ahead:` line (an old driver says
/// "unsupported"; the test never switches anything).
#[test]
fn driver_read_ahead_reads_one_line_or_says_not_installed() {
    let out = cli().args(["driver", "read-ahead"]).output().unwrap();
    if driver_installed() {
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{out:?}");
        assert!(stdout.starts_with("read-ahead: "), "{stdout}");
        assert_eq!(stdout.lines().count(), 1, "{stdout}");
    } else {
        assert_eq!(out.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&out.stderr).contains("driver not installed"));
    }
}

#[test]
fn driver_read_ahead_switch_refuses_while_engine_lock_is_held() {
    let (home, _held) = home_with_lock("engine.lock");
    for mode in ["on", "off"] {
        cli()
            .env("HOME", home.path())
            .args(["driver", "read-ahead", mode])
            .assert()
            .code(1)
            .stderr(predicate::str::contains("owns the audio engine"));
    }
}

#[test]
fn meter_unknown_device_is_clear_error() {
    cli()
        .args(["meter", "zz-nope"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("cannot meter 'zz-nope'"))
        .stderr(predicate::str::contains("no device with uid"));
}

#[test]
fn doctor_exit_code_matches_driver_presence() {
    let out = cli().arg("doctor").output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    if stdout.contains("NOT FOUND") {
        assert_eq!(out.status.code(), Some(1));
        assert!(stdout.contains("driver-dev.sh install") && stdout.contains(".pkg"));
    } else {
        assert!(stdout.contains("driver plug-in:  found"));
    }
}

#[test]
fn run_without_driver_says_not_installed() {
    if driver_installed() {
        return; // driver present: this scenario cannot occur on this machine
    }
    let f = file(&format!(r#"{{"version":1,"devices":[{GOOD_DEV}]}}"#));
    let (mut c, _home) = isolated();
    c.arg("run")
        .arg(f.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("driver not installed"));
}

/// Needs the driver installed, BlackHole16ch, and SR_MAIN_UID_OVERRIDE support in sr-audio's
/// engine. Set BLACKHOLE_UID to BlackHole16ch's uid from `sr-cli list`.
#[test]
#[ignore = "needs driver + BlackHole16ch + engine SR_MAIN_UID_OVERRIDE hook"]
fn run_exits_0_on_sigint() {
    let uid = std::env::var("BLACKHOLE_UID").expect("set BLACKHOLE_UID");
    let cfg = format!(
        r#"{{"version":1,"devices":[{{"id":"v1","name":"V","enabled":true,"channels":2,"volume_db":0.0,
 "muted":false,"sources":[{{"id":"s","kind":{{"type":"device","uid":"{uid}"}},"channels":2,"volume_db":0.0,"muted":false}}],
 "wires":[{{"source":"s","src_ch":0,"out_ch":0}}],"monitors":[]}}]}}"#
    );
    let f = file(&cfg);
    let mut child = std::process::Command::new(assert_cmd::cargo::cargo_bin("sr-cli"))
        .arg("run")
        .arg(f.path())
        .env("SR_MAIN_UID_OVERRIDE", &uid)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // Wait for the "running" banner so the SIGINT handler is installed.
    let mut line = String::new();
    let mut r = std::io::BufReader::new(child.stdout.take().unwrap());
    std::io::BufRead::read_line(&mut r, &mut line).unwrap();
    assert!(line.starts_with("running"), "got: {line}");
    let st = std::process::Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(st.success());
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

/// HOME is where both lock files live; point the CLI at a scratch one so no real app is involved.
fn home_with_lock(name: &str) -> (tempfile::TempDir, std::fs::File) {
    let home = tempfile::tempdir().unwrap();
    let p = home
        .path()
        .join("Library/Application Support/SoundRouter")
        .join(name);
    let held = sr_core::lock::try_lock(&p)
        .unwrap()
        .expect("test takes the lock");
    (home, held)
}

#[test]
fn run_refuses_while_the_app_holds_its_locks() {
    let cfg = file(r#"{"version":1,"devices":[]}"#);
    for name in ["instance.lock", "engine.lock"] {
        let (home, _held) = home_with_lock(name);
        cli()
            .env("HOME", home.path())
            .arg("run")
            .arg(cfg.path())
            .assert()
            .code(1)
            .stderr(predicate::str::contains("owns the audio engine"))
            .stderr(predicate::str::contains("--force"));
    }
}

#[test]
fn driver_set_refuses_while_engine_lock_is_held() {
    let list = file("[]");
    let (home, _held) = home_with_lock("engine.lock");
    cli()
        .env("HOME", home.path())
        .args(["driver", "set"])
        .arg(list.path())
        .assert()
        .code(1)
        .stderr(predicate::str::contains("owns the audio engine"));
}

#[test]
fn force_bypasses_the_lock_check() {
    // The missing config file stops the run before any engine exists, so this is safe to run
    // anywhere: reaching "reading" instead of the refusal proves --force skipped the held lock.
    for args in [
        &["run", "--force", "/nonexistent/sr-config.json"][..],
        &["driver", "set", "--force", "/nonexistent/devs.json"][..],
    ] {
        let (home, _held) = home_with_lock("instance.lock");
        cli()
            .env("HOME", home.path())
            .args(args)
            .assert()
            .code(1)
            .stderr(predicate::str::contains("reading /nonexistent"));
    }
}

// --- presets ---------------------------------------------------------------------------------

fn support(home: &std::path::Path) -> std::path::PathBuf {
    let d = home.join("Library/Application Support/SoundRouter");
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A scratch HOME whose config has one device `id` at `volume_db`.
fn home_with_config(id: &str, volume_db: f32) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &[(id, volume_db)]);
    home
}

fn write_config(home: &std::path::Path, devs: &[(&str, f32)]) {
    let devs: Vec<String> = devs
        .iter()
        .map(|(id, db)| {
            format!(
                r#"{{"id":"{id}","name":"Dev {id}","enabled":true,"channels":2,"volume_db":{db:?},"muted":false,"sources":[],"wires":[],"monitors":[]}}"#
            )
        })
        .collect();
    std::fs::write(
        support(home).join("config.json"),
        format!(r#"{{"version":1,"devices":[{}]}}"#, devs.join(",")),
    )
    .unwrap();
}

fn config_volumes(home: &std::path::Path) -> Vec<f64> {
    let v: serde_json::Value =
        serde_json::from_slice(&std::fs::read(support(home).join("config.json")).unwrap()).unwrap();
    v["devices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["volume_db"].as_f64().unwrap())
        .collect()
}

fn preset(home: &std::path::Path, args: &[&str]) -> assert_cmd::assert::Assert {
    cli().env("HOME", home).arg("preset").args(args).assert()
}

#[test]
fn preset_list_starts_empty_then_shows_saved_presets() {
    let home = home_with_config("v1", 0.0);
    preset(home.path(), &["list"])
        .code(0)
        .stdout(predicate::str::contains("no presets"));
    preset(home.path(), &["save", "Streaming"])
        .code(0)
        .stdout(predicate::str::contains("saved preset \"Streaming\""));
    preset(home.path(), &["save", "streaming"])
        .code(0)
        .stdout(predicate::str::contains("replaced preset"));
    preset(home.path(), &["list"])
        .code(0)
        .stdout(predicate::str::contains("streaming  (1 device)"));
}

#[test]
fn preset_save_needs_a_config_and_a_name() {
    let home = tempfile::tempdir().unwrap();
    preset(home.path(), &["save", "x"])
        .code(1)
        .stderr(predicate::str::contains("config.json"));
    let home = home_with_config("v1", 0.0);
    preset(home.path(), &["save", "  "])
        .code(1)
        .stderr(predicate::str::contains("Give the preset a name"));
}

#[test]
fn preset_apply_unknown_name_exits_1_and_lists_the_known_ones() {
    let home = home_with_config("v1", 0.0);
    preset(home.path(), &["apply", "nope"])
        .code(1)
        .stderr(predicate::str::contains("no preset named \"nope\""));
    preset(home.path(), &["save", "Mine"]).code(0);
    preset(home.path(), &["apply", "nope"])
        .code(1)
        .stderr(predicate::str::contains("(have: Mine)"));
}

#[test]
fn preset_apply_restores_the_saved_routing_in_the_config() {
    let home = home_with_config("v1", -6.0);
    preset(home.path(), &["save", "Quiet"]).code(0);
    write_config(home.path(), &[("v1", 0.0)]);
    preset(home.path(), &["apply", "quiet"])
        .code(0)
        .stdout(predicate::str::contains("applied \"Quiet\""));
    assert_eq!(config_volumes(home.path()), [-6.0]);
}

#[test]
fn preset_apply_that_adds_a_device_needs_the_flag() {
    let home = tempfile::tempdir().unwrap();
    write_config(home.path(), &[("v1", 0.0), ("v2", -3.0)]);
    preset(home.path(), &["save", "Two"]).code(0);
    write_config(home.path(), &[("v1", 0.0)]);
    preset(home.path(), &["apply", "Two"])
        .code(1)
        .stderr(predicate::str::contains("Add Dev v2"))
        .stderr(predicate::str::contains("--add-devices"));
    assert_eq!(config_volumes(home.path()), [0.0], "config untouched");
    preset(home.path(), &["apply", "Two", "--add-devices"]).code(0);
    assert_eq!(config_volumes(home.path()), [0.0, -3.0]);
}

#[test]
fn a_corrupt_presets_file_warns_and_is_backed_up() {
    let home = home_with_config("v1", 0.0);
    let p = support(home.path()).join("presets.json");
    std::fs::write(&p, "{nope").unwrap();
    preset(home.path(), &["list"])
        .code(0)
        .stderr(predicate::str::contains("presets.json.bad"))
        .stdout(predicate::str::contains("no presets"));
    assert!(!p.exists() && support(home.path()).join("presets.json.bad").exists());
}

#[test]
fn preset_apply_hands_over_to_the_running_app_instead_of_editing_its_config() {
    let home = home_with_config("v1", 0.0);
    preset(home.path(), &["save", "Café 1"]).code(0);
    let _app = sr_core::lock::try_lock(&support(home.path()).join("instance.lock"))
        .unwrap()
        .unwrap();
    let log = home.path().join("opened");
    let opener = home.path().join("open.sh");
    std::fs::write(
        &opener,
        format!("#!/bin/sh\necho \"$1\" > '{}'\n", log.display()),
    )
    .unwrap();
    std::fs::set_permissions(&opener, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    write_config(home.path(), &[("v1", -1.0)]);
    cli()
        .env("HOME", home.path())
        .env("SR_OPEN", &opener)
        .args(["preset", "apply", "café 1"])
        .assert()
        .code(0)
        .stdout(predicate::str::contains("asked SoundRouter"));
    assert_eq!(
        std::fs::read_to_string(log).unwrap().trim(),
        "soundrouter://preset/Caf%C3%A9%201"
    );
    assert_eq!(
        config_volumes(home.path()),
        [-1.0],
        "the app owns the config"
    );
}

/// `sr-cli preset list | head -0`: the reader is gone before the first write. Rust ignores SIGPIPE,
/// so without the default restored `println!` panicked (exit 101 and a "panicked at" report).
#[cfg(unix)]
#[test]
fn a_closed_stdout_ends_the_process_quietly_not_with_a_panic() {
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(cli().get_program())
        .args(["preset", "list"])
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert_ne!(out.status.code(), Some(101));
}

#[test]
fn concurrent_preset_saves_all_land() {
    // Each process loads, adds its preset and saves: without the lock a later save drops an earlier one.
    let home = home_with_config("v1", -6.0);
    let bin = cli().get_program().to_owned();
    let kids: Vec<_> = (0..8)
        .map(|i| {
            std::process::Command::new(&bin)
                .env("HOME", home.path())
                .args(["preset", "save", &format!("P{i}")])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    for k in kids {
        let out = k.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let list = cli()
        .env("HOME", home.path())
        .args(["preset", "list"])
        .output()
        .unwrap();
    let list = String::from_utf8_lossy(&list.stdout);
    for i in 0..8 {
        assert!(list.contains(&format!("P{i}  ")), "P{i} lost:\n{list}");
    }
}

#[test]
fn preset_apply_never_overwrites_a_config_from_a_newer_version() {
    let home = home_with_config("v1", -6.0);
    preset(home.path(), &["save", "Quiet"]).code(0);
    let path = support(home.path()).join("config.json");
    let newer = r#"{"version":3,"devices":[],"future":{"x":1}}"#;
    std::fs::write(&path, newer).unwrap();
    preset(home.path(), &["apply", "Quiet", "--add-devices"])
        .code(1)
        .stderr(predicate::str::contains("version 3"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
}

#[test]
fn preset_names_with_unicode_and_quotes_round_trip() {
    let home = home_with_config("v1", -6.0);
    let name = "Stream 🎙 \"live\" — café";
    preset(home.path(), &["save", name]).code(0);
    preset(home.path(), &["list"])
        .code(0)
        .stdout(predicate::str::contains(name));
    write_config(home.path(), &[("v1", 0.0)]);
    preset(home.path(), &["apply", name]).code(0);
    assert_eq!(config_volumes(home.path()), [-6.0]);
}
