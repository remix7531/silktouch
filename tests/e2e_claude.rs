//! End to end fixture modelled on the motivating example from the README: a Claude
//! Code style `settings.json` split across `10-`, `20-`, `30-` and a
//! `99-local.json` catch all, with `/permissions/allow` drawing elements
//! from three different fragments under the default `concat-dedupe`
//! strategy.
//!
//! `tests/fixtures/claude/edited-output.json` stands in for what the
//! program wrote back to the combined file: an overwritten value
//! (`model`), a brand new key (`cleanupPeriodDays`), a deleted key
//! (`hooks`), an added array element (`Bash(pytest:*)`) and a removed one
//! (`Bash(git log:*)`).
//!
//! `tests/fixtures/claude/expected/*.json` is what each fragment should
//! contain after `sync`, worked out by hand from the README's "How split
//! works" routing table (owner = last declarer for overwrites, catch all
//! for new keys/added elements, every declarer for deletions/removed
//! elements): **not** derived by running the tool and pasting its
//! output. See the per fragment comments below for the derivation.

mod common;

use std::path::Path;

use common::*;
use tempfile::TempDir;

const FRAGMENT_NAMES: &[&str] = &[
    "10-permissions.json",
    "20-hooks.json",
    "30-editor.json",
    "99-local.json",
];

fn fixture_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude")
}

/// Copy only the fragment files (never `expected/` or `edited-output.json`)
/// into a fresh temp dir, so the real fixture on disk is never mutated.
fn seed_fragments() -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for name in FRAGMENT_NAMES {
        std::fs::copy(fixture_dir().join(name), dir.path().join(name)).expect("copy fragment");
    }
    dir
}

#[test]
fn sync_routes_the_external_edit_to_the_right_fragments() {
    let fixture = fixture_dir();
    let frags = seed_fragments();

    // 30-editor.json is untouched by the edit. Pin its mtime far in the past
    // so any rewrite (even a same second one) is unmistakable.
    let untouched_path = frags.path().join("30-editor.json");
    pin_old_mtime(&untouched_path);
    let untouched_mtime_before = mtime(&untouched_path);
    let untouched_bytes_before = read(&untouched_path);

    // The "program wrote this back" file.
    let root = TempDir::new().expect("tempdir");
    let output = root.path().join("settings.json");
    std::fs::copy(fixture.join("edited-output.json"), &output).expect("seed output");

    bin()
        .args([
            "sync",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .success();

    // ---- Changed fragments: byte identical to the hand derived expected
    // content (see module docs for the derivation of each). ----

    // 10-permissions.json: /permissions/allow loses "Bash(git log:*)" (an
    // array element in base but not output, so removed from every fragment
    // contributing it: only this one does). "deny" is untouched.
    let expected_10 = read(&fixture.join("expected/10-permissions.json"));
    assert_eq!(
        read(&frags.path().join("10-permissions.json")),
        expected_10,
        "10-permissions.json did not match the hand-derived expectation"
    );

    // 20-hooks.json: /hooks is present in base only (deleted entirely in the
    // edit), so removed from every fragment declaring it: this is the only
    // one. Its /permissions/allow entry ("Bash(npm test:*)") is untouched.
    let expected_20 = read(&fixture.join("expected/20-hooks.json"));
    assert_eq!(
        read(&frags.path().join("20-hooks.json")),
        expected_20,
        "20-hooks.json did not match the hand-derived expectation"
    );

    // 99-local.json (the catch all): /model is overwritten in place (it is
    // the sole declarer, hence the owner). /cleanupPeriodDays is a brand new
    // key, filed in the catch all. "Bash(pytest:*)" is a new array element,
    // appended to the catch all's own /permissions/allow array.
    let expected_99 = read(&fixture.join("expected/99-local.json"));
    assert_eq!(
        read(&frags.path().join("99-local.json")),
        expected_99,
        "99-local.json did not match the hand-derived expectation"
    );

    // ---- Untouched fragment: byte identical, mtime included. ----
    assert_eq!(
        read(&untouched_path),
        untouched_bytes_before,
        "30-editor.json must be byte-identical -- nothing in the edit touches it"
    );
    assert_eq!(
        mtime(&untouched_path),
        untouched_mtime_before,
        "30-editor.json's mtime must not move"
    );

    // ---- A following diff must exit 0: the round trip is complete. ----
    bin()
        .args([
            "diff",
            "--fragments",
            frags.path().to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
        ])
        .assert()
        .code(0)
        .stdout(predicates::str::is_empty());
}

/// Sanity check that the fixture's `edited-output.json` really is what
/// `combine` on the *unedited* fragments plus the documented edits would
/// produce: i.e. that the fixture is internally consistent, independent of
/// `split`/`route` entirely.
#[test]
fn fixture_combine_of_unedited_fragments_matches_the_documented_base() {
    let frags = seed_fragments();

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
    let base: serde_json::Value = serde_json::from_str(&stdout).unwrap();

    assert_eq!(
        base,
        serde_json::json!({
            "permissions": {
                "allow": [
                    "Bash(git diff:*)",
                    "Bash(git log:*)",
                    "Bash(npm test:*)",
                    "Bash(rm:*)"
                ],
                "deny": []
            },
            "hooks": {
                "PreToolUse": [
                    {
                        "matcher": "Bash",
                        "hooks": [
                            {"type": "command", "command": "echo pre"}
                        ]
                    }
                ]
            },
            "editorMode": "vim",
            "theme": "dark",
            "model": "claude-sonnet-4-5"
        }),
        "the fixture's base combine did not match the documented derivation"
    );
}
