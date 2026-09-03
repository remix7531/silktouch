//! Filter mode: `-o -` writes to stdout and touches no files at all: see
//! the README's "The four commands" section: the engine must be drivable
//! to stdout with no filesystem writes.

mod common;

use common::*;

// ---- 4. combine --fragments d/ -o - writes to stdout, exits 0, and creates
// no files: assert both the directory listing and every mtime. -------------

#[test]
fn combine_stdout_creates_no_files_and_mtimes_are_unchanged() {
    let frags = fragments_dir(&[
        ("10-base.json", r#"{"a":1,"nested":{"x":1}}"#),
        ("20-override.json", r#"{"a":2,"list":["p","q"]}"#),
    ]);
    for name in ["10-base.json", "20-override.json"] {
        pin_old_mtime(&frags.path().join(name));
    }

    let listing_before = listing(frags.path());
    let mtimes_before = mtimes(frags.path());

    let assert = bin()
        .args([
            "combine",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            "-",
        ])
        .assert()
        .success();

    let stdout = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is valid JSON");
    assert_eq!(
        value,
        serde_json::json!({"a": 2, "nested": {"x": 1}, "list": ["p", "q"]})
    );

    let listing_after = listing(frags.path());
    assert_eq!(
        listing_before, listing_after,
        "combine -o - must create no files"
    );
    let mtimes_after = mtimes(frags.path());
    assert_eq!(
        mtimes_before, mtimes_after,
        "combine -o - must not touch any fragment's mtime"
    );
}

// ---- 5. stdout output is valid JSON and matches a golden file byte for byte.

#[test]
fn combine_stdout_matches_golden_file_byte_for_byte() {
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/golden_combine");
    let expected = std::fs::read(fixture.join("expected.json")).expect("read golden file");

    let assert = bin()
        .args([
            "combine",
            "--fragments",
            fixture.to_str().unwrap(),
            "-o",
            "-",
        ])
        .assert()
        .success();

    let stdout = assert.get_output().stdout.clone();
    assert_eq!(
        stdout, expected,
        "combine -o - stdout must match the golden file byte-for-byte"
    );
}
