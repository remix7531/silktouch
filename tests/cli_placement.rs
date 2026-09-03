//! Placement (`placement = "symlink"`), driven the only way the CLI exposes
//! it: a registry set. One idempotent rule, three outcomes: see
//! `src/placement.rs`'s module docs.

mod common;

use std::fs;
use std::path::Path;

use common::*;
use tempfile::TempDir;

fn write_registry(dir: &Path, toml_text: &str) {
    let path = dir.join("silktouch").join("registry.toml");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, toml_text).unwrap();
}

fn registry_toml(fragments: &Path, output: &Path, target: &Path) -> String {
    format!(
        "[sets.test]\nfragments = {:?}\noutput = {:?}\nplacement = \"symlink\"\ntarget = {:?}\n",
        fragments.to_str().unwrap(),
        output.to_str().unwrap(),
        target.to_str().unwrap(),
    )
}

// ---- 16. An absent target gets an absolute symlink pointing at output. ---

#[test]
fn symlink_placement_with_absent_target_creates_absolute_symlink() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("out").join("settings.json");
    let target = root.path().join("deep").join("nested").join("target.json");

    write_registry(
        &root.path().join("xdg-config"),
        &registry_toml(frags.path(), &output, &target),
    );

    let mut cmd = bin();
    isolate_xdg(&mut cmd, root.path());
    cmd.args(["combine", "--set", "test"]).assert().success();

    let meta = fs::symlink_metadata(&target).expect("target should exist");
    assert!(meta.file_type().is_symlink(), "target must be a symlink");

    let resolved = fs::read_link(&target).expect("read_link");
    assert!(
        resolved.is_absolute(),
        "symlink must be absolute: {resolved:?}"
    );
    assert_eq!(
        resolved, output,
        "the symlink must resolve to exactly the output path"
    );
}

// ---- 17. Rerunning is a silent no op. -------------------------------

#[test]
fn symlink_placement_rerun_is_a_silent_noop() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("settings.json");
    let target = root.path().join("target.json");

    write_registry(
        &root.path().join("xdg-config"),
        &registry_toml(frags.path(), &output, &target),
    );

    let mut cmd = bin();
    isolate_xdg(&mut cmd, root.path());
    cmd.args(["combine", "--set", "test"]).assert().success();

    let link_mtime_before = fs::symlink_metadata(&target).unwrap().modified().unwrap();
    let output_mtime_before = mtime(&output);

    let mut cmd2 = bin();
    isolate_xdg(&mut cmd2, root.path());
    cmd2.args(["combine", "--set", "test"])
        .assert()
        .code(0)
        .stdout(predicates::str::is_empty());

    let link_mtime_after = fs::symlink_metadata(&target).unwrap().modified().unwrap();
    assert_eq!(
        link_mtime_before, link_mtime_after,
        "the symlink must not be touched on a no-op rerun"
    );
    assert_eq!(
        mtime(&output),
        output_mtime_before,
        "the output file must not be rewritten on a no-op rerun"
    );
    assert_eq!(
        fs::read_link(&target).unwrap(),
        output,
        "the symlink must still point at the output"
    );
}

// ---- 18. A regular file already at target: exit 2, name the path, and
// leave BOTH that file and the output file untouched. Seed the output with
// content that differs from what `combine` would produce, so the test
// proves the abort happened before the write. -------------------------

#[test]
fn symlink_placement_conflict_leaves_target_and_output_untouched() {
    let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("settings.json");
    let target = root.path().join("target.json");

    // A regular file already sits at `target`.
    let target_contents = "not a symlink, do not touch me";
    fs::write(&target, target_contents).unwrap();

    // The output already holds content that differs from what `combine`
    // would produce for these fragments (`{"a":1}` vs this): if the abort
    // did not happen before the write, this content would be clobbered.
    let output_contents = "{\"a\":999,\"stale\":true}\n";
    fs::write(&output, output_contents).unwrap();

    write_registry(
        &root.path().join("xdg-config"),
        &registry_toml(frags.path(), &output, &target),
    );

    let mut cmd = bin();
    isolate_xdg(&mut cmd, root.path());
    let assert = cmd.args(["combine", "--set", "test"]).assert().code(2);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(
        stderr.contains(target.to_str().unwrap()),
        "stderr must name the conflicting target path: {stderr:?}"
    );

    assert!(
        !fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink(),
        "target must still be the original regular file, not a symlink"
    );
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        target_contents,
        "the conflicting file's contents must be untouched"
    );
    assert_eq!(
        fs::read_to_string(&output).unwrap(),
        output_contents,
        "the output file must be untouched -- proves the abort happened before the write"
    );
}
