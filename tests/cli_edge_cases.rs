//! The dangerous edge cases: the ones where a bug would mean data loss, not
//! just a wrong answer. Test 9 in particular (`split` with a missing
//! output file must never be mistaken for "delete everything") is the
//! single most important test in the whole wave.

mod common;

use std::fs;

use common::*;
use tempfile::TempDir;

// ---- 9. split with a missing output file exits 2 and leaves every
// fragment byte identical. THE "not delete everything" test. --------------

#[test]
fn split_missing_output_leaves_every_fragment_byte_identical() {
    let frags = fragments_dir(&[
        ("10-base.json", "{\n  \"a\": 1\n}\n"),
        ("20-more.json", "{\n  \"b\": {\n    \"c\": 2\n  }\n}\n"),
        ("99-local.json", "{\n  \"local\": true\n}\n"),
    ]);
    let names = ["10-base.json", "20-more.json", "99-local.json"];
    let before: Vec<(std::path::PathBuf, Vec<u8>)> = names
        .iter()
        .map(|n| {
            let p = frags.path().join(n);
            (p.clone(), read(&p))
        })
        .collect();
    let listing_before = listing(frags.path());

    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("does-not-exist.json");

    let assert = bin()
        .args([
            "split",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(output.to_str().unwrap()),
        "stderr must name the missing output file: {stderr:?}"
    );

    // The heart of it: every fragment's bytes, not just its mtime.
    for (path, bytes_before) in &before {
        let bytes_after = read(path);
        assert_eq!(
            bytes_before, &bytes_after,
            "fragment {path:?} must be byte-identical after a failed split"
        );
    }
    assert_eq!(
        listing_before,
        listing(frags.path()),
        "split must not create or delete any fragment file on this error"
    );
    assert!(
        !output.exists(),
        "split must not create the output file either"
    );
}

// ---- 10. --allow-empty on an empty fragment directory emits {}. Without it,
// exit 2. --------------------------------------------------------------

#[test]
fn allow_empty_on_empty_directory_emits_empty_object() {
    let dir = TempDir::new().expect("tempdir");

    let assert = bin()
        .args([
            "combine",
            "--fragments",
            dir.path().to_str().unwrap(),
            "-o",
            "-",
            "--allow-empty",
        ])
        .assert()
        .success();
    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("valid JSON");
    assert_eq!(value, serde_json::json!({}));
}

#[test]
fn empty_directory_without_allow_empty_exits_2() {
    let dir = TempDir::new().expect("tempdir");

    let assert = bin()
        .args([
            "combine",
            "--fragments",
            dir.path().to_str().unwrap(),
            "-o",
            "-",
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(dir.path().to_str().unwrap()),
        "stderr must name the empty fragment directory: {stderr:?}"
    );
    assert!(
        stderr.contains("allow-empty") || stderr.contains("allow_empty"),
        "stderr should point at --allow-empty: {stderr:?}"
    );
}

// ---- 11. split on an output file whose root is not an object exits 2,
// naming the file, and changes nothing. ------------------------------------

#[test]
fn split_non_object_output_root_exits_2_and_changes_nothing() {
    let frags = fragments_dir(&[("10-base.json", "{\n  \"a\": 1\n}\n")]);
    let frag_path = frags.path().join("10-base.json");
    let before = read(&frag_path);

    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");
    let bad_output = "[1,2,3]";
    fs::write(&output, bad_output).expect("write non-object output");

    let assert = bin()
        .args([
            "split",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(output.to_str().unwrap()),
        "stderr must name the offending output file: {stderr:?}"
    );

    assert_eq!(read(&frag_path), before, "the fragment must be untouched");
    assert_eq!(
        String::from_utf8(read(&output)).unwrap(),
        bad_output,
        "the output file itself must be untouched"
    );
}
