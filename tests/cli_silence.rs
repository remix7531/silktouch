//! Rule of silence: a successful run prints nothing when nothing changed.
//! When something changed, one line per file written. See the README's
//! "rule of silence" example under Quick start.

mod common;

use std::fs;

use common::*;
use silktouch::format::Format;
use tempfile::TempDir;

fn combine_args(fragments: &std::path::Path, output: &std::path::Path) -> [String; 5] {
    [
        "combine".to_string(),
        "--fragments".to_string(),
        fragments.to_str().unwrap().to_string(),
        "-o".to_string(),
        output.to_str().unwrap().to_string(),
    ]
}

fn sync_args(fragments: &std::path::Path, output: &std::path::Path) -> [String; 5] {
    [
        "sync".to_string(),
        "--fragments".to_string(),
        fragments.to_str().unwrap().to_string(),
        "-o".to_string(),
        output.to_str().unwrap().to_string(),
    ]
}

fn stdout_of(assert: &assert_cmd::assert::Assert) -> String {
    String::from_utf8(assert.get_output().stdout.clone()).unwrap()
}

// ---- 6. combine twice: the second run prints nothing and exits 0. --------

#[test]
fn combine_twice_second_run_is_silent() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");

    let first = bin()
        .args(combine_args(frags.path(), &output))
        .assert()
        .success();
    assert!(
        stdout_of(&first).contains("wrote"),
        "first combine should report the write"
    );

    bin()
        .args(combine_args(frags.path(), &output))
        .assert()
        .code(0)
        .stdout(predicates::str::is_empty());
}

// ---- 7. sync twice: the second run writes nothing (fragment mtimes do not
// move between the two calls). ----------------------------------------------

#[test]
fn sync_twice_second_run_writes_nothing() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1,"list":["x","y"]}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");

    // Seed the output, then apply an external edit so the first `sync` has
    // real work to do (both a fragment write and a recombine write).
    bin()
        .args(combine_args(frags.path(), &output))
        .assert()
        .success();
    fs::write(&output, r#"{"a":2,"list":["x","y","z"]}"#).expect("external edit");

    let first = bin()
        .args(sync_args(frags.path(), &output))
        .assert()
        .success();
    assert!(
        stdout_of(&first).contains("wrote"),
        "first sync should report writes: {:?}",
        stdout_of(&first)
    );

    let frag_path = frags.path().join("10-base.json");
    let baseline_frag_mtime = mtime(&frag_path);
    let baseline_output_mtime = mtime(&output);

    bin()
        .args(sync_args(frags.path(), &output))
        .assert()
        .code(0)
        .stdout(predicates::str::is_empty());

    assert_eq!(
        mtime(&frag_path),
        baseline_frag_mtime,
        "second sync must not touch the fragment"
    );
    assert_eq!(
        mtime(&output),
        baseline_output_mtime,
        "second sync must not touch the output"
    );
}

// ---- 8. A run that does write prints exactly one `wrote` line per file
// written, and no more. ------------------------------------------------

#[test]
fn combine_into_a_fresh_output_prints_exactly_one_wrote_line() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");

    let assert = bin()
        .args(combine_args(frags.path(), &output))
        .assert()
        .success();
    let stdout = stdout_of(&assert);
    let wrote_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("wrote ")).collect();
    assert_eq!(
        wrote_lines,
        vec![format!("wrote {}", output.display())],
        "exactly one wrote line naming the output file, no more: {stdout:?}"
    );
    assert_eq!(
        stdout.lines().count(),
        1,
        "no other output besides the wrote line: {stdout:?}"
    );
}

#[test]
fn sync_prints_one_wrote_line_per_file_actually_written() {
    let frags = fragments_dir(&[
        ("10-a.json", r#"{"a":1}"#),
        ("20-b.json", r#"{"b":1}"#),
        ("30-c.json", r#"{"c":1}"#),
    ]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("output.json");

    bin()
        .args(combine_args(frags.path(), &output))
        .assert()
        .success();

    // Edit only `a` and `b`. `c` must stay untouched, and its fragment file
    // must not appear in the write report. Written in the same canonical
    // format `combine` itself produces, so the recombine step at the end of
    // `sync` is *also* a no op and only the two fragments are reported.
    let edited = silktouch::format::Json
        .serialize(
            &serde_json::json!({"a": 9, "b": 9, "c": 1}),
            &silktouch::format::WriteOpts::default(),
        )
        .expect("serialize edited output");
    fs::write(&output, edited).expect("external edit");

    let assert = bin()
        .args(sync_args(frags.path(), &output))
        .assert()
        .success();
    let stdout = stdout_of(&assert);
    let wrote_lines: Vec<&str> = stdout.lines().filter(|l| l.starts_with("wrote ")).collect();

    // The output file is unchanged in content (already what the edit made
    // it), so only the two edited fragments should be reported.
    let expect_a = format!("wrote {}", frags.path().join("10-a.json").display());
    let expect_b = format!("wrote {}", frags.path().join("20-b.json").display());
    assert_eq!(
        wrote_lines.len(),
        2,
        "exactly two wrote lines, one per changed fragment: {stdout:?}"
    );
    assert!(wrote_lines.contains(&expect_a.as_str()), "{stdout:?}");
    assert!(wrote_lines.contains(&expect_b.as_str()), "{stdout:?}");
    assert!(
        !stdout.contains("30-c.json"),
        "the untouched fragment must not be mentioned: {stdout:?}"
    );
}
