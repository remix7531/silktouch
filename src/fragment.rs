//! Fragment discovery, ordering, loading and ownership queries.
//!
//! This module owns both halves of the fragment lifecycle:
//!
//! - **load**: finding `*.json` files in a directory, sorting them by
//!   filename byte order, parsing each into a [`serde_json::Value`], and
//!   answering "which fragment owns this path" queries by scanning current
//!   fragment state.
//! - **write back**: [`write_atomic`] and [`FragmentSet::write_back`],
//!   which rewrite only the fragments that actually changed, via a
//!   same directory temp file and an atomic rename.
//!
//! The `original` and `raw` fields on [`Fragment`] exist to make the
//! write back half a no op when nothing changed. See
//! [`FragmentSet::write_back`] for the exact skip rule and why its order
//! matters.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use tempfile::NamedTempFile;

use serde_json::{Map, Value};

use crate::error::{Error, Result};
use crate::format::{Comments, Format, ReadOpts, WriteOpts};
use crate::pointer::Pointer;

/// One loaded (or synthesised) fragment file.
#[derive(Debug, Clone)]
pub struct Fragment {
    /// Path to the fragment on disk (may not yet exist for a synthesised
    /// catch all).
    pub path: PathBuf,
    /// File name only: this is the sort key used to order fragments and to
    /// resolve the catch all.
    pub name: String,
    /// The current value. Later waves mutate this in place as they route
    /// deltas back to fragments.
    pub value: Value,
    /// The value exactly as loaded (or, for a synthesised fragment, the
    /// initial empty object). Never mutated after construction.
    pub(crate) original: Value,
    /// The exact bytes read from disk. `None` when the fragment was
    /// synthesised in memory (e.g. an absent catch all) rather than loaded.
    ///
    /// [`FragmentSet::write_back`] uses this as a second chance guard: a
    /// fragment whose new serialisation is byte identical to what is already
    /// on disk is not rewritten.
    pub(crate) raw: Option<String>,
    /// Whether the loaded text held comments this format's dialect
    /// recognises (see [`Format::has_comments`]). Always `false` for a
    /// synthesised fragment (nothing was loaded). Consulted only by
    /// [`FragmentSet::write_back`] under [`Comments::Protect`]: a fragment
    /// that is never rewritten keeps its comments regardless of this flag,
    /// by construction (its bytes on disk never change).
    pub(crate) had_comments: bool,
}

impl Fragment {
    /// Whether `value` has diverged from `original` since load.
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        self.value != self.original
    }
}

/// A directory's worth of fragments, loaded and name sorted.
#[derive(Debug, Clone)]
pub struct FragmentSet {
    /// The fragment directory.
    pub dir: PathBuf,
    /// Fragments sorted by [`Fragment::name`] as raw bytes, ascending.
    pub fragments: Vec<Fragment>,
    /// The configured catch all file name (not necessarily present on disk
    /// or in `fragments`).
    pub catch_all: String,
}

impl FragmentSet {
    /// Discover and load every `*.json` fragment in `dir`.
    ///
    /// Names starting with `.` and names that do not end in `.json` are
    /// skipped, as are subdirectories. Fragments are sorted by name as raw
    /// bytes (not natural/numeric order). A missing directory is always an
    /// error. An empty result is an error unless `allow_empty` is set.
    /// `comments` governs whether a comment in a fragment is accepted at all
    /// (see [`Comments`]). When it is, `Fragment::had_comments` records
    /// whether it actually had one.
    pub fn load(
        dir: &Path,
        fmt: &dyn Format,
        catch_all: &str,
        allow_empty: bool,
        comments: Comments,
    ) -> Result<Self> {
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::FragmentDirMissing(dir.to_path_buf()));
            }
            Err(source) => {
                return Err(Error::Io {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        };

        let mut fragments = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| Error::Io {
                path: dir.to_path_buf(),
                source,
            })?;
            let path = entry.path();

            let file_type = entry.file_type().map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            if file_type.is_dir() {
                continue;
            }

            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| Error::NonUtf8Name { path: path.clone() })?;

            // Case sensitive on purpose: fragment names are compared as raw
            // bytes throughout this module (see the module doc), not
            // case folded, so `.JSON` is deliberately not a fragment.
            #[allow(clippy::case_sensitive_file_extension_comparisons)]
            if name.starts_with('.') || !name.ends_with(".json") {
                continue;
            }

            let raw = fs::read_to_string(&path).map_err(|source| Error::Io {
                path: path.clone(),
                source,
            })?;
            let value = fmt.parse(&raw, &path, &ReadOpts { comments })?;
            if !value.is_object() {
                return Err(Error::FragmentRootNotObject {
                    path,
                    found: type_name(&value),
                });
            }
            let original = value.clone();
            let had_comments = fmt.has_comments(&raw);

            fragments.push(Fragment {
                path,
                name,
                value,
                original,
                raw: Some(raw),
                had_comments,
            });
        }

        if fragments.is_empty() && !allow_empty {
            return Err(Error::NoFragments(dir.to_path_buf()));
        }

        fragments.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));

        Ok(FragmentSet {
            dir: dir.to_path_buf(),
            fragments,
            catch_all: catch_all.to_string(),
        })
    }

    /// Verify that no discovered fragment name sorts strictly after
    /// `catch_all`, comparing names as raw bytes.
    ///
    /// The catch all is compared purely as a name. This holds even when no
    /// file by that name exists on disk yet.
    pub fn check_catch_all_sorts_last(&self) -> Result<()> {
        let catch_all_bytes = self.catch_all.as_bytes();
        for f in &self.fragments {
            if f.name.as_bytes() > catch_all_bytes {
                return Err(Error::CatchAllNotLast {
                    catch_all: self.catch_all.clone(),
                    after: f.name.clone(),
                });
            }
        }
        Ok(())
    }

    /// The index of the highest precedence (last) fragment whose current
    /// `value` declares `p`. The root pointer is declared by every
    /// fragment, so this is the last fragment overall when any exist.
    #[must_use]
    pub fn owner_of(&self, p: &Pointer) -> Option<usize> {
        self.fragments
            .iter()
            .rposition(|f| p.resolve(&f.value).is_some())
    }

    /// Every fragment index (ascending) whose current `value` declares `p`.
    #[must_use]
    pub fn declarers_of(&self, p: &Pointer) -> Vec<usize> {
        self.fragments
            .iter()
            .enumerate()
            .filter(|(_, f)| p.resolve(&f.value).is_some())
            .map(|(i, _)| i)
            .collect()
    }

    /// The index of the catch all fragment, synthesising an empty one in
    /// memory (keeping the vector name sorted) if none is present.
    ///
    /// A freshly synthesised catch all is not dirty: `original` and `value`
    /// both start as an empty object, so it is only written back once
    /// something is actually routed into it.
    pub fn catch_all_index(&mut self) -> usize {
        if let Some(i) = self.fragments.iter().position(|f| f.name == self.catch_all) {
            return i;
        }

        let value = Value::Object(Map::new());
        let fragment = Fragment {
            path: self.dir.join(&self.catch_all),
            name: self.catch_all.clone(),
            value: value.clone(),
            original: value,
            raw: None,
            had_comments: false,
        };

        let insert_at = self
            .fragments
            .partition_point(|f| f.name.as_bytes() < fragment.name.as_bytes());
        self.fragments.insert(insert_at, fragment);
        insert_at
    }

    /// Write every dirty fragment back to disk, returning the paths actually
    /// written (in fragment order). Clean fragments are left completely
    /// untouched, so their mtimes stay stable.
    ///
    /// # The skip rule, and why the order matters
    ///
    /// 1. **Value first.** A fragment whose `value` still equals its
    ///    `original` is skipped without ever being serialised. This check is
    ///    sufficient on its own.
    /// 2. **Comments second.** A dirty fragment that `Fragment::had_comments`
    ///    and `comments == `[`Comments::Protect`] is [`Error::CommentsWouldBeLost`]
    ///    naming the fragment's path, and nothing is written for it. This is
    ///    the write back boundary the comments setting exists for: a
    ///    fragment `split`/`route` merely *reads*, or leaves untouched, never
    ///    reaches this check at all (rejected by rule 1 above), so one
    ///    comment anywhere can never wedge an otherwise unrelated run.
    ///    [`Comments::Strip`] skips this check and silently discards the
    ///    comments. [`Comments::Forbid`] never has a commented fragment to
    ///    begin with, since [`Format::parse`] already rejected it at load.
    /// 3. **Bytes third.** Only for a fragment that *did* change (and, if
    ///    commented, was allowed past rule 2) is the serialisation compared
    ///    against `Fragment::raw`. If they are equal the write is skipped
    ///    too.
    ///
    /// Reversing (1) and (3), serialising first and skipping only on a byte
    /// mismatch, would silently reformat hand authored files: a fragment
    /// indented with four spaces that nobody edited would reserialise to two,
    /// compare unequal, and be rewritten. That breaks law A
    /// (`split(F, combine(F)) == F`, byte identical) and the rule that a
    /// successful run which changed nothing prints nothing.
    pub fn write_back(
        &self,
        fmt: &dyn Format,
        opts: &WriteOpts,
        comments: Comments,
    ) -> Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for frag in &self.fragments {
            // (1) Value based check, first and sufficient.
            if !frag.is_dirty() {
                continue;
            }
            // (2) A rewrite that would drop comments, under Protect. Fires
            // before serialising: a commented fragment's raw text always
            // differs from its canonical serialisation (the comment bytes
            // themselves are gone), so this fragment was always going to be
            // written: there is no byte identical escape hatch to check
            // first.
            if comments == Comments::Protect && frag.had_comments {
                return Err(Error::CommentsWouldBeLost {
                    path: frag.path.clone(),
                });
            }
            let text = fmt.serialize(&frag.value, opts)?;
            // (3) Byte based second chance: the value moved but the file
            // would not.
            if frag.raw.as_deref() == Some(text.as_str()) {
                continue;
            }
            if write_atomic(&frag.path, &text)? {
                // Report the fragment's own path, not the symlink resolved
                // one: that is the name the caller knows it by.
                written.push(frag.path.clone());
            }
        }
        Ok(written)
    }
}

/// The JSON type name of `v`, for use in [`Error::FragmentRootNotObject`].
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

/// How many symlink hops [`resolve_destination`] will follow before giving
/// up, matching the spirit of `ELOOP`.
const MAX_LINK_HOPS: u32 = 32;

/// Resolve `path` to the real file the write should land on.
///
/// Renaming over a symlink *replaces* it with a regular file, which would
/// break the documented GNU Stow workflow where the output file is a
/// stow placed symlink and the whole design depends on writing *through* it.
/// So symlinks are followed here, and the resolved path is used both as the
/// rename target and to choose the temp directory (keeping the rename
/// same filesystem).
///
/// A destination that does not exist yet cannot be canonicalised, so its
/// parent is canonicalised instead and the file name joined back on.
fn resolve_destination(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINK_HOPS {
        match fs::canonicalize(&current) {
            Ok(real) => return Ok(real),
            // Not found only means the *final* component is missing, or that
            // we are looking at a dangling symlink. Both are handled below.
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }

        // A dangling symlink: follow one hop by hand so we still write
        // through it rather than clobbering the link itself.
        if let Ok(meta) = fs::symlink_metadata(&current)
            && meta.file_type().is_symlink()
        {
            let target = fs::read_link(&current).map_err(|source| Error::Io {
                path: current.clone(),
                source,
            })?;
            current = if target.is_absolute() {
                target
            } else {
                parent_of(&current).join(target)
            };
            continue;
        }

        // A plain not yet existing file: canonicalise the parent instead.
        let name = current.file_name().ok_or_else(|| Error::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "path has no file name"),
        })?;
        let parent = fs::canonicalize(parent_of(&current)).map_err(|source| Error::Io {
            path: path.to_path_buf(),
            source,
        })?;
        return Ok(parent.join(name));
    }

    Err(Error::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "too many levels of symbolic links",
        ),
    })
}

/// The parent of `path`, treating a bare file name as living in `.`.
fn parent_of(path: &Path) -> &Path {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    }
}

/// Atomically write `text` to `path`, returning whether a write happened.
///
/// Returns `Ok(false)` when the file already contains exactly `text`: the
/// write is skipped and the mtime left alone.
///
/// The write goes to a [`NamedTempFile`] in the destination's own directory
/// and is then `persist`ed, so the rename is same filesystem and atomic:
/// readers see either the old file or the new one, never a partial write.
/// Two consequences of renaming *over* the destination are handled
/// explicitly:
///
/// - **Symlinks** are resolved first (see `resolve_destination`), so a
///   symlinked destination is still a symlink afterwards and the new content
///   lands at the link target.
/// - **Permissions** of an existing destination are copied onto the temp
///   file before persisting. Otherwise the rename would silently narrow the
///   file to `NamedTempFile`'s 0600.
///
/// Creating parent directories is deliberately not done here: fragments
/// live in a directory that already exists. But a missing parent yields
/// [`Error::Io`] naming the path rather than a panic.
pub fn write_atomic(path: &Path, text: &str) -> Result<bool> {
    let real = resolve_destination(path)?;

    // Byte identical content: nothing to do, and the mtime stays put.
    if let Ok(existing) = fs::read_to_string(&real)
        && existing == text
    {
        return Ok(false);
    }

    let dir = parent_of(&real);
    let mut tmp = NamedTempFile::new_in(dir).map_err(|source| Error::Io {
        path: real.clone(),
        source,
    })?;

    tmp.write_all(text.as_bytes()).map_err(|source| Error::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;
    tmp.flush().map_err(|source| Error::Io {
        path: tmp.path().to_path_buf(),
        source,
    })?;

    // Preserve the destination's permissions. NamedTempFile is 0600 and a
    // rename would otherwise carry that over the existing file.
    if let Ok(meta) = fs::metadata(&real) {
        tmp.as_file()
            .set_permissions(meta.permissions())
            .map_err(|source| Error::Io {
                path: tmp.path().to_path_buf(),
                source,
            })?;
    }

    tmp.persist(&real).map_err(|source| Error::Io {
        path: real.clone(),
        source: source.error,
    })?;

    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::Json;
    use tempfile::TempDir;

    fn json() -> &'static dyn Format {
        &Json
    }

    fn write(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).expect("write fragment");
    }

    fn p(s: &str) -> Pointer {
        Pointer::parse(s).expect("valid pointer")
    }

    // ---- discovery / filtering ------------------------------------------

    #[test]
    fn json_extension_filter() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.json", "{}");
        write(dir.path(), "b.txt", "not json");
        write(dir.path(), "c.json.bak", "{}");

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let names: Vec<&str> = set.fragments.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["a.json"]);
    }

    #[test]
    fn dot_prefixed_names_skipped() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), ".hidden.json", "{}");
        write(dir.path(), "visible.json", "{}");

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let names: Vec<&str> = set.fragments.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["visible.json"]);
    }

    #[test]
    fn sorted_by_byte_order_not_natural_order() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-a.json", "{}");
        write(dir.path(), "9-b.json", "{}");
        write(dir.path(), "99-c.json", "{}");

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let names: Vec<&str> = set.fragments.iter().map(|f| f.name.as_str()).collect();
        // Byte order: '1' (0x31) < '9' (0x39), so "10-a.json" sorts before
        // "9-b.json" and "99-c.json". A natural sort would put 9 before 10.
        assert_eq!(names, vec!["10-a.json", "9-b.json", "99-c.json"]);
    }

    #[test]
    fn non_object_root_errors_naming_the_file() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "bad.json", "[1,2,3]");

        let err = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap_err();
        match &err {
            Error::FragmentRootNotObject { path, found } => {
                assert_eq!(*found, "array");
                assert!(path.to_string_lossy().contains("bad.json"));
            }
            other => panic!("expected FragmentRootNotObject, got {other:?}"),
        }
        assert!(err.to_string().contains("bad.json"));
    }

    #[test]
    fn missing_directory_errors() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");

        let err = FragmentSet::load(&missing, json(), "99-local.json", false, Comments::Forbid)
            .unwrap_err();
        assert!(matches!(err, Error::FragmentDirMissing(p) if p == missing));
    }

    #[test]
    fn empty_directory_errors_unless_allow_empty() {
        let dir = TempDir::new().unwrap();

        let err = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap_err();
        assert!(matches!(err, Error::NoFragments(p) if p == dir.path()));

        let set =
            FragmentSet::load(dir.path(), json(), "99-local.json", true, Comments::Forbid).unwrap();
        assert!(set.fragments.is_empty());
    }

    // ---- ownership -------------------------------------------------------

    fn shadowed_set(dir: &TempDir) -> FragmentSet {
        write(dir.path(), "1.json", r#"{"a":1}"#);
        write(dir.path(), "2.json", r#"{"a":2}"#);
        write(dir.path(), "3.json", r#"{"a":3}"#);
        FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid).unwrap()
    }

    #[test]
    fn owner_of_shadowed_path_is_the_last_declarer() {
        let dir = TempDir::new().unwrap();
        let set = shadowed_set(&dir);
        assert_eq!(set.owner_of(&p("/a")), Some(2));
    }

    #[test]
    fn declarers_of_returns_all_ascending() {
        let dir = TempDir::new().unwrap();
        let set = shadowed_set(&dir);
        assert_eq!(set.declarers_of(&p("/a")), vec![0, 1, 2]);
    }

    #[test]
    fn owner_of_none_when_nobody_declares_it() {
        let dir = TempDir::new().unwrap();
        let set = shadowed_set(&dir);
        assert_eq!(set.owner_of(&p("/nope")), None);
        assert!(set.declarers_of(&p("/nope")).is_empty());
    }

    // ---- catch all ---------------------------------------------------------

    #[test]
    fn check_catch_all_sorts_last_passes_and_fails_naming_offender() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-x.json", "{}");
        write(dir.path(), "99-local.json", "{}");

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        set.check_catch_all_sorts_last().unwrap();

        write(dir.path(), "zz-late.json", "{}");
        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let err = set.check_catch_all_sorts_last().unwrap_err();
        match err {
            Error::CatchAllNotLast { catch_all, after } => {
                assert_eq!(catch_all, "99-local.json");
                assert_eq!(after, "zz-late.json");
            }
            other => panic!("expected CatchAllNotLast, got {other:?}"),
        }
    }

    #[test]
    fn check_catch_all_sorts_last_works_when_catch_all_absent_from_disk() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-x.json", "{}");
        // Note: no 99-local.json written at all.

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        set.check_catch_all_sorts_last().unwrap();
    }

    #[test]
    fn catch_all_index_finds_existing() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-x.json", "{}");
        write(dir.path(), "99-local.json", r#"{"k":"v"}"#);

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        let idx = set.catch_all_index();
        assert_eq!(set.fragments[idx].name, "99-local.json");
        assert_eq!(set.fragments[idx].value, serde_json::json!({"k": "v"}));
        assert_eq!(set.fragments.len(), 2);
    }

    #[test]
    fn catch_all_index_synthesises_when_absent_and_is_not_dirty() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-x.json", "{}");
        write(dir.path(), "zz-after.json", "{}");

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        assert_eq!(set.fragments.len(), 2);

        let idx = set.catch_all_index();

        // Inserted in name sorted position: 10-x, 99-local, zz-after.
        let names: Vec<&str> = set.fragments.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["10-x.json", "99-local.json", "zz-after.json"]);
        assert_eq!(idx, 1);

        let synthesised = &set.fragments[idx];
        assert_eq!(synthesised.value, serde_json::json!({}));
        assert_eq!(synthesised.raw, None);
        assert_eq!(synthesised.path, dir.path().join("99-local.json"));
        assert!(
            !synthesised.is_dirty(),
            "an untouched synthesised catch-all must not be dirty"
        );

        // Calling again must not insert a second copy.
        let idx2 = set.catch_all_index();
        assert_eq!(idx2, idx);
        assert_eq!(set.fragments.len(), 3);
    }

    // ---- raw / dirty on load -----------------------------------------------

    #[test]
    fn raw_holds_exact_bytes_and_is_not_dirty_after_load() {
        let dir = TempDir::new().unwrap();
        let content = "{\n  \"a\": 1\n}\n";
        write(dir.path(), "a.json", content);

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let f = &set.fragments[0];
        assert_eq!(f.raw.as_deref(), Some(content));
        assert_eq!(f.value, f.original);
        assert!(!f.is_dirty());
    }

    // ---- write back ---------------------------------------------------------

    /// An mtime far enough in the past that any rewrite is unmistakable.
    fn pin_old_mtime(path: &Path) -> std::time::SystemTime {
        let old = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(old)).unwrap();
        fs::metadata(path).unwrap().modified().unwrap()
    }

    fn mtime(path: &Path) -> std::time::SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    #[test]
    fn byte_identical_no_op_leaves_mtime_unchanged() {
        let dir = TempDir::new().unwrap();
        // Already exactly what the default WriteOpts would produce.
        write(dir.path(), "a.json", "{\n  \"a\": 1\n}\n");
        let path = dir.path().join("a.json");
        let before = pin_old_mtime(&path);

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();

        assert!(written.is_empty(), "nothing was dirty, nothing to write");
        assert_eq!(
            mtime(&path),
            before,
            "an untouched fragment must not be rewritten"
        );
    }

    /// Law A regression: a hand authored file whose *value* nobody touched is
    /// never reformatted, even when its indentation differs from `WriteOpts`.
    #[test]
    fn hand_authored_four_space_file_is_not_reformatted() {
        let dir = TempDir::new().unwrap();
        let four_space = "{\n    \"a\": 1,\n    \"b\": {\n        \"c\": 2\n    }\n}\n";
        write(dir.path(), "a.json", four_space);
        let path = dir.path().join("a.json");
        let before = pin_old_mtime(&path);

        let set = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap();
        // Default opts use 2 spaces of indentation, so serialising would
        // differ byte wise, but the value check comes first and skips before
        // we ever serialise.
        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();

        assert!(
            written.is_empty(),
            "an untouched fragment must not be rewritten"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), four_space);
        assert_eq!(mtime(&path), before);
    }

    #[test]
    fn modified_fragment_is_written_and_returned() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.json", "{\n  \"a\": 1\n}\n");
        let path = dir.path().join("a.json");

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        set.fragments[0].value = serde_json::json!({"a": 2});
        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();

        assert_eq!(written, vec![path.clone()]);
        assert_eq!(fs::read_to_string(&path).unwrap(), "{\n  \"a\": 2\n}\n");
    }

    #[test]
    fn only_dirty_fragments_are_written() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-a.json", "{\n  \"a\": 1\n}\n");
        write(dir.path(), "20-b.json", "{\n  \"b\": 1\n}\n");
        write(dir.path(), "30-c.json", "{\n  \"c\": 1\n}\n");
        let clean_a = pin_old_mtime(&dir.path().join("10-a.json"));
        let clean_c = pin_old_mtime(&dir.path().join("30-c.json"));

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        set.fragments[1].value = serde_json::json!({"b": 99});
        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();

        assert_eq!(written, vec![dir.path().join("20-b.json")]);
        assert_eq!(mtime(&dir.path().join("10-a.json")), clean_a);
        assert_eq!(mtime(&dir.path().join("30-c.json")), clean_c);
        assert_eq!(
            fs::read_to_string(dir.path().join("20-b.json")).unwrap(),
            "{\n  \"b\": 99\n}\n"
        );
    }

    #[test]
    fn untouched_synthesised_catch_all_is_not_written() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-x.json", "{}\n");

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        let idx = set.catch_all_index();
        assert_eq!(set.fragments[idx].name, "99-local.json");

        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();
        assert!(written.is_empty());
        assert!(
            !dir.path().join("99-local.json").exists(),
            "an empty catch-all nothing was routed into must not be created"
        );
    }

    #[test]
    fn no_stray_temp_files_are_left_behind() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.json", "{\n  \"a\": 1\n}\n");

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
                .unwrap();
        set.fragments[0].value = serde_json::json!({"a": 2});
        set.write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();

        // The temp file must have been created in this same directory, and
        // renamed within it, leaving nothing behind.
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, vec!["a.json".to_string()]);
    }

    #[test]
    fn write_atomic_creates_a_missing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.json");

        assert!(write_atomic(&path, "{}\n").unwrap());
        assert_eq!(fs::read_to_string(&path).unwrap(), "{}\n");
        // Second identical write is skipped.
        assert!(!write_atomic(&path, "{}\n").unwrap());
    }

    #[test]
    fn missing_parent_directory_is_an_io_error_naming_the_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("nope").join("a.json");

        let err = write_atomic(&path, "{}\n").unwrap_err();
        match &err {
            Error::Io { path: p, .. } => assert_eq!(*p, path),
            other => panic!("expected Error::Io, got {other:?}"),
        }
        assert!(err.to_string().contains("a.json"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn writing_through_a_symlink_keeps_the_symlink() {
        let real_dir = TempDir::new().unwrap();
        let link_dir = TempDir::new().unwrap();
        let real = real_dir.path().join("a.json");
        write(real_dir.path(), "a.json", "{\n  \"a\": 1\n}\n");

        let link = link_dir.path().join("a.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let mut set = FragmentSet::load(
            link_dir.path(),
            json(),
            "99-local.json",
            false,
            Comments::Forbid,
        )
        .unwrap();
        set.fragments[0].value = serde_json::json!({"a": 2});
        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Forbid)
            .unwrap();
        assert_eq!(written, vec![link.clone()]);

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the destination must still be a symlink after the write"
        );
        assert_eq!(fs::read_link(&link).unwrap(), real);
        assert_eq!(fs::read_to_string(&real).unwrap(), "{\n  \"a\": 2\n}\n");
        // And nothing was dropped in the directory holding the link.
        let names: Vec<String> = fs::read_dir(link_dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, vec!["a.json".to_string()]);
    }

    #[cfg(unix)]
    #[test]
    fn destination_permissions_are_preserved() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new().unwrap();
        write(dir.path(), "a.json", "{\n  \"a\": 1\n}\n");
        let path = dir.path().join("a.json");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        assert!(write_atomic(&path, "{\n  \"a\": 2\n}\n").unwrap());

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "expected 0644 preserved, got {mode:o} (NamedTempFile's 0600 leaked through?)"
        );
    }

    // ---- comments (JSONC) ---------------------------------------------

    #[test]
    fn forbid_mode_a_commented_fragment_is_a_parse_error_naming_the_file() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-base.json", "{\n  \"a\": 1 // note\n}\n");

        let err = FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Forbid)
            .unwrap_err();
        // A comment under Forbid is reported as a comment, not as the
        // "key must be a string" parse error serde_json would give.
        match &err {
            Error::CommentsNotEnabled { path } => {
                assert!(path.to_string_lossy().contains("10-base.json"));
            }
            other => panic!("expected Error::CommentsNotEnabled, got {other:?}"),
        }
        assert!(err.to_string().contains("10-base.json"));
    }

    #[test]
    fn protect_mode_loads_a_commented_fragment_and_records_had_comments() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-base.json", "{\n  \"a\": 1 // note\n}\n");
        write(dir.path(), "20-plain.json", "{\n  \"b\": 2\n}\n");

        let set = FragmentSet::load(
            dir.path(),
            json(),
            "99-local.json",
            false,
            Comments::Protect,
        )
        .unwrap();
        assert_eq!(set.fragments[0].value, serde_json::json!({"a": 1}));
        assert!(set.fragments[0].had_comments, "10-base.json has a comment");
        assert!(
            !set.fragments[1].had_comments,
            "20-plain.json has no comment"
        );
    }

    #[test]
    fn protect_mode_rewrite_of_a_commented_fragment_errors_and_leaves_it_on_disk_unchanged() {
        let dir = TempDir::new().unwrap();
        let original = "{\n  \"a\": 1 // note\n}\n";
        write(dir.path(), "10-base.json", original);
        let path = dir.path().join("10-base.json");

        let mut set = FragmentSet::load(
            dir.path(),
            json(),
            "99-local.json",
            false,
            Comments::Protect,
        )
        .unwrap();
        set.fragments[0].value = serde_json::json!({"a": 2});

        let err = set
            .write_back(json(), &WriteOpts::default(), Comments::Protect)
            .unwrap_err();
        match &err {
            Error::CommentsWouldBeLost { path: p } => assert_eq!(*p, path),
            other => panic!("expected Error::CommentsWouldBeLost, got {other:?}"),
        }
        assert!(err.to_string().contains("10-base.json"), "{err}");
        assert!(
            err.to_string().contains("strip"),
            "the error should suggest comments = \"strip\": {err}"
        );

        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            original,
            "a Protect error must leave the fragment's bytes untouched"
        );
    }

    #[test]
    fn strip_mode_rewrite_of_a_commented_fragment_succeeds_and_drops_the_comment() {
        let dir = TempDir::new().unwrap();
        write(dir.path(), "10-base.json", "{\n  \"a\": 1 // note\n}\n");
        let path = dir.path().join("10-base.json");

        let mut set =
            FragmentSet::load(dir.path(), json(), "99-local.json", false, Comments::Strip).unwrap();
        set.fragments[0].value = serde_json::json!({"a": 2});

        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Strip)
            .unwrap();
        assert_eq!(written, vec![path.clone()]);

        let on_disk = fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "{\n  \"a\": 2\n}\n");
        assert!(!on_disk.contains("//"), "the comment must be gone");
    }

    #[test]
    fn protect_mode_an_untouched_commented_fragment_does_not_block_other_rewrites() {
        let dir = TempDir::new().unwrap();
        let commented = "{\n  \"a\": 1 // note, never edited\n}\n";
        write(dir.path(), "10-base.json", commented);
        write(dir.path(), "20-plain.json", "{\n  \"b\": 1\n}\n");
        let commented_path = dir.path().join("10-base.json");
        let plain_path = dir.path().join("20-plain.json");

        let mut set = FragmentSet::load(
            dir.path(),
            json(),
            "99-local.json",
            false,
            Comments::Protect,
        )
        .unwrap();
        // Only the plain fragment is edited. The commented one is left as is.
        set.fragments[1].value = serde_json::json!({"b": 99});

        let written = set
            .write_back(json(), &WriteOpts::default(), Comments::Protect)
            .expect("a clean commented fragment must not block other rewrites");
        assert_eq!(written, vec![plain_path.clone()]);

        assert_eq!(
            fs::read_to_string(&commented_path).unwrap(),
            commented,
            "the untouched commented fragment must be byte-identical"
        );
        assert_eq!(
            fs::read_to_string(&plain_path).unwrap(),
            "{\n  \"b\": 99\n}\n"
        );
    }
}
