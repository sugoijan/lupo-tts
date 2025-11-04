use assert_cmd::Command;
use serde_json::Value;
use tempfile::TempDir;

fn cli() -> Command {
    Command::from_std(std::process::Command::new(assert_cmd::cargo::cargo_bin!(
        "lupo-cli"
    )))
}

#[test]
fn settings_subcommand_list_defaults_and_manage_overrides() {
    let temp = TempDir::new().expect("temp dir");
    let data_dir = temp.path().join("data");
    let data_dir_str = data_dir.to_str().expect("utf8 path");

    let output = cli()
        .args([
            "settings",
            "--data-dir",
            data_dir_str,
            "list",
            "--format",
            "json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let entries: Value = serde_json::from_slice(&output).expect("valid json");
    assert!(entries.is_array());
    let keys: Vec<_> = entries
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry.get("key"))
        .filter_map(|key| key.as_str())
        .collect();
    assert!(keys.contains(&"text_transformation"));

    // Set an override
    cli()
        .args([
            "settings",
            "--data-dir",
            data_dir_str,
            "set",
            "text_transformation",
            r#"{"template":"Test","strip_emotes":false,"normalize_whitespace":false}"#,
        ])
        .assert()
        .success();

    // Fetch it back
    let get_output = cli()
        .args([
            "settings",
            "--data-dir",
            data_dir_str,
            "get",
            "text_transformation",
            "--value-only",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value =
        serde_json::from_slice(&get_output).expect("value-only output is valid json");
    assert_eq!(value["template"], "Test");
    assert_eq!(value["strip_emotes"], false);

    // Reset and confirm it returns to defaults.
    cli()
        .args([
            "settings",
            "--data-dir",
            data_dir_str,
            "reset",
            "text_transformation",
        ])
        .assert()
        .success();

    let defaults = cli()
        .args([
            "settings",
            "--data-dir",
            data_dir_str,
            "get",
            "text_transformation",
            "--value-only",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let value: Value = serde_json::from_slice(&defaults).expect("value-only output");
    assert_eq!(value["template"], "{user} said {message}");
}
