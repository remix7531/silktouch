//! Shared helpers for silktouch's CLI integration tests (`assert_cmd` +
//! `tempfile`, driving the real `silktouch` binary).
//!
//! Not itself a test binary: files under `tests/<subdir>/` are not picked up
//! by cargo as separate integration test targets, only `tests/*.rs` is, so
//! every test file does `mod common;` to pull this in.
#![allow(dead_code)] // not every test file uses every helper.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use assert_cmd::Command;
use tempfile::TempDir;

/// A fresh `silktouch` [`Command`], one per invocation: `assert_cmd`
/// doesn't support reuse across `.assert()` calls anyway.
pub fn bin() -> Command {
    Command::cargo_bin("silktouch").expect("silktouch binary is built")
}

/// The four XDG directories a registry driven run should touch (config) or
/// never touch (state/data/cache), all pointed at fresh, not yet existing
/// subdirectories of `root`.
// The shared `_home` postfix mirrors the XDG_*_HOME env var names
// (XDG_CONFIG_HOME, XDG_STATE_HOME, ...) these fields stand in for:
// stripping it would make the fields read like generic paths instead.
#[allow(clippy::struct_field_names)]
pub struct XdgPaths {
    pub config_home: PathBuf,
    pub state_home: PathBuf,
    pub data_home: PathBuf,
    pub cache_home: PathBuf,
}

impl XdgPaths {
    /// Assert none of state/data/cache exist: the stateless design means
    /// there is nothing silktouch should ever put there.
    pub fn assert_state_data_cache_absent(&self) {
        assert!(
            !self.state_home.exists(),
            "XDG_STATE_HOME must not be created: {}",
            self.state_home.display()
        );
        assert!(
            !self.data_home.exists(),
            "XDG_DATA_HOME must not be created: {}",
            self.data_home.display()
        );
        assert!(
            !self.cache_home.exists(),
            "XDG_CACHE_HOME must not be created: {}",
            self.cache_home.display()
        );
    }
}

/// Point every XDG variable at fresh subdirectories of `root`, so a test
/// never sees (or accidentally depends on) the real invoking user's XDG
/// state, and "was X created" assertions have a deterministic place to look.
/// `XDG_CONFIG_DIRS` is cleared (not inherited) so the read only fallback
/// search is exactly what each test opts into.
pub fn isolate_xdg(cmd: &mut Command, root: &Path) -> XdgPaths {
    let paths = XdgPaths {
        config_home: root.join("xdg-config"),
        state_home: root.join("xdg-state"),
        data_home: root.join("xdg-data"),
        cache_home: root.join("xdg-cache"),
    };
    cmd.env("XDG_CONFIG_HOME", &paths.config_home)
        .env("XDG_CONFIG_DIRS", "")
        .env("XDG_STATE_HOME", &paths.state_home)
        .env("XDG_DATA_HOME", &paths.data_home)
        .env("XDG_CACHE_HOME", &paths.cache_home);
    paths
}

/// Write `(name, content)` pairs as fragment files into a fresh temp dir.
pub fn fragments_dir(contents: &[(&str, &str)]) -> TempDir {
    let dir = TempDir::new().expect("tempdir");
    for (name, text) in contents {
        fs::write(dir.path().join(name), text).expect("write fragment");
    }
    dir
}

/// Sorted names of a directory's direct entries.
pub fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// mtime of one file.
pub fn mtime(path: &Path) -> SystemTime {
    fs::metadata(path)
        .unwrap_or_else(|e| panic!("metadata for {}: {e}", path.display()))
        .modified()
        .expect("mtime")
}

/// mtimes of every direct entry in `dir`, keyed by file name: for
/// before/after comparisons that must show nothing moved.
pub fn mtimes(dir: &Path) -> BTreeMap<String, SystemTime> {
    fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| {
            let e = e.expect("entry");
            let name = e.file_name().to_string_lossy().into_owned();
            let m = e.metadata().expect("metadata").modified().expect("mtime");
            (name, m)
        })
        .collect()
}

/// Force a file's mtime far into the past, so a same second rewrite in a
/// fast running test is still unmistakable when rechecked.
pub fn pin_old_mtime(path: &Path) {
    let old = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let f = fs::File::options()
        .write(true)
        .open(path)
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    f.set_times(fs::FileTimes::new().set_modified(old))
        .expect("set mtime");
}

/// Recursively copy `src`'s contents into `dst` (created if needed).
/// End to end tests use this to work on a disposable copy of
/// `tests/fixtures/...`: the fixture on disk is never mutated in place.
pub fn copy_dir_all(src: &Path, dst: &Path) {
    fs::create_dir_all(dst).expect("mkdir");
    for entry in fs::read_dir(src).expect("read_dir") {
        let entry = entry.expect("entry");
        let ty = entry.file_type().expect("file_type");
        let target = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_all(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Read a file to bytes, panicking with the path on failure: friendlier
/// failure messages than a bare `fs::read(..).unwrap()`.
pub fn read(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}
