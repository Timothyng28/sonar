use serde_json::Value;
use std::fs;
use tempfile::TempDir;

/// Smoke test the install logic via the public CLI binary, sandboxed in a
/// temp $HOME so we don't touch the real ~/.claude/. The point isn't to
/// exercise every code path — just to assert the happy paths: writes
/// happen, backups happen, idempotency works, uninstall restores.
fn run_sonar(home: &std::path::Path, args: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_sonar");
    std::process::Command::new(bin)
        .args(args)
        .env("HOME", home)
        .output()
        .expect("running sonar binary")
}

#[test]
fn install_creates_hook_and_mcp_with_backups() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    fs::create_dir_all(home.join(".claude")).unwrap();

    // seed an existing settings.json with one Stop hook, mimicking the
    // user's real config so we can verify we don't trample it.
    let existing = serde_json::json!({
        "hooks": {
            "Stop": [
                { "hooks": [{ "type": "command", "command": "afplay foo.aiff" }] }
            ]
        }
    });
    fs::write(
        home.join(".claude/settings.json"),
        serde_json::to_string_pretty(&existing).unwrap(),
    )
    .unwrap();

    // seed a near-empty ~/.claude.json with mcpServers=null
    fs::write(home.join(".claude.json"), r#"{"mcpServers": null}"#).unwrap();

    let out = run_sonar(home, &["install"]);
    assert!(out.status.success(), "install failed: {:?}", out);

    // settings.json: SessionEnd added, Stop preserved
    let settings: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".claude/settings.json")).unwrap())
            .unwrap();
    let session_end = settings.pointer("/hooks/SessionEnd").unwrap();
    assert!(session_end.is_array());
    assert_eq!(session_end.as_array().unwrap().len(), 1);
    let stop = settings.pointer("/hooks/Stop").unwrap();
    assert!(stop.is_array());
    assert!(stop.as_array().unwrap()[0]
        .to_string()
        .contains("afplay foo.aiff"));

    // backup landed
    assert!(home.join(".claude/settings.json.pre-sonar").exists());

    // global mcp.json: mcpServers.sonar added
    let mcp: Value = serde_json::from_str(&fs::read_to_string(home.join(".claude.json")).unwrap())
        .unwrap();
    assert_eq!(
        mcp.pointer("/mcpServers/sonar/args").unwrap(),
        &serde_json::json!(["mcp"])
    );
    assert!(home.join(".claude.json.pre-sonar").exists());
}

#[test]
fn install_is_idempotent() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    fs::write(home.join(".claude/settings.json"), "{}").unwrap();
    fs::write(home.join(".claude.json"), "{}").unwrap();

    let first = run_sonar(home, &["install"]);
    assert!(first.status.success());
    let after_first =
        fs::read_to_string(home.join(".claude/settings.json")).unwrap();

    let second = run_sonar(home, &["install"]);
    assert!(second.status.success());
    let after_second =
        fs::read_to_string(home.join(".claude/settings.json")).unwrap();

    // Same content — second install was a no-op.
    assert_eq!(after_first, after_second);
}

#[test]
fn uninstall_restores_from_backup() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let original = r#"{"hooks": {"Stop": []}}"#;
    fs::write(home.join(".claude/settings.json"), original).unwrap();
    fs::write(home.join(".claude.json"), "{}").unwrap();

    let install = run_sonar(home, &["install"]);
    assert!(install.status.success());
    let modified = fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert!(modified.contains("SessionEnd"));

    let uninstall = run_sonar(home, &["uninstall"]);
    assert!(uninstall.status.success());
    let restored = fs::read_to_string(home.join(".claude/settings.json")).unwrap();
    assert_eq!(restored, original);
}

#[test]
fn dry_run_writes_nothing() {
    let tmp = TempDir::new().unwrap();
    let home = tmp.path();
    fs::create_dir_all(home.join(".claude")).unwrap();
    let original = "{}";
    fs::write(home.join(".claude/settings.json"), original).unwrap();
    fs::write(home.join(".claude.json"), original).unwrap();

    let out = run_sonar(home, &["install", "--dry-run"]);
    assert!(out.status.success());
    assert_eq!(
        fs::read_to_string(home.join(".claude/settings.json")).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(home.join(".claude.json")).unwrap(),
        original
    );
    assert!(!home.join(".claude/settings.json.pre-sonar").exists());
}
