//! Placement: getting the combined output file to wherever the program that
//! reads it actually looks: the `target` half of a `[sets.NAME]` entry,
//! independent of `output` (the file silktouch itself writes).
//!
//! One idempotent rule, three outcomes:
//!
//! - `target` absent: create parent directories, create an absolute
//!   symlink to `output`.
//! - `target` is already exactly that symlink: no op.
//! - anything else (a regular file, a directory, a symlink pointing
//!   elsewhere): [`Error::PlacementConflict`], naming `target`, and change
//!   nothing.
//!
//! `target` is never unlinked or overwritten: no `--force`, no `--adopt`,
//! no relative link mode. The shell already has `ln -sf` and `mv`.

use std::fs;
use std::path::{Path, PathBuf};

use crate::Placement;
use crate::error::{Error, Result};

/// The state of a `placement = "symlink"` target, as reported by [`status`]
/// (never mutates). Used by `diff` to surface placement drift without ever
/// touching the filesystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementStatus {
    /// `placement = "none"`: there is no target to check.
    NotApplicable,
    /// `target` is already exactly the symlink it should be.
    Satisfied,
    /// `target` does not exist yet.
    Missing,
    /// `target` exists but is not the expected symlink: a regular file, a
    /// directory, or a symlink pointing elsewhere.
    Conflict { path: PathBuf },
}

/// What's at `target` relative to the symlink it should be.
enum Found {
    Absent,
    CorrectSymlink,
    Other,
}

/// Absolutize `path` against the current working directory. Placement
/// symlinks are always created absolute, never relative, so the link keeps
/// resolving no matter which directory it is read from.
fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let cwd = std::env::current_dir().map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(cwd.join(path))
}

/// Inspect `target` without following it, so a dangling symlink is detected
/// as a symlink (and compared by its stored destination) rather than as
/// "absent".
fn inspect(target: &Path, expected: &Path) -> Result<Found> {
    match fs::symlink_metadata(target) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                let dest = fs::read_link(target).map_err(|source| Error::Io {
                    path: target.to_path_buf(),
                    source,
                })?;
                if dest == expected {
                    Ok(Found::CorrectSymlink)
                } else {
                    Ok(Found::Other)
                }
            } else {
                Ok(Found::Other)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Found::Absent),
        Err(source) => Err(Error::Io {
            path: target.to_path_buf(),
            source,
        }),
    }
}

#[cfg(unix)]
fn create_symlink(dest: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(dest, link).map_err(|source| Error::Io {
        path: link.to_path_buf(),
        source,
    })
}

/// Windows symlink creation needs Developer Mode or elevation. A failure
/// there maps to [`Error::SymlinkUnsupported`], whose message already
/// points at `placement = "none"` with `output` set directly to the target
/// path.
#[cfg(windows)]
fn create_symlink(dest: &Path, link: &Path) -> Result<()> {
    std::os::windows::fs::symlink_file(dest, link).map_err(|source| Error::SymlinkUnsupported {
        target: link.to_path_buf(),
        source,
    })
}

/// Apply `placement` for a combined document written at `output`.
///
/// `Ok(None)` for [`Placement::None`], and for [`Placement::Symlink`] when
/// `target` is already exactly the right symlink (a true no op: nothing is
/// touched, not even an mtime). `Ok(Some(target))` when a symlink was
/// created. [`Error::PlacementConflict`] when something else is at
/// `target`. In that case nothing is changed.
pub fn apply(placement: &Placement, output: &Path) -> Result<Option<PathBuf>> {
    let target = match placement {
        Placement::None => return Ok(None),
        Placement::Symlink { target } => target,
    };
    let expected = absolute(output)?;

    match inspect(target, &expected)? {
        Found::CorrectSymlink => Ok(None),
        Found::Absent => {
            if let Some(parent) = target.parent()
                && !parent.as_os_str().is_empty()
            {
                fs::create_dir_all(parent).map_err(|source| Error::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
            create_symlink(&expected, target)?;
            Ok(Some(target.clone()))
        }
        Found::Other => Err(Error::PlacementConflict {
            path: target.clone(),
            output: output.to_path_buf(),
        }),
    }
}

/// Report placement drift for `diff`, without ever writing anything.
pub fn status(placement: &Placement, output: &Path) -> Result<PlacementStatus> {
    let target = match placement {
        Placement::None => return Ok(PlacementStatus::NotApplicable),
        Placement::Symlink { target } => target,
    };
    let expected = absolute(output)?;

    match inspect(target, &expected)? {
        Found::CorrectSymlink => Ok(PlacementStatus::Satisfied),
        Found::Absent => Ok(PlacementStatus::Missing),
        Found::Other => Ok(PlacementStatus::Conflict {
            path: target.clone(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn symlink_placement(target: &Path) -> Placement {
        Placement::Symlink {
            target: target.to_path_buf(),
        }
    }

    #[test]
    fn none_is_always_not_applicable_and_a_noop() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();

        assert_eq!(apply(&Placement::None, &output).unwrap(), None);
        assert_eq!(
            status(&Placement::None, &output).unwrap(),
            PlacementStatus::NotApplicable
        );
    }

    #[test]
    fn absent_target_gets_an_absolute_symlink_and_parents_created() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();
        let target = dir.path().join("nested").join("deep").join("target.json");
        assert!(!target.parent().unwrap().exists());

        let placement = symlink_placement(&target);
        let created = apply(&placement, &output).unwrap();
        assert_eq!(created, Some(target.clone()));

        let meta = fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_symlink());
        #[cfg(unix)]
        {
            let dest = fs::read_link(&target).unwrap();
            assert_eq!(dest, output);
            assert!(dest.is_absolute());
        }

        assert_eq!(
            status(&placement, &output).unwrap(),
            PlacementStatus::Satisfied
        );
    }

    #[test]
    #[cfg(unix)]
    fn already_correct_symlink_is_a_noop() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();
        let target = dir.path().join("target.json");

        let placement = symlink_placement(&target);
        apply(&placement, &output).unwrap();
        let mtime_before = fs::symlink_metadata(&target).unwrap().modified().unwrap();

        let second = apply(&placement, &output).unwrap();
        assert_eq!(second, None, "already-correct symlink must be a no-op");

        let mtime_after = fs::symlink_metadata(&target).unwrap().modified().unwrap();
        assert_eq!(mtime_before, mtime_after);
    }

    #[test]
    fn regular_file_at_target_is_a_conflict_and_untouched() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();
        let target = dir.path().join("target.json");
        fs::write(&target, "not a symlink, leave me alone").unwrap();

        let placement = symlink_placement(&target);
        let err = apply(&placement, &output).expect_err("must conflict");
        match &err {
            Error::PlacementConflict { path, output: out } => {
                assert_eq!(path, &target);
                assert_eq!(out, &output);
            }
            other => panic!("expected PlacementConflict, got {other:?}"),
        }

        let contents = fs::read_to_string(&target).unwrap();
        assert_eq!(contents, "not a symlink, leave me alone");

        assert_eq!(
            status(&placement, &output).unwrap(),
            PlacementStatus::Conflict {
                path: target.clone()
            }
        );
    }

    #[test]
    #[cfg(unix)]
    fn symlink_pointing_elsewhere_is_a_conflict() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();
        let elsewhere = dir.path().join("elsewhere.json");
        fs::write(&elsewhere, "{}").unwrap();
        let target = dir.path().join("target.json");
        std::os::unix::fs::symlink(&elsewhere, &target).unwrap();

        let placement = symlink_placement(&target);
        let err = apply(&placement, &output).expect_err("must conflict");
        assert!(matches!(err, Error::PlacementConflict { .. }));

        // Untouched: still points at `elsewhere`, not `output`.
        let dest = fs::read_link(&target).unwrap();
        assert_eq!(dest, elsewhere);

        assert_eq!(
            status(&placement, &output).unwrap(),
            PlacementStatus::Conflict {
                path: target.clone()
            }
        );
    }

    #[test]
    fn status_never_writes() {
        let dir = TempDir::new().unwrap();
        let output = dir.path().join("output.json");
        fs::write(&output, "{}").unwrap();
        let target = dir.path().join("nested").join("target.json");

        let placement = symlink_placement(&target);
        assert_eq!(
            status(&placement, &output).unwrap(),
            PlacementStatus::Missing
        );

        assert!(!target.exists());
        assert!(
            !target.parent().unwrap().exists(),
            "status must not create parent dirs"
        );
    }
}
