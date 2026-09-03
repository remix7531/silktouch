//! Flag/registry parity and registry resolution order: the registry is
//! shorthand for flags that already exist, never the only route to a
//! capability, and its search order is documented in the README's
//! "Configuration" section.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use tempfile::TempDir;

fn write_registry(dir: &Path, toml_text: &str) -> std::path::PathBuf {
    let path = dir.join("silktouch").join("registry.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, toml_text).unwrap();
    path
}

// ---- 12. --merge parity with an equivalent registry [sets.NAME.merge]. --

#[test]
fn merge_flag_matches_equivalent_registry_entry_byte_for_byte() {
    let frags = fragments_dir(&[
        ("10-base.json", r#"{"list":["a","b"]}"#),
        ("20-more.json", r#"{"list":["c"]}"#),
    ]);

    let root = TempDir::new().expect("tempdir");
    let out_flag = root.path().join("out-flag.json");
    let out_registry = root.path().join("out-registry.json");

    // Side A: the flag.
    bin()
        .args([
            "combine",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            out_flag.to_str().unwrap(),
            "--merge",
            "/list=replace",
        ])
        .assert()
        .success();

    // Side B: an equivalent registry entry, driven with no --merge flag at
    // all.
    let xdg_root = TempDir::new().expect("tempdir");
    let registry_toml = format!(
        "[sets.test]\nfragments = {:?}\noutput = {:?}\n\n[sets.test.merge]\n\"/list\" = \"replace\"\n",
        frags.path().to_str().unwrap(),
        out_registry.to_str().unwrap(),
    );
    write_registry(&xdg_root.path().join("xdg-config"), &registry_toml);

    let mut cmd = bin();
    isolate_xdg(&mut cmd, xdg_root.path());
    cmd.args(["combine", "--set", "test"]).assert().success();

    let a = read(&out_flag);
    let b = read(&out_registry);
    assert_eq!(
        a, b,
        "--merge and an equivalent registry [sets.NAME.merge] entry must produce byte-identical output"
    );
    // Guard against the test going vacuous: `replace` must actually differ
    // from the concat-dedupe default here.
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&a).unwrap()["list"],
        serde_json::json!(["c"]),
        "replace should have made 20-more.json's list win outright"
    );
}

// ---- 13a. --set resolves via XDG_CONFIG_HOME. -----------------------------

#[test]
fn set_resolves_via_xdg_config_home() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("out").join("settings.json");

    let registry_toml = format!(
        "[sets.test]\nfragments = {:?}\noutput = {:?}\n",
        frags.path().to_str().unwrap(),
        output.to_str().unwrap(),
    );
    write_registry(&root.path().join("xdg-config"), &registry_toml);

    let mut cmd = bin();
    let xdg = isolate_xdg(&mut cmd, root.path());
    cmd.args(["combine", "--set", "test"]).assert().success();

    assert!(
        output.exists(),
        "the set's output file must have been written"
    );
    let value: serde_json::Value = serde_json::from_slice(&read(&output)).unwrap();
    assert_eq!(value, serde_json::json!({"a": 1}));

    // 14. A registry driven run must never create state/data/cache dirs.
    xdg.assert_state_data_cache_absent();
}

// ---- 13b. --config PATH beats discovery entirely. -------------------------

#[test]
fn explicit_config_flag_beats_discovered_registry() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output_via_xdg = root.path().join("via-xdg.json");
    let output_via_explicit = root.path().join("via-explicit.json");

    // A discoverable registry via XDG_CONFIG_HOME...
    let xdg_registry_toml = format!(
        "[sets.test]\nfragments = {:?}\noutput = {:?}\n",
        frags.path().to_str().unwrap(),
        output_via_xdg.to_str().unwrap(),
    );
    write_registry(&root.path().join("xdg-config"), &xdg_registry_toml);

    // ...and an explicit one naming a different output entirely.
    let explicit_dir = root.path().join("explicit");
    fs::create_dir_all(&explicit_dir).unwrap();
    let explicit_path = explicit_dir.join("other-registry.toml");
    let explicit_toml = format!(
        "[sets.test]\nfragments = {:?}\noutput = {:?}\n",
        frags.path().to_str().unwrap(),
        output_via_explicit.to_str().unwrap(),
    );
    fs::write(&explicit_path, explicit_toml).unwrap();

    let mut cmd = bin();
    isolate_xdg(&mut cmd, root.path());
    cmd.args([
        "combine",
        "--set",
        "test",
        "--config",
        explicit_path.to_str().unwrap(),
    ])
    .assert()
    .success();

    assert!(
        output_via_explicit.exists(),
        "the explicit --config registry must have been used"
    );
    assert!(
        !output_via_xdg.exists(),
        "--config must beat discovery entirely -- the XDG registry must be ignored"
    );
}

// ---- 13c. $XDG_CONFIG_DIRS is searched in order, first match wins. --------

#[test]
fn xdg_config_dirs_searched_in_order_first_match_wins() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    // config_home deliberately has no registry.toml, so discovery falls
    // through to $XDG_CONFIG_DIRS.
    let config_home = root.path().join("xdg-config-empty");
    let dir_a = root.path().join("dir_a");
    let dir_b = root.path().join("dir_b");
    let output_a = root.path().join("output-a.json");
    let output_b = root.path().join("output-b.json");

    write_registry(
        &dir_a,
        &format!(
            "[sets.test]\nfragments = {:?}\noutput = {:?}\n",
            frags.path().to_str().unwrap(),
            output_a.to_str().unwrap(),
        ),
    );
    write_registry(
        &dir_b,
        &format!(
            "[sets.test]\nfragments = {:?}\noutput = {:?}\n",
            frags.path().to_str().unwrap(),
            output_b.to_str().unwrap(),
        ),
    );

    let config_dirs = std::env::join_paths([&dir_a, &dir_b]).unwrap();
    bin()
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CONFIG_DIRS", &config_dirs)
        .env("XDG_STATE_HOME", root.path().join("xdg-state"))
        .env("XDG_DATA_HOME", root.path().join("xdg-data"))
        .env("XDG_CACHE_HOME", root.path().join("xdg-cache"))
        .args(["combine", "--set", "test"])
        .assert()
        .success();

    assert!(output_a.exists(), "dir_a is first in the list and must win");
    assert!(
        !output_b.exists(),
        "dir_b must not be consulted once dir_a matched"
    );

    // Reversed order picks the other one: order, not content, decides.
    fs::remove_file(&output_a).unwrap();
    let config_dirs_reversed = std::env::join_paths([&dir_b, &dir_a]).unwrap();
    bin()
        .env("XDG_CONFIG_HOME", &config_home)
        .env("XDG_CONFIG_DIRS", &config_dirs_reversed)
        .env("XDG_STATE_HOME", root.path().join("xdg-state"))
        .env("XDG_DATA_HOME", root.path().join("xdg-data"))
        .env("XDG_CACHE_HOME", root.path().join("xdg-cache"))
        .args(["combine", "--set", "test"])
        .assert()
        .success();

    assert!(output_b.exists(), "dir_b is now first and must win");
    assert!(!output_a.exists(), "dir_a must not be consulted this time");
}

// ---- 15. --set together with --fragments is rejected (clap conflict). ----

#[test]
fn set_and_fragments_together_is_rejected() {
    let root = TempDir::new().expect("tempdir");
    let frags = root.path().join("frags");
    fs::create_dir_all(&frags).unwrap();
    let output = root.path().join("output.json");

    let assert = bin()
        .args([
            "combine",
            "--set",
            "whatever",
            "--fragments",
            frags.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("--set") || stderr.contains("set"),
        "clap's conflict message should mention --set: {stderr:?}"
    );
}
