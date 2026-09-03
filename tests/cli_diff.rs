//! Exit codes and `diff` output: see the README's "The four commands"
//! section. `diff` follows diff(1) (0 same, 1 different, 2 error) and
//! every error must name the offending path.

mod common;

use std::fs;

use common::*;
use tempfile::TempDir;

fn combine_ok(fragments: &std::path::Path, output: &std::path::Path) {
    bin()
        .args([
            "combine",
            "--fragments",
            fragments.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();
}

fn diff_cmd(fragments: &std::path::Path, output: &std::path::Path) -> assert_cmd::Command {
    let mut cmd = bin();
    cmd.args([
        "diff",
        "--fragments",
        fragments.to_str().unwrap(),
        "-o",
        output.to_str().unwrap(),
    ]);
    cmd
}

// ---- 1. diff exits 0 on a synchronised set, twice in a row --------------
//
// A raw JSON diff would report array reordering forever on a set that
// spreads a concat-dedupe array across fragments. `diff`'s exit code must
// come from `delta::classify`, not a naive structural comparison, so this is
// run twice to catch a regression back to that.

#[test]
fn diff_exits_0_on_synchronised_set_run_twice() {
    let frags = fragments_dir(&[
        ("10-base.json", r#"{"a":1,"list":["x","y"]}"#),
        ("20-more.json", r#"{"list":["y","z"],"b":2}"#),
    ]);
    let out_dir = TempDir::new().expect("tempdir");
    let output = out_dir.path().join("output.json");

    combine_ok(frags.path(), &output);

    for _ in 0..2 {
        diff_cmd(frags.path(), &output)
            .assert()
            .code(0)
            .stdout(predicates::str::is_empty());
    }
}

// ---- 2. diff exits 1 after an external edit, naming the pointer and the
// fragment the change would land in. ---------------------------------------

#[test]
fn diff_exits_1_and_names_pointer_and_destination_after_external_edit() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let out_dir = TempDir::new().expect("tempdir");
    let output = out_dir.path().join("output.json");

    combine_ok(frags.path(), &output);
    fs::write(&output, r#"{"a":2}"#).expect("external edit");

    let assert = diff_cmd(frags.path(), &output).assert().code(1);
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();

    assert!(
        stdout.contains("/a"),
        "stdout should name the pointer /a: {stdout:?}"
    );
    assert!(
        stdout.contains("10-base.json"),
        "stdout should name the destination fragment 10-base.json: {stdout:?}"
    );
    assert!(
        stdout.contains('2'),
        "stdout should show the new value: {stdout:?}"
    );
}

// ---- 3. diff exits 2 on every listed error case, each message naming the
// offending path. ------------------------------------------------------

#[test]
fn diff_exits_2_missing_fragment_directory() {
    let root = TempDir::new().expect("tempdir");
    let missing = root.path().join("nope");
    let output = root.path().join("output.json");
    fs::write(&output, "{}").unwrap();

    let assert = diff_cmd(&missing, &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(missing.to_str().unwrap()),
        "stderr should name the missing fragment dir: {stderr:?}"
    );
}

#[test]
fn diff_exits_2_missing_output_file() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("does-not-exist.json");

    let assert = diff_cmd(frags.path(), &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(output.to_str().unwrap()),
        "stderr should name the missing output file: {stderr:?}"
    );
}

#[test]
fn diff_exits_2_malformed_json_fragment() {
    let frags = fragments_dir(&[("10-base.json", "{ not json")]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");
    fs::write(&output, "{}").unwrap();

    let bad = frags.path().join("10-base.json");
    let assert = diff_cmd(frags.path(), &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(bad.to_str().unwrap()),
        "stderr should name the malformed fragment: {stderr:?}"
    );
}

#[test]
fn diff_exits_2_fragment_root_not_object() {
    let frags = fragments_dir(&[("10-base.json", "[1,2,3]")]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");
    fs::write(&output, "{}").unwrap();

    let bad = frags.path().join("10-base.json");
    let assert = diff_cmd(frags.path(), &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(bad.to_str().unwrap()),
        "stderr should name the offending fragment: {stderr:?}"
    );
}

#[test]
fn diff_exits_2_catch_all_does_not_sort_last() {
    // Default catch all is 99-local.json. zz-late.json sorts after it.
    let frags = fragments_dir(&[
        ("10-base.json", r#"{"a":1}"#),
        ("zz-late.json", r#"{"b":2}"#),
    ]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");
    fs::write(&output, "{}").unwrap();

    let assert = diff_cmd(frags.path(), &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains("zz-late.json"),
        "stderr should name the offending fragment zz-late.json: {stderr:?}"
    );
    assert!(
        stderr.contains("99-local.json"),
        "stderr should name the configured catch-all: {stderr:?}"
    );
}

#[test]
fn diff_exits_2_output_root_not_object() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");
    fs::write(&output, "[1,2,3]").unwrap();

    let assert = diff_cmd(frags.path(), &output).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(output.to_str().unwrap()),
        "stderr should name the output file: {stderr:?}"
    );
}
