//! silktouch: a bidirectional config fragment merger.
//!
//! This module wires the engine modules (see the crate's other `pub mod`s)
//! into the three public entry points, [`combine`], [`split`] and [`diff`],
//! plus [`Config`] to drive them.
//!
//! `sync` (split then combine) is deliberately **not** here. It is CLI
//! sugar, and nothing in the library may depend on it. It lives in
//! `main.rs`.

// Every fallible function in this crate returns the one shared
// `crate::error::Result<T>` (`Result<T, error::Error>`), and every `Error`
// variant already carries a `#[error("...")]` message in `error.rs` that
// says exactly when it fires. A per function "# Errors" section would, at
// 19 call sites, do nothing but repeat "returns `Err`: see `Error`". That
// is pure boilerplate, with no information `Error`'s own docs don't
// already give more precisely.
#![allow(clippy::missing_errors_doc)]

pub mod delta;
pub mod error;
pub mod format;
pub mod fragment;
pub mod merge;
pub mod placement;
pub mod pointer;
pub mod registry;
pub mod route;

pub use error::{Error, Result};

pub use delta::Change;
pub use format::{Comments, Format, Json, ReadOpts, WriteOpts};
pub use fragment::{Fragment, FragmentSet};
pub use merge::combine_values;
pub use pointer::{ArrayStrategy, Pointer, StrategyTable};
pub use route::RouteReport;

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::fragment::write_atomic;

/// Where a combined document goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutputTarget {
    /// Write to (and, for `split`/`diff`, read from) this path.
    Path(PathBuf),
    /// `combine` only: emit to stdout and touch no files at all.
    Stdout,
}

/// How the output file is delivered to whatever reads it.
///
/// Applied by [`combine`] (and so by the CLI's `sync`), reported by [`diff`].
/// Never applied for [`OutputTarget::Stdout`], which touches no files at all.
/// See [`crate::placement`] for the three outcomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// silktouch touches only `output`.
    None,
    /// silktouch also ensures `target` is a symlink pointing at `output`.
    Symlink { target: PathBuf },
}

/// Everything needed to drive [`combine`], [`split`] or [`diff`].
#[derive(Clone)]
pub struct Config {
    pub fragments: PathBuf,
    pub output: OutputTarget,
    pub catch_all: String,
    pub strategies: StrategyTable,
    pub indent: usize,
    pub allow_empty: bool,
    pub placement: Placement,
    pub format: &'static dyn Format,
    /// How comments in source fragments (and the output file) are treated.
    /// Defaults to [`Comments::Forbid`]: see its docs for why.
    pub comments: Comments,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            fragments: PathBuf::new(),
            output: OutputTarget::Stdout,
            catch_all: "99-local.json".to_string(),
            strategies: StrategyTable::default(),
            indent: 2,
            allow_empty: false,
            placement: Placement::None,
            format: &Json,
            comments: Comments::Forbid,
        }
    }
}

/// The result of [`combine`].
#[derive(Debug, Clone, PartialEq)]
pub struct Combined {
    /// The combined document.
    pub value: Value,
    /// Its serialised text, exactly as written (or as would have been
    /// written, for `OutputTarget::Stdout`).
    pub text: String,
    /// The path actually written, or `None` when nothing was written:
    /// either `OutputTarget::Stdout`, or the file already held identical
    /// content.
    pub wrote: Option<PathBuf>,
    /// The placement target newly symlinked to the output, or `None` when
    /// placement is `None`, the link was already correct, or the output went
    /// to stdout. Lets a caller report only what actually changed.
    pub linked: Option<PathBuf>,
}

/// The result of [`split`].
#[derive(Debug, Clone, PartialEq)]
pub struct SplitOutcome {
    /// The changes classified between the fragments' combined base and the
    /// on disk output, in the order they were routed.
    pub changes: Vec<Change>,
    /// Fragment paths actually rewritten (a subset of the fragments that
    /// changed, see [`FragmentSet::write_back`]).
    pub written: Vec<PathBuf>,
}

/// The result of [`diff`].
#[derive(Debug, Clone, PartialEq)]
pub struct DiffOutcome {
    /// The changes classified between the fragments' combined base and the
    /// on disk output.
    pub changes: Vec<Change>,
    /// Whether `changes` is empty. Exit code fodder for the CLI: 0 when
    /// `true`, 1 when `false`, matching `diff(1)`.
    pub in_sync: bool,
}

/// The JSON type name of `v`, for use in [`Error::OutputRootNotObject`].
///
/// A near duplicate of the private `type_name` in [`fragment`]: that one is
/// not `pub(crate)`, and this crate's "one file" ownership discipline for
/// this wave keeps this module from reaching in to change that.
fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Resolve `output` to a real path, or an error for `split`/`diff`, which
/// have no meaningful "stdout" reading side.
fn require_output_path<'a>(output: &'a OutputTarget, verb: &'static str) -> Result<&'a Path> {
    match output {
        OutputTarget::Path(p) => Ok(p.as_path()),
        // Not OutputMissing: nothing is missing. Saying "output file not
        // found: -" would send the reader looking for a file they never
        // named. This is an invalid combination of flags, and says so.
        OutputTarget::Stdout => Err(Error::OutputIsStdout { verb }),
    }
}

/// Read, parse and validate the on disk output document at `path`.
///
/// A missing file is [`Error::OutputMissing`], never treated as "everything
/// was deleted". A non object root is [`Error::OutputRootNotObject`]. `comments`
/// governs whether a comment in the output file is accepted, exactly as for
/// a fragment. There is no write back concern here (`combine` always
/// regenerates the output file fresh from the fragments' values, so it is
/// never a candidate for [`Error::CommentsWouldBeLost`]).
fn read_output_object(fmt: &dyn Format, path: &Path, comments: Comments) -> Result<Value> {
    let raw = fs::read_to_string(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::OutputMissing(path.to_path_buf())
        } else {
            Error::Io {
                path: path.to_path_buf(),
                source,
            }
        }
    })?;

    let value = fmt.parse(&raw, path, &ReadOpts { comments })?;
    if !value.is_object() {
        return Err(Error::OutputRootNotObject {
            path: path.to_path_buf(),
            found: type_name(&value),
        });
    }
    Ok(value)
}

/// Combine the fragments in `cfg.fragments` into one document.
///
/// `OutputTarget::Stdout` touches no files whatsoever: no directory
/// creation, no write, no placement, so the engine is fully drivable with
/// no filesystem access on the output side. `OutputTarget::Path` creates any
/// missing parent directories and writes atomically. `wrote` is `Some` only
/// when a write actually happened (see [`fragment::write_atomic`]).
///
/// A placement conflict is detected *before* the write, so it leaves the
/// output file exactly as it was: a conflict must change nothing, and an
/// output holding an external edit that has not been split back yet is
/// precisely the case where clobbering it would lose data.
pub fn combine(cfg: &Config) -> Result<Combined> {
    let set = FragmentSet::load(
        &cfg.fragments,
        cfg.format,
        &cfg.catch_all,
        cfg.allow_empty,
        cfg.comments,
    )?;
    let value = combine_values(&set.fragments, &cfg.strategies);

    let opts = WriteOpts {
        indent: cfg.indent,
        trailing_newline: true,
    };
    let text = cfg.format.serialize(&value, &opts)?;

    // Stdout is the filter mode: no directories, no write, and no placement.
    // Placement only makes sense once a real file exists to point at.
    let mut linked = None;
    let wrote = match &cfg.output {
        OutputTarget::Stdout => None,
        OutputTarget::Path(path) => {
            // Check placement BEFORE writing. A conflict must leave everything
            // as it was, and `write_atomic` has already happened by the time
            // `apply` could report one, so ask `status`, which never writes.
            if let placement::PlacementStatus::Conflict { path: conflict } =
                placement::status(&cfg.placement, path)?
            {
                return Err(Error::PlacementConflict {
                    path: conflict,
                    output: path.clone(),
                });
            }

            if let Some(parent) = path.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|source| Error::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }

            let did_write = write_atomic(path, &text)?;

            // After the write, so the symlink never points at a file that is
            // not there yet. `apply` is idempotent and returns `None` when the
            // link is already correct, so a steady state run stays silent.
            linked = placement::apply(&cfg.placement, path)?;

            if did_write { Some(path.clone()) } else { None }
        }
    };

    Ok(Combined {
        value,
        text,
        wrote,
        linked,
    })
}

/// Split the on disk output back into the fragments that produced it.
///
/// Recomputes the expected base from the fragments, classifies it against
/// the actual output file, routes each difference to the fragment(s) that
/// should absorb it, and writes back only what changed.
///
/// `OutputTarget::Stdout` is meaningless here (there is nothing to read) and
/// is an error rather than being treated as "no output" or "delete
/// everything".
pub fn split(cfg: &Config) -> Result<SplitOutcome> {
    let mut set = FragmentSet::load(
        &cfg.fragments,
        cfg.format,
        &cfg.catch_all,
        cfg.allow_empty,
        cfg.comments,
    )?;
    set.check_catch_all_sorts_last()?;

    let output_path = require_output_path(&cfg.output, "split")?;
    let output_value = read_output_object(cfg.format, output_path, cfg.comments)?;

    let base = combine_values(&set.fragments, &cfg.strategies);
    let changes = delta::classify(&base, &output_value, &cfg.strategies)?;
    route::route(&mut set, &changes, &cfg.strategies)?;

    let opts = WriteOpts {
        indent: cfg.indent,
        trailing_newline: true,
    };
    let written = set.write_back(cfg.format, &opts, cfg.comments)?;

    Ok(SplitOutcome { changes, written })
}

/// Report whether the fragments and the on disk output agree, without
/// writing anything, ever.
///
/// `in_sync` is derived solely from [`delta::classify`]'s output, never from
/// a raw `json_patch::diff`: `combine` reemits array elements in fragment
/// order while the output file keeps its own, so a raw diff would report
/// differences forever on a perfectly synchronised set.
pub fn diff(cfg: &Config) -> Result<DiffOutcome> {
    let set = FragmentSet::load(
        &cfg.fragments,
        cfg.format,
        &cfg.catch_all,
        cfg.allow_empty,
        cfg.comments,
    )?;
    set.check_catch_all_sorts_last()?;

    let output_path = require_output_path(&cfg.output, "diff")?;
    let output_value = read_output_object(cfg.format, output_path, cfg.comments)?;

    let base = combine_values(&set.fragments, &cfg.strategies);
    let changes = delta::classify(&base, &output_value, &cfg.strategies)?;
    let in_sync = changes.is_empty();

    Ok(DiffOutcome { changes, in_sync })
}

/// The filesystem free core of `split`: classify `output` against the
/// fragments' current combined value and route the result into `set`,
/// mutating it in place. No file is read or written.
///
/// This is what property tests and embedders drive directly, with no
/// [`OutputTarget`], no output file, and no [`Config`] at all.
pub fn split_values(
    set: &mut FragmentSet,
    output: &Value,
    strategies: &StrategyTable,
) -> Result<Vec<Change>> {
    let base = combine_values(&set.fragments, strategies);
    let changes = delta::classify(&base, output, strategies)?;
    route::route(set, &changes, strategies)?;
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fragment::Fragment;
    use serde_json::json;
    use std::collections::HashSet;
    use std::fs;
    use tempfile::TempDir;

    /// Write `contents` (name -> JSON text) into a fresh temp directory of
    /// fragment files.
    fn fragments_dir(contents: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        for (name, text) in contents {
            fs::write(dir.path().join(name), text).expect("write fragment");
        }
        dir
    }

    fn cfg(fragments: &Path, output: OutputTarget) -> Config {
        Config {
            fragments: fragments.to_path_buf(),
            output,
            ..Config::default()
        }
    }

    /// Listing of a directory's entry names, sorted, for before/after
    /// comparisons that must show no new files appeared.
    fn listing(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // 1. combine to Stdout returns text and touches no files.
    #[test]
    fn combine_stdout_touches_no_files() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let before = listing(dir.path());

        let c = cfg(dir.path(), OutputTarget::Stdout);
        let combined = combine(&c).expect("combine");

        assert_eq!(combined.wrote, None);
        assert_eq!(combined.value, json!({"a": 1}));
        assert!(combined.text.contains("\"a\""));

        let after = listing(dir.path());
        assert_eq!(before, after, "combine to stdout must create no files");
    }

    /// A placement conflict must abort before anything is written. The
    /// output file here holds an external edit that has not been split back
    /// yet. Clobbering it and *then* reporting the conflict would destroy
    /// data the user still needed, and the spec says a conflict changes
    /// nothing.
    #[test]
    fn placement_conflict_aborts_before_writing_the_output() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let out_dir = TempDir::new().unwrap();
        let output = out_dir.path().join("settings.json");
        let edited = "{\n  \"a\": 99,\n  \"new\": \"keep me\"\n}\n";
        fs::write(&output, edited).unwrap();

        // Something that is not our symlink is already sitting at the target.
        let target = out_dir.path().join("target.json");
        fs::write(&target, "not a symlink").unwrap();

        let mut c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        c.placement = Placement::Symlink {
            target: target.clone(),
        };

        let err = combine(&c).expect_err("a placement conflict must fail");
        assert!(matches!(err, Error::PlacementConflict { .. }), "{err:?}");

        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            edited,
            "the output must be untouched when placement conflicts"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), "not a symlink");
    }

    // 2. combine into a not yet existing nested directory creates the
    // parents and writes the file.
    #[test]
    fn combine_creates_nested_parent_dirs() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("nested/deep/output.json");
        assert!(!output.parent().unwrap().exists());

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        let combined = combine(&c).expect("combine");

        assert_eq!(combined.wrote, Some(output.clone()));
        assert!(output.exists());
        let on_disk = fs::read_to_string(&output).expect("read output");
        assert_eq!(on_disk, combined.text);
    }

    // 3. combine twice in a row: the second is a no op (wrote: None).
    #[test]
    fn combine_twice_is_idempotent_on_disk() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        let first = combine(&c).expect("first combine");
        assert_eq!(first.wrote, Some(output.clone()));

        let second = combine(&c).expect("second combine");
        assert_eq!(second.wrote, None, "identical content must not rewrite");
    }

    // 4. split with a missing output file errors and leaves every fragment
    // byte identical on disk.
    #[test]
    fn split_missing_output_errors_and_leaves_fragments_untouched() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let frag_path = dir.path().join("10-base.json");
        let before = fs::read(&frag_path).expect("read fragment");

        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("does-not-exist.json");

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        let err = split(&c).expect_err("missing output must error");
        match err {
            Error::OutputMissing(p) => assert_eq!(p, output),
            other => panic!("expected OutputMissing, got {other:?}"),
        }

        let after = fs::read(&frag_path).expect("read fragment again");
        assert_eq!(before, after, "fragments must be byte-identical");
    }

    // 5. split with a non object output root errors naming the file.
    #[test]
    fn split_non_object_output_root_errors() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");
        fs::write(&output, "[1,2,3]").expect("write array output");

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        let err = split(&c).expect_err("non-object root must error");
        match err {
            Error::OutputRootNotObject { path, found } => {
                assert_eq!(path, output);
                assert_eq!(found, "array");
            }
            other => panic!("expected OutputRootNotObject, got {other:?}"),
        }
    }

    // 6. split with OutputTarget::Stdout errors.
    #[test]
    fn split_stdout_errors() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let c = cfg(dir.path(), OutputTarget::Stdout);
        let err = split(&c).expect_err("split to stdout must error");
        assert!(matches!(err, Error::OutputIsStdout { verb: "split" }));
        // The message must not send the reader looking for a missing file.
        let msg = err.to_string();
        assert!(msg.contains("stdout"), "{msg}");
        assert!(!msg.contains("missing"), "{msg}");
    }

    // 7. diff on a synchronised set is in_sync, has no changes, and writes
    // nothing (mtimes of fragments and output are unchanged).
    #[test]
    fn diff_in_sync_writes_nothing() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1,"list":["x","y"]}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        combine(&c).expect("seed output");

        let frag_path = dir.path().join("10-base.json");
        let frag_mtime_before = fs::metadata(&frag_path).unwrap().modified().unwrap();
        let out_mtime_before = fs::metadata(&output).unwrap().modified().unwrap();

        let outcome = diff(&c).expect("diff");
        assert!(outcome.in_sync);
        assert!(outcome.changes.is_empty());

        let frag_mtime_after = fs::metadata(&frag_path).unwrap().modified().unwrap();
        let out_mtime_after = fs::metadata(&output).unwrap().modified().unwrap();
        assert_eq!(frag_mtime_before, frag_mtime_after);
        assert_eq!(out_mtime_before, out_mtime_after);
    }

    // 8. diff after an external edit is not in_sync and names the changed
    // path.
    #[test]
    fn diff_detects_external_edit() {
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");

        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        combine(&c).expect("seed output");

        fs::write(&output, r#"{"a":2}"#).expect("external edit");

        let outcome = diff(&c).expect("diff");
        assert!(!outcome.in_sync);
        assert_eq!(outcome.changes.len(), 1);
        assert_eq!(outcome.changes[0].path().as_str(), "/a");
    }

    // 9. Law A at the API level: combine then split is a no op.
    #[test]
    fn law_a_combine_then_split_is_noop() {
        let dir = fragments_dir(&[
            ("10-base.json", r#"{"a":1,"b":{"x":1},"list":["a","b"]}"#),
            ("99-local.json", r#"{"local":true}"#),
        ]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");
        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));

        combine(&c).expect("combine");

        let before: Vec<(PathBuf, Vec<u8>)> = ["10-base.json", "99-local.json"]
            .iter()
            .map(|n| {
                let p = dir.path().join(n);
                let bytes = fs::read(&p).unwrap();
                (p, bytes)
            })
            .collect();

        let outcome = split(&c).expect("split");
        assert!(
            outcome.written.is_empty(),
            "split after an unedited combine must write nothing"
        );

        for (path, bytes) in &before {
            let after = fs::read(path).unwrap();
            assert_eq!(bytes, &after, "fragment {path:?} must be byte-identical");
        }
    }

    // 10. Law B at the API level: an external edit survives combine(split(..)).
    #[test]
    fn law_b_external_edit_survives_round_trip() {
        let dir = fragments_dir(&[(
            "10-base.json",
            r#"{"a":1,"b":{"x":1},"list":["a","b"],"toDelete":"gone"}"#,
        )]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");
        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));

        combine(&c).expect("seed output");

        // External edit: change a value, change a nested value, delete a
        // key, add a new key, and append to a concat-dedupe array.
        let edited = json!({
            "a": 2,
            "b": {"x": 2},
            "list": ["a", "b", "c"],
            "newKey": "newValue"
        });
        let edited_text = serde_json::to_string_pretty(&edited).unwrap();
        fs::write(&output, edited_text).expect("external edit");

        let split_outcome = split(&c).expect("split");
        assert!(!split_outcome.changes.is_empty());

        let combined = combine(&c).expect("re-combine");

        let obj = combined.value.as_object().expect("object root");
        assert_eq!(obj.get("a"), Some(&json!(2)));
        assert_eq!(obj.get("b"), Some(&json!({"x": 2})));
        assert_eq!(obj.get("newKey"), Some(&json!("newValue")));
        assert!(obj.get("toDelete").is_none());

        let list: HashSet<String> = obj
            .get("list")
            .and_then(Value::as_array)
            .expect("list array")
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let expected: HashSet<String> = ["a", "b", "c"].iter().map(ToString::to_string).collect();
        assert_eq!(list, expected, "list must match as a multiset");
    }

    // 11. split_values drives the engine with no filesystem and matches the
    // file based split for an equivalent setup.
    #[test]
    fn split_values_matches_file_based_split() {
        let edited = json!({
            "a": 2,
            "list": ["a", "b", "c"],
            "newKey": "newValue"
        });

        // File based side.
        let dir = fragments_dir(&[("10-base.json", r#"{"a":1,"list":["a","b"]}"#)]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");
        let c = cfg(dir.path(), OutputTarget::Path(output.clone()));
        combine(&c).expect("seed output");
        fs::write(&output, serde_json::to_string(&edited).unwrap()).expect("external edit");
        let file_outcome = split(&c).expect("file-based split");

        // Filesystem free side: a hand built FragmentSet, no directory
        // involved at all.
        let value = json!({"a": 1, "list": ["a", "b"]});
        let fragment = Fragment {
            path: PathBuf::from("10-base.json"),
            name: "10-base.json".to_string(),
            value: value.clone(),
            original: value,
            raw: None,
            had_comments: false,
        };
        let mut set = FragmentSet {
            dir: PathBuf::from("/nonexistent"),
            fragments: vec![fragment],
            catch_all: "99-local.json".to_string(),
        };
        let strategies = StrategyTable::default();
        let changes = split_values(&mut set, &edited, &strategies).expect("split_values");

        assert_eq!(changes, file_outcome.changes);
    }

    // ---- Comments (JSONC): API level behaviour, on top of the unit tests
    // in fragment.rs and format.rs. ----------------------------------------

    fn cfg_with_comments(fragments: &Path, output: OutputTarget, comments: Comments) -> Config {
        Config {
            fragments: fragments.to_path_buf(),
            output,
            comments,
            ..Config::default()
        }
    }

    /// **Protect, read only.** A commented fragment that `combine`/`diff`
    /// only ever *read* must work fine: the `Protect` check lives at the
    /// write back boundary in `FragmentSet::write_back`, which neither of
    /// these call.
    #[test]
    fn protect_mode_a_commented_fragment_that_is_only_read_does_not_error() {
        let dir = fragments_dir(&[("10-base.json", "{\n  \"a\": 1 // keep me\n}\n")]);
        let root = TempDir::new().expect("tempdir");
        let output = root.path().join("output.json");
        let c = cfg_with_comments(dir.path(), OutputTarget::Path(output), Comments::Protect);

        let combined = combine(&c).expect("combine must succeed on a read-only commented fragment");
        assert_eq!(combined.value, json!({"a": 1}));

        let outcome = diff(&c).expect("diff must succeed on a read-only commented fragment");
        assert!(outcome.in_sync, "nothing was edited, so diff must agree");
    }

    /// **The combined output never contains a comment, in every mode where
    /// parsing succeeds.** `Forbid` never has a commented fragment to begin
    /// with (exercised here with a comment free fragment for symmetry)
    /// while `Protect` and `Strip` both accept one on read. Either way,
    /// `combine` always writes the output fresh from parsed `Value`s, which
    /// never carry comments at all.
    #[test]
    fn combined_output_never_contains_a_comment() {
        for (comments, fragment_text) in [
            (Comments::Forbid, r#"{"a": 1}"#.to_string()),
            (Comments::Protect, "{\n  \"a\": 1 // note\n}\n".to_string()),
            (Comments::Strip, "{\n  \"a\": 1 /* note */\n}\n".to_string()),
        ] {
            let dir = fragments_dir(&[("10-base.json", &fragment_text)]);
            let root = TempDir::new().expect("tempdir");
            let output = root.path().join("output.json");
            let c = cfg_with_comments(dir.path(), OutputTarget::Path(output.clone()), comments);

            let combined = combine(&c).expect("combine must succeed");
            assert_eq!(combined.value, json!({"a": 1}));
            assert!(
                !combined.text.contains("//") && !combined.text.contains("/*"),
                "combined text held a comment under {comments:?}: {}",
                combined.text
            );
            let on_disk = fs::read_to_string(&output).expect("read output");
            assert!(
                !on_disk.contains("//") && !on_disk.contains("/*"),
                "output file held a comment under {comments:?}: {on_disk}"
            );
        }
    }
}

#[cfg(test)]
mod laws {
    //! Property tests for the three round trip laws (see the README's "The
    //! three laws" section).
    //!
    //! These live **in crate** rather than in `tests/` because [`Fragment`]'s
    //! `original` and `raw` fields are `pub(crate)`: an external integration
    //! test cannot hand build a fragment, and would have to go through a
    //! `TempDir` and [`FragmentSet::load`] for every single case. The
    //! value level laws need no filesystem at all, so they run here directly
    //! against [`combine_values`] and [`split_values`]: orders of magnitude
    //! faster to generate and, more importantly, to *shrink*.
    //!
    //! One law does need the filesystem: law A's byte identity claim. A
    //! value based no op check and a byte based one are indistinguishable in
    //! memory. Only a fragment file whose formatting differs from what
    //! silktouch would emit tells them apart. [`law_a_inverse_on_disk`]
    //! therefore writes deliberately non canonical fragments and drives the
    //! real file based [`split`].
    //!
    //! # What the generators deliberately exclude, and why
    //!
    //! Each exclusion below removes a shape that is **not representable** by
    //! the design, not one that is merely inconvenient. They are listed here
    //! because an over narrowed generator would make the laws vacuous.
    //!
    //! - **No floats.** `-0.0 == 0.0` as [`Value`]s but they serialise
    //!   differently (`-0.0` vs `0.0`), which would produce spurious
    //!   byte level failures in law A's on disk case with no bug behind them.
    //!   `NaN`/infinities are not JSON at all. Config data is integers and
    //!   strings. Nothing about the round trip is float specific.
    //! - **Arrays hold distinct elements.** `concat-dedupe` has *set*
    //!   semantics: `combine` drops later duplicates, so an array containing
    //!   the same element twice is not a value silktouch can represent, and
    //!   generating one would fail law B for a reason that is not a bug.
    //!   Generated arrays are deduped, and `ArrayAdd` refuses to insert an
    //!   element already present.
    //! - **Edits address object paths only, never array interiors.** An edit
    //!   at `/list/0/k` is representable, but it can silently turn two
    //!   distinct elements into equal ones (the duplicate case above,
    //!   arrived at indirectly). Array interiors are exercised through
    //!   `ArrayAdd`/`ArrayRemove` instead, which is also how a foreign
    //!   program's drop in list actually changes.
    //! - **`TypeFlip` is object to scalar, or scalar to object, only** (as
    //!   specified), never scalar to array. Nothing else is excluded from it.
    //!
    //! Object keys come from the alphabet `a`..`f` precisely so that
    //! fragments *collide*: with a wide key space every fragment would be
    //! disjoint from every other, ownership would be trivial, and shadowing
    //! (the interesting half of `owner_of`) would never be exercised.

    use super::*;
    use crate::fragment::Fragment;
    use proptest::prelude::*;
    use serde_json::{Map, Value};
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// The catch all used by every generated case. Sorts after every
    /// generated fragment name (`00-f0.json`, `10-f1.json`, `20-f2.json`),
    /// as [`FragmentSet::check_catch_all_sorts_last`] demands.
    const CATCH_ALL: &str = "99-local.json";

    /// Length of the fixed size seed vectors. Leaf counts vary per generated
    /// universe, so the seeds are indexed modulo this rather than being
    /// length matched to the universe: dependent lengths would need a
    /// `prop_flat_map` and shrink far worse.
    const SEEDS: usize = 16;

    // ---------------------------------------------------------------- values

    /// Canonical serialisation of a value: a total, cheap ordering key that
    /// works without `Ord` on [`Value`].
    fn key_of(v: &Value) -> String {
        serde_json::to_string(v).expect("a Value always serialises")
    }

    /// Drop later duplicates by full value equality, keeping the first,
    /// exactly `combine`'s `concat-dedupe` rule.
    fn dedupe(items: Vec<Value>) -> Vec<Value> {
        let mut out: Vec<Value> = Vec::with_capacity(items.len());
        for item in items {
            if !out.contains(&item) {
                out.push(item);
            }
        }
        out
    }

    /// `null | bool | i64 | short string`. **No floats.** See the module
    /// docs for why.
    fn arb_scalar() -> impl Strategy<Value = Value> {
        prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            (-100i64..100).prop_map(|n| Value::Number(n.into())),
            "[a-f]{0,3}".prop_map(Value::String),
        ]
    }

    /// What may sit *inside* an array: scalars, objects, and (since
    /// `delta::outermost_array_ancestor` hoists to the shallowest enclosing
    /// array rather than the deepest) arrays again. An array nested inside an
    /// array used to make `classify` emit a change at an indexed path, which
    /// `route` cannot file. Nothing is excluded here on that account any more.
    // `breadth` is always a small hardcoded literal at every call site below
    // (2, 3, 4). The usize->u32 cast can never truncate in practice, and
    // `breadth` must stay `usize` to feed the `0..breadth` collection
    // ranges used elsewhere in this function.
    #[allow(clippy::cast_possible_truncation)]
    fn arb_array_element(depth: u32, breadth: usize) -> impl Strategy<Value = Value> {
        arb_scalar().prop_recursive(depth, 12, breadth as u32, move |inner| {
            prop_oneof![
                prop::collection::vec(inner.clone(), 0..breadth)
                    .prop_map(|v| Value::Array(dedupe(v))),
                prop::collection::vec(("[a-f]{1,2}", inner), 0..breadth)
                    .prop_map(|kvs| Value::Object(kvs.into_iter().collect::<Map<String, Value>>())),
            ]
        })
    }

    /// Arbitrary JSON: scalars, objects keyed from `a`..`f`, and arrays of
    /// 0..4 **distinct** elements.
    // Same rationale as `arb_array_element` above: `breadth` is always a
    // small hardcoded literal at the call sites below.
    #[allow(clippy::cast_possible_truncation)]
    fn arb_value(depth: u32, breadth: usize) -> impl Strategy<Value = Value> {
        arb_scalar().prop_recursive(depth, 24, breadth as u32, move |inner| {
            prop_oneof![
                prop::collection::vec(arb_array_element(2, breadth), 0..4)
                    .prop_map(|v| Value::Array(dedupe(v))),
                prop::collection::vec(("[a-f]{1,2}", inner), 0..breadth)
                    .prop_map(|kvs| Value::Object(kvs.into_iter().collect::<Map<String, Value>>())),
            ]
        })
    }

    /// The root object every case is built from. Non empty, so that
    /// [`leaf_pointers`] always yields at least one non root pointer.
    fn arb_universe() -> impl Strategy<Value = Value> {
        prop::collection::vec(("[a-f]{1,2}", arb_value(3, 4)), 2..6)
            .prop_map(|kvs| Value::Object(kvs.into_iter().collect::<Map<String, Value>>()))
    }

    // ----------------------------------------------------------------- edits

    /// One external edit, drawn against whatever document it is applied to:
    /// `which`/`pos`/`elem` are resolved modulo the number of candidates, so
    /// a spec stays meaningful as proptest shrinks the document underneath
    /// it.
    #[derive(Debug, Clone)]
    enum EditSpec {
        /// An existing pointer set to a new scalar.
        Update { which: usize, value: Value },
        /// An existing pointer removed.
        Delete { which: usize },
        /// A fresh key into an existing object. The `n` prefix keeps it
        /// outside the `a`..`f` key alphabet, so it is genuinely new.
        InsertNew {
            which: usize,
            key: String,
            value: Value,
        },
        /// A fresh element into an existing array, at an arbitrary position.
        /// Position matters, because `combine` reemits in fragment order and
        /// law C depends on `classify` discarding pure reorderings.
        ArrayAdd {
            which: usize,
            pos: usize,
            value: Value,
        },
        /// An existing element out of an existing array.
        ArrayRemove { which: usize, elem: usize },
        /// Object to scalar, or scalar to object, at an existing pointer.
        TypeFlip {
            which: usize,
            key: String,
            scalar: Value,
        },
    }

    fn arb_edit() -> impl Strategy<Value = EditSpec> {
        prop_oneof![
            (0usize..32, arb_scalar()).prop_map(|(which, value)| EditSpec::Update { which, value }),
            (0usize..32).prop_map(|which| EditSpec::Delete { which }),
            (0usize..32, "[a-f]{1,2}", arb_value(2, 3)).prop_map(|(which, key, value)| {
                EditSpec::InsertNew {
                    which,
                    key: format!("n{key}"),
                    value,
                }
            }),
            (0usize..32, 0usize..8, arb_array_element(2, 3))
                .prop_map(|(which, pos, value)| EditSpec::ArrayAdd { which, pos, value }),
            (0usize..32, 0usize..8).prop_map(|(which, elem)| EditSpec::ArrayRemove { which, elem }),
            (0usize..32, "[a-f]{1,2}", arb_scalar())
                .prop_map(|(which, key, scalar)| EditSpec::TypeFlip { which, key, scalar }),
        ]
    }

    // ------------------------------------------------------------------ case

    /// A whole generated scenario: one universe, how its leaves are spread
    /// over fragments, which array paths use `replace`, and the external
    /// edits to apply to the combined document.
    #[derive(Debug, Clone)]
    struct Case {
        universe: Value,
        /// Non catch all fragments. The catch all is always appended, so the
        /// set holds `extra_frags + 1` files.
        extra_frags: usize,
        /// Per leaf (modulo `SEEDS`): which fragment declares it.
        assign: Vec<u8>,
        /// Per leaf (modulo `SEEDS`): a second, *different* value declared by
        /// a strictly higher index fragment. This is the only thing that
        /// exercises shadowing and `owner_of`, so it is deliberately common
        /// (~30%).
        shadow: Vec<Option<Value>>,
        /// Per array leaf (modulo `SEEDS`): `replace` instead of the default
        /// `concat-dedupe`.
        replace: Vec<bool>,
        edits: Vec<EditSpec>,
    }

    fn arb_case() -> impl Strategy<Value = Case> {
        (
            arb_universe(),
            1usize..4,
            prop::collection::vec(any::<u8>(), SEEDS),
            prop::collection::vec(prop::option::weighted(0.3, arb_value(2, 3)), SEEDS),
            prop::collection::vec(any::<bool>(), SEEDS),
            prop::collection::vec(arb_edit(), 0..6),
        )
            .prop_map(
                |(universe, extra_frags, assign, shadow, replace, edits)| Case {
                    universe,
                    extra_frags,
                    assign,
                    shadow,
                    replace,
                    edits,
                },
            )
    }

    // ------------------------------------------------------------- machinery

    /// Every pointer whose value is not a non empty object, i.e. the deepest
    /// paths that carry actual content. An empty object counts as a leaf so
    /// that assigning all leaves reproduces the universe's full shape.
    fn leaf_pointers(v: &Value, at: &Pointer, out: &mut Vec<(Pointer, Value)>) {
        match v {
            Value::Object(map) if !map.is_empty() => {
                for (k, child) in map {
                    leaf_pointers(child, &at.child(k), out);
                }
            }
            other => out.push((at.clone(), other.clone())),
        }
    }

    /// Every pointer reachable from the root through **object keys only**.
    /// Array interiors are never entered. See the module docs.
    fn object_paths(v: &Value, at: &Pointer, out: &mut Vec<Pointer>) {
        if let Value::Object(map) = v {
            for (k, child) in map {
                let p = at.child(k);
                out.push(p.clone());
                object_paths(child, &p, out);
            }
        }
    }

    /// Write `value` at `ptr` in `root`, creating missing objects on the way.
    fn set_at(root: &mut Value, ptr: &Pointer, value: Value) {
        let last = ptr
            .segments()
            .last()
            .expect("leaf pointers are never the root")
            .clone();
        let parent = ptr.parent().expect("leaf pointers are never the root");
        let slot = parent
            .ensure_object_path(root)
            .expect("no leaf pointer is a prefix of another, so every step is an object");
        if let Value::Object(map) = slot {
            map.insert(last, value);
        }
    }

    /// Spread the universe's leaves over `extra_frags + 1` fragments, with
    /// the catch all last. Every leaf path has at least one declarer by
    /// construction, which is exactly the invariant `route` relies on.
    fn build_fragments(case: &Case) -> Vec<Fragment> {
        let n = case.extra_frags + 1;
        let mut values: Vec<Value> = (0..n).map(|_| Value::Object(Map::new())).collect();

        let mut leaves = Vec::new();
        leaf_pointers(&case.universe, &Pointer::root(), &mut leaves);

        for (i, (ptr, val)) in leaves.iter().enumerate() {
            let seed = case.assign[i % SEEDS] as usize;
            let idx = seed % n;
            set_at(&mut values[idx], ptr, val.clone());

            // A shadowing declaration: same path, different value, strictly
            // higher fragment. `combine` must prefer this one and `split`
            // must route overwrites back to it and not to `idx`.
            if let Some(shadow) = &case.shadow[i % SEEDS]
                && shadow != val
                && idx + 1 < n
            {
                let j = idx + 1 + (seed / n) % (n - idx - 1);
                set_at(&mut values[j], ptr, shadow.clone());
            }
        }

        values
            .into_iter()
            .enumerate()
            .map(|(i, value)| {
                let name = if i + 1 == n {
                    CATCH_ALL.to_string()
                } else {
                    format!("{:02}-f{i}.json", i * 10)
                };
                Fragment {
                    path: PathBuf::from(&name),
                    name,
                    original: value.clone(),
                    value,
                    raw: None,
                    had_comments: false,
                }
            })
            .collect()
    }

    /// Mark some of the universe's array paths `replace`. Everything else
    /// defaults to `concat-dedupe`.
    fn build_strategies(case: &Case) -> StrategyTable {
        let mut leaves = Vec::new();
        leaf_pointers(&case.universe, &Pointer::root(), &mut leaves);

        let mut pairs = Vec::new();
        let mut seen_arrays = 0usize;
        for (ptr, val) in &leaves {
            if val.is_array() {
                if case.replace[seen_arrays % SEEDS] {
                    pairs.push((ptr.as_str(), "replace".to_string()));
                }
                seen_arrays += 1;
            }
        }
        StrategyTable::from_pairs(pairs).expect("generated pointers are well-formed")
    }

    fn pick(candidates: &[Pointer], which: usize) -> Option<Pointer> {
        if candidates.is_empty() {
            None
        } else {
            Some(candidates[which % candidates.len()].clone())
        }
    }

    /// Apply one edit to `doc`, recomputing candidate paths first so that
    /// each edit sees the effect of its predecessors. An edit with no
    /// applicable target is a no op rather than an error.
    fn apply_one(doc: &mut Value, edit: &EditSpec) {
        let mut paths = Vec::new();
        object_paths(doc, &Pointer::root(), &mut paths);

        match edit {
            EditSpec::Update { which, value } => {
                let Some(p) = pick(&paths, *which) else {
                    return;
                };
                if let Some(slot) = p.resolve_mut(doc) {
                    *slot = value.clone();
                }
            }
            EditSpec::Delete { which } => {
                let Some(p) = pick(&paths, *which) else {
                    return;
                };
                p.remove_from(doc);
            }
            EditSpec::InsertNew { which, key, value } => {
                let mut objects: Vec<Pointer> = vec![Pointer::root()];
                objects.extend(
                    paths
                        .iter()
                        .filter(|p| matches!(p.resolve(doc), Some(Value::Object(_))))
                        .cloned(),
                );
                let Some(p) = pick(&objects, *which) else {
                    return;
                };
                if let Some(Value::Object(map)) = p.resolve_mut(doc)
                    && !map.contains_key(key)
                {
                    map.insert(key.clone(), value.clone());
                }
            }
            EditSpec::ArrayAdd { which, pos, value } => {
                let arrays: Vec<Pointer> = paths
                    .iter()
                    .filter(|p| matches!(p.resolve(doc), Some(Value::Array(_))))
                    .cloned()
                    .collect();
                let Some(p) = pick(&arrays, *which) else {
                    return;
                };
                if let Some(Value::Array(items)) = p.resolve_mut(doc)
                    // Set semantics: an element already present cannot be
                    // added twice and would not survive `combine`.
                    && !items.contains(value)
                {
                    let at = pos % (items.len() + 1);
                    items.insert(at, value.clone());
                }
            }
            EditSpec::ArrayRemove { which, elem } => {
                let arrays: Vec<Pointer> = paths
                    .iter()
                    .filter(|p| matches!(p.resolve(doc), Some(Value::Array(_))))
                    .cloned()
                    .collect();
                let Some(p) = pick(&arrays, *which) else {
                    return;
                };
                if let Some(Value::Array(items)) = p.resolve_mut(doc)
                    && !items.is_empty()
                {
                    let at = elem % items.len();
                    items.remove(at);
                }
            }
            EditSpec::TypeFlip { which, key, scalar } => {
                let Some(p) = pick(&paths, *which) else {
                    return;
                };
                let Some(slot) = p.resolve_mut(doc) else {
                    return;
                };
                match slot {
                    Value::Object(_) => *slot = scalar.clone(),
                    // Arrays are left alone: scalar to array is not one of the
                    // two directions this edit models.
                    Value::Array(_) => {}
                    _ => {
                        let mut map = Map::new();
                        map.insert(key.clone(), scalar.clone());
                        *slot = Value::Object(map);
                    }
                }
            }
        }
    }

    fn apply_edits(base: &Value, edits: &[EditSpec]) -> Value {
        let mut doc = base.clone();
        for edit in edits {
            apply_one(&mut doc, edit);
        }
        doc
    }

    fn show(ptr: &Pointer) -> String {
        if ptr.is_root() {
            "<root>".to_string()
        } else {
            ptr.as_str()
        }
    }

    /// Law B's comparison. Objects compare key wise and order insensitively
    /// (`IndexMap`'s `PartialEq` already ignores key order). Arrays at
    /// `concat-dedupe` paths compare as **multisets**, because `combine`
    /// reemits in fragment order and ordering is not round trippable once
    /// elements are spread across fragments. Arrays at `replace` paths
    /// compare exactly, because there ordering *is* preserved.
    ///
    /// The error carries the failing pointer: a bare "values differ" on a
    /// document this deep is unusable when reading a shrunk counterexample.
    fn eq_up_to_array_order(
        a: &Value,
        b: &Value,
        strategies: &StrategyTable,
        ptr: &Pointer,
    ) -> std::result::Result<(), String> {
        match (a, b) {
            (Value::Object(am), Value::Object(bm)) => {
                for (k, av) in am {
                    match bm.get(k) {
                        Some(bv) => eq_up_to_array_order(av, bv, strategies, &ptr.child(k))?,
                        None => {
                            return Err(format!(
                                "{}: key {k:?} on the left only",
                                show(&ptr.child(k))
                            ));
                        }
                    }
                }
                for k in bm.keys() {
                    if !am.contains_key(k) {
                        return Err(format!(
                            "{}: key {k:?} on the right only",
                            show(&ptr.child(k))
                        ));
                    }
                }
                Ok(())
            }
            (Value::Array(aa), Value::Array(ba)) => match strategies.lookup(ptr) {
                ArrayStrategy::Replace => {
                    if aa == ba {
                        Ok(())
                    } else {
                        Err(format!(
                            "{}: replace array differs: {} != {}",
                            show(ptr),
                            key_of(a),
                            key_of(b)
                        ))
                    }
                }
                ArrayStrategy::ConcatDedupe => {
                    let mut ak: Vec<String> = aa.iter().map(key_of).collect();
                    let mut bk: Vec<String> = ba.iter().map(key_of).collect();
                    ak.sort();
                    bk.sort();
                    if ak == bk {
                        Ok(())
                    } else {
                        Err(format!(
                            "{}: concat-dedupe array differs as a multiset: {} != {}",
                            show(ptr),
                            key_of(a),
                            key_of(b)
                        ))
                    }
                }
            },
            _ => {
                if a == b {
                    Ok(())
                } else {
                    Err(format!("{}: {} != {}", show(ptr), key_of(a), key_of(b)))
                }
            }
        }
    }

    fn make_set(fragments: Vec<Fragment>) -> FragmentSet {
        FragmentSet {
            // Never touched: the value level laws call `split_values`, which
            // reads and writes no files at all.
            dir: PathBuf::from("/nonexistent"),
            fragments,
            catch_all: CATCH_ALL.to_string(),
        }
    }

    fn fragment_values(set: &FragmentSet) -> Vec<(String, Value)> {
        set.fragments
            .iter()
            .map(|f| (f.name.clone(), f.value.clone()))
            .collect()
    }

    /// Recursively reverse object key order. Used only to make the on disk
    /// fragments in law A's filesystem case differ from anything silktouch
    /// would write out itself.
    fn reverse_keys(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut kvs: Vec<(&String, &Value)> = map.iter().collect();
                kvs.reverse();
                let mut out = Map::new();
                for (k, child) in kvs {
                    out.insert(k.clone(), reverse_keys(child));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(reverse_keys).collect()),
            other => other.clone(),
        }
    }

    fn listing_of(dir: &Path) -> BTreeSet<String> {
        fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect()
    }

    // ------------------------------------------------------------- the laws

    proptest! {
        #![proptest_config(ProptestConfig { cases: 256, ..ProptestConfig::default() })]

        /// **Law A (inverse), value level.** `split(F, combine(F)) == F`.
        ///
        /// Both halves are asserted: the change list is empty *and* no
        /// fragment's value moved. The second is not implied by the first.
        /// A router that acted on an empty change list would still be caught.
        #[test]
        fn law_a_inverse_values(case in arb_case()) {
            let fragments = build_fragments(&case);
            let strategies = build_strategies(&case);
            let before: Vec<(String, Value)> = fragments
                .iter()
                .map(|f| (f.name.clone(), f.value.clone()))
                .collect();

            let base = combine_values(&fragments, &strategies);
            let mut set = make_set(fragments);
            let changes = split_values(&mut set, &base, &strategies)
                .unwrap_or_else(|e| panic!("split_values failed: {e}"));

            prop_assert!(
                changes.is_empty(),
                "split(F, combine(F)) classified {} change(s): {:?}",
                changes.len(),
                changes
            );
            prop_assert_eq!(fragment_values(&set), before, "fragment values moved");
        }

        /// **Law B (absorption).** `combine(split(F, O)) == O`, up to array
        /// element order under `concat-dedupe` (a documented, intended
        /// limitation), since `combine` emits elements in fragment order.
        #[test]
        fn law_b_absorption(case in arb_case()) {
            let fragments = build_fragments(&case);
            let strategies = build_strategies(&case);

            let base = combine_values(&fragments, &strategies);
            let output = apply_edits(&base, &case.edits);

            let mut set = make_set(fragments);
            split_values(&mut set, &output, &strategies)
                .unwrap_or_else(|e| panic!("split_values failed: {e}"));

            let recombined = combine_values(&set.fragments, &strategies);
            let cmp = eq_up_to_array_order(&recombined, &output, &strategies, &Pointer::root());
            prop_assert!(
                cmp.is_ok(),
                "combine(split(F, O)) != O at {}",
                cmp.unwrap_err()
            );
        }

        /// **Law C (idempotence).** `split(split(F, O), O) == split(F, O)`.
        ///
        /// Asserted on **fragment state**, which is the claim that matters:
        /// a second pass must not move anything. The stronger claim that the
        /// second pass classifies nothing at all is asserted too. It holds,
        /// and it is what lets `silktouch diff` exit 0 on a synchronised set.
        #[test]
        fn law_c_idempotence(case in arb_case()) {
            let fragments = build_fragments(&case);
            let strategies = build_strategies(&case);

            let base = combine_values(&fragments, &strategies);
            let output = apply_edits(&base, &case.edits);

            let mut set = make_set(fragments);
            let first = split_values(&mut set, &output, &strategies)
                .unwrap_or_else(|e| panic!("first split_values failed: {e}"));
            let after_first = fragment_values(&set);

            let second = split_values(&mut set, &output, &strategies)
                .unwrap_or_else(|e| panic!("second split_values failed: {e}"));
            let after_second = fragment_values(&set);

            prop_assert_eq!(
                &after_second,
                &after_first,
                "a second split moved fragment state"
            );
            prop_assert!(
                second.is_empty(),
                "second split classified {:?} (first pass: {:?})",
                second,
                first
            );
        }
    }

    proptest! {
        // The filesystem cases are two orders of magnitude slower than the
        // value level ones (a TempDir, N fragment files and an output file
        // per case), and they test one specific claim rather than the whole
        // relation, so they get their own, smaller budget.
        #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

        /// **Law A (inverse), byte level.** The claim that can only be seen
        /// on disk.
        ///
        /// The fragments are written with deliberately non canonical
        /// formatting (4 space indent, no trailing newline, object keys in
        /// reverse order) so that reserialising them would produce
        /// *different bytes* for the *same value*. A byte based no op check
        /// would therefore rewrite every fragment. Only a value based one
        /// leaves them alone. `written` must be empty, every file must be
        /// byte identical, and no new file (a spurious catch all) may appear.
        #[test]
        fn law_a_inverse_on_disk(case in arb_case()) {
            let fragments = build_fragments(&case);
            let strategies = build_strategies(&case);

            let dir = TempDir::new().expect("tempdir");
            let noncanonical = WriteOpts { indent: 4, trailing_newline: false };
            for f in &fragments {
                let text = Json
                    .serialize(&reverse_keys(&f.value), &noncanonical)
                    .expect("serialize");
                fs::write(dir.path().join(&f.name), text).expect("write fragment");
            }

            let out_dir = TempDir::new().expect("tempdir");
            let output = out_dir.path().join("output.json");
            let cfg = Config {
                fragments: dir.path().to_path_buf(),
                output: OutputTarget::Path(output),
                strategies,
                ..Config::default()
            };
            combine(&cfg).expect("combine");

            let listing_before = listing_of(dir.path());
            let before: Vec<(PathBuf, Vec<u8>)> = fragments
                .iter()
                .map(|f| {
                    let p = dir.path().join(&f.name);
                    let bytes = fs::read(&p).expect("read fragment");
                    (p, bytes)
                })
                .collect();

            // Guard against this test quietly going vacuous: it only proves
            // anything while the on disk bytes really do differ from what
            // silktouch would write for the same value.
            let canonical = WriteOpts::default();
            for (path, bytes) in &before {
                let on_disk = String::from_utf8(bytes.clone()).expect("utf-8");
                let value = Json
                    .parse(&on_disk, path, &ReadOpts::default())
                    .expect("parse fragment");
                let would_write = Json.serialize(&value, &canonical).expect("serialize");
                prop_assert_ne!(
                    &would_write,
                    &on_disk,
                    "fragment {:?} is already in canonical form, so this case proves nothing",
                    path
                );
            }

            let outcome = split(&cfg).expect("split");

            prop_assert!(
                outcome.changes.is_empty(),
                "split after an unedited combine classified {:?}",
                outcome.changes
            );
            prop_assert!(
                outcome.written.is_empty(),
                "split rewrote {:?} despite nothing having changed",
                outcome.written
            );
            for (path, bytes) in &before {
                let after = fs::read(path).expect("read fragment again");
                prop_assert_eq!(
                    &after,
                    bytes,
                    "fragment {:?} is not byte-identical",
                    path
                );
            }
            prop_assert_eq!(
                listing_of(dir.path()),
                listing_before,
                "split created or removed a fragment file"
            );
        }
    }

    /// Guards the laws against quietly going vacuous on the shape that used
    /// to break them: an array **directly inside** an array.
    ///
    /// This was excluded from the generators for as long as
    /// `delta::classify` hoisted an op to the *innermost* enclosing array,
    /// which for `{"b": [[]]}` is `/b/0`, an indexed path `route` cannot
    /// file. Hoisting to the outermost array fixed it and the exclusion was
    /// lifted. If anyone narrows `arb_array_element` back down, the laws
    /// would still pass while no longer covering the case, so assert the
    /// coverage directly.
    #[test]
    fn the_generator_really_produces_arrays_inside_arrays() {
        use proptest::strategy::ValueTree;

        fn has_array_in_array(v: &Value) -> bool {
            match v {
                Value::Array(items) => {
                    items.iter().any(serde_json::Value::is_array)
                        || items.iter().any(has_array_in_array)
                }
                Value::Object(map) => map.values().any(has_array_in_array),
                _ => false,
            }
        }

        let mut runner = proptest::test_runner::TestRunner::deterministic();
        let strategy = arb_universe();
        let found = (0..2000).any(|_| {
            let tree = strategy.new_tree(&mut runner).expect("generate a universe");
            has_array_in_array(&tree.current())
        });
        assert!(
            found,
            "arb_universe never generated an array inside an array"
        );
    }
}
