//! OmO setup uses isolated agent roots and never reads the user's native config.

use std::path::{Path, PathBuf};

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::{Value, json};

fn cli(home: &Path, agent: &Path) -> Command {
    let mut command = Command::cargo_bin("rtrt").unwrap();
    command
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("OMO_CODING_AGENT_DIR", agent)
        .env_remove("SENPI_CODING_AGENT_DIR")
        .env_remove("PI_CODING_AGENT_DIR")
        .env_remove("RTRT_MEMORY_PATH")
        .env("RTRT_CONFIG", home.join("config.toml"));
    command
}

fn setup(home: &Path, agent: &Path) -> Command {
    let mut command = cli(home, agent);
    command.args(["setup", "--agent", "omo", "--binary"]);
    command.arg(assert_cmd::cargo::cargo_bin!("rtrt"));
    command
}

fn config(agent: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(agent.join("mcp.json")).unwrap()).unwrap()
}

fn home() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let canonical = temp.path().canonicalize().unwrap();
    (temp, canonical)
}

#[test]
fn dry_run_resolves_native_targets_without_writing() {
    // Given: a native agent directory that does not yet exist.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    // When: setup is invoked without --apply.
    setup(&home, &agent).assert().success();
    // Then: the entire agent tree remains absent.
    assert!(!agent.exists());
}

#[test]
fn dry_run_does_not_print_other_servers_credentials() {
    // Given: another server with a private credential in the shared config.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    std::fs::create_dir(&agent).unwrap();
    let original = json!({"mcpServers": {"foreign": {
        "command": "other-server", "env": {"PRIVATE_API_KEY": "fixture-secret-only"}
    }}});
    std::fs::write(agent.join("mcp.json"), original.to_string()).unwrap();
    // When: the operator previews the native installation.
    setup(&home, &agent)
        .assert()
        .success()
        .stdout(predicate::str::contains("fixture-secret-only").not());
    // Then: neither the private configuration nor the extension inventory changes.
    assert_eq!(config(&agent), original);
    assert!(!agent.join("extensions").exists());
}

#[test]
fn repeated_install_preserves_foreign_configuration_and_original_backup() {
    // Given: another MCP server, a custom prefix, and an unrelated extension.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    std::fs::create_dir_all(agent.join("extensions/rtrt")).unwrap();
    let original = json!({
        "settings": {"toolPrefix": "native", "searchThreshold": 7},
        "mcpServers": {"foreign": {"command": "other-server", "enabled": false}}
    });
    std::fs::write(agent.join("mcp.json"), original.to_string()).unwrap();
    std::fs::write(agent.join("extensions/rtrt/user.txt"), "foreign").unwrap();
    // When: setup is applied twice.
    setup(&home, &agent).arg("--apply").assert().success();
    let first = std::fs::read(agent.join("mcp.json")).unwrap();
    setup(&home, &agent).arg("--apply").assert().success();
    // Then: configuration converges and original data and backup survive.
    assert_eq!(std::fs::read(agent.join("mcp.json")).unwrap(), first);
    assert_eq!(config(&agent)["settings"], original["settings"]);
    assert_eq!(
        config(&agent)["mcpServers"]["foreign"],
        original["mcpServers"]["foreign"]
    );
    assert_eq!(config(&agent)["mcpServers"]["rtrt"]["args"], json!([]));
    assert!(config(&agent)["mcpServers"]["rtrt"].get("cwd").is_none());
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(agent.join("mcp.json.bak")).unwrap())
            .unwrap(),
        original
    );
    let entry = std::fs::read_to_string(agent.join("extensions/rtrt/index.js")).unwrap();
    let options = entry
        .lines()
        .find_map(|line| line.strip_prefix("const config = "))
        .unwrap();
    let options: Value = serde_json::from_str(options).unwrap();
    assert_eq!(options["toolPrefix"], "native");
    assert!(Path::new(options["binary"].as_str().unwrap()).is_absolute());
}

#[test]
fn uninstall_removes_only_owned_files_and_server() {
    // Given: an installed integration with an unrelated file in the same directory.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    setup(&home, &agent).arg("--apply").assert().success();
    std::fs::write(agent.join("extensions/rtrt/user.txt"), "foreign").unwrap();
    // When: the owned integration is removed.
    cli(&home, &agent)
        .args(["uninstall", "--agent", "omo", "--apply"])
        .assert()
        .success();
    // Then: user content remains while only owned configuration is removed.
    assert!(config(&agent)["mcpServers"].get("rtrt").is_none());
    assert!(!agent.join("extensions/rtrt/index.js").exists());
    assert!(!agent.join("extensions/rtrt/runtime.mjs").exists());
    assert_eq!(
        std::fs::read_to_string(agent.join("extensions/rtrt/user.txt")).unwrap(),
        "foreign"
    );
}

#[test]
fn foreign_rtrt_server_is_refused_without_partial_installation() {
    // Given: a user-owned server using the same name.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    std::fs::create_dir(&agent).unwrap();
    let original = br#"{"mcpServers":{"rtrt":{"command":"foreign"}}}"#;
    std::fs::write(agent.join("mcp.json"), original).unwrap();
    // When: setup encounters the collision.
    setup(&home, &agent).arg("--apply").assert().failure();
    // Then: neither the foreign server nor extension inventory changes.
    assert_eq!(std::fs::read(agent.join("mcp.json")).unwrap(), original);
    assert!(!agent.join("extensions").exists());
}

#[test]
fn modified_runtime_is_preserved_during_setup_and_uninstall() {
    // Given: a user modification to an installed runtime module.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    setup(&home, &agent).arg("--apply").assert().success();
    let original = std::fs::read(agent.join("mcp.json")).unwrap();
    let runtime = agent.join("extensions/rtrt/runtime.mjs");
    std::fs::write(&runtime, "export default function userExtension() {}").unwrap();
    // When: either installer action would replace or delete that runtime.
    setup(&home, &agent).arg("--apply").assert().failure();
    cli(&home, &agent)
        .args(["uninstall", "--agent", "omo", "--apply"])
        .assert()
        .failure();
    // Then: both the modified source and current MCP configuration survive.
    assert_eq!(
        std::fs::read_to_string(runtime).unwrap(),
        "export default function userExtension() {}"
    );
    assert_eq!(std::fs::read(agent.join("mcp.json")).unwrap(), original);
}

#[test]
fn relative_agent_directory_and_binary_are_refused() {
    // Given: overrides that depend on ambient cwd.
    let (_temp, home) = home();
    let agent = home.join("native-agent");
    // When: either override is relative.
    setup(&home, &agent)
        .env("OMO_CODING_AGENT_DIR", "relative")
        .assert()
        .failure();
    cli(&home, &agent)
        .args(["setup", "--agent", "omo", "--binary", "relative"])
        .assert()
        .failure();
    // Then: no configuration is written.
    assert!(!agent.exists());
}

#[cfg(unix)]
#[test]
fn symlinked_destinations_are_refused_before_any_writes() {
    use std::os::unix::fs::symlink;
    // Given: each managed sink redirected to a foreign file or directory.
    for relative in ["mcp.json", "mcp.json.bak", "extensions"] {
        let (_temp, home) = home();
        let agent = home.join("native-agent");
        std::fs::create_dir(&agent).unwrap();
        let target = home.join("foreign");
        if relative == "extensions" {
            std::fs::create_dir(&target).unwrap();
        } else {
            std::fs::write(&target, "{}").unwrap();
        }
        symlink(&target, agent.join(relative)).unwrap();
        // When: native setup preflights its destinations.
        setup(&home, &agent).arg("--apply").assert().failure();
        // Then: no extension or configuration can be written through the link.
        assert!(
            std::fs::symlink_metadata(agent.join(relative))
                .unwrap()
                .file_type()
                .is_symlink()
        );
        if relative == "extensions" {
            assert!(std::fs::read_dir(target).unwrap().next().is_none());
        } else {
            assert_eq!(std::fs::read_to_string(target).unwrap(), "{}");
        }
    }
}
