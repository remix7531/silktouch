//! The global registry: `$XDG_CONFIG_HOME/silktouch/registry.toml` (or an
//! explicit `--config PATH`), turning a named set into a [`Config`].
//!
//! Discovery follows the XDG Base Directory specification on every
//! platform, via `etcetera`'s explicit [`base_strategy::Xdg`](etcetera::base_strategy::Xdg),
//! never `directories::ProjectDirs`, which substitutes platform native
//! paths off Linux. Nothing here writes to `$XDG_STATE_HOME`,
//! `$XDG_DATA_HOME` or `$XDG_CACHE_HOME`. Discovery only ever reads.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use etcetera::base_strategy::{BaseStrategy, Xdg};
use serde::Deserialize;

use crate::error::{Error, Result};
use crate::format::{Comments, Json};
use crate::pointer::StrategyTable;
use crate::{Config, OutputTarget, Placement};

/// The parsed `registry.toml`: global defaults plus a table of named sets.
#[derive(Debug, Deserialize)]
pub struct Registry {
    /// Global default indent. Overridden by a set's own `indent`, which is
    /// in turn overridden by `--indent`, the same precedence as
    /// `catch_all`.
    pub indent: Option<usize>,
    /// Global default catch all filename. A set's own `catch_all` wins over
    /// this, and this wins over the crate wide `99-local.json` default.
    pub catch_all: Option<String>,
    /// Global default comments mode (`"forbid"`, `"protect"` or `"strip"`).
    /// Overridden by a set's own `comments`, which is in turn overridden by
    /// `--comments`, the same precedence as `catch_all`. Unset anywhere
    /// falls back to [`Comments::Forbid`].
    pub comments: Option<String>,
    #[serde(default)]
    pub sets: BTreeMap<String, SetEntry>,
}

/// One `[sets.NAME]` table.
#[derive(Debug, Deserialize)]
pub struct SetEntry {
    /// Fragment directory. `~` and `$VAR` are expanded.
    pub fragments: String,
    /// Where `combine` writes the combined document. `~` and `$VAR` are
    /// expanded.
    pub output: String,
    /// Per set catch all filename, overriding the registry's global default.
    pub catch_all: Option<String>,
    /// Per set indent, overriding the registry's global default.
    pub indent: Option<usize>,
    /// Per set comments mode, overriding the registry's global default.
    pub comments: Option<String>,
    /// How the output file is delivered. Defaults to [`PlacementKind::None`].
    pub placement: Option<PlacementKind>,
    /// Symlink destination for `placement = "symlink"`. `~` and `$VAR` are
    /// expanded. Required when `placement` is `"symlink"`.
    pub target: Option<String>,
    /// Per pointer array merge strategy overrides, as written in `--merge`.
    #[serde(default)]
    pub merge: BTreeMap<String, String>,
}

/// How the output file is delivered to whatever reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlacementKind {
    /// silktouch touches only `output`.
    None,
    /// silktouch also ensures `target` is a symlink pointing at `output`.
    Symlink,
}

/// The environment inputs [`discover`] reads. Factored out so unit tests can
/// drive the resolution logic directly, with no process wide env var
/// mutation at all. See the `resolve_*` tests below, which construct this
/// by hand instead of touching `std::env`.
struct Env {
    config_home: PathBuf,
    config_dirs: Vec<PathBuf>,
}

impl Env {
    /// Read the real process environment via `etcetera`'s `Xdg` strategy
    /// (for `$XDG_CONFIG_HOME`'s fallback to `~/.config` behaviour) plus
    /// `$XDG_CONFIG_DIRS` directly (`etcetera` has no notion of the
    /// plural, read only search path).
    fn from_process() -> Result<Self> {
        let strategy = Xdg::new().map_err(|source| Error::Io {
            path: PathBuf::from("$HOME"),
            source: std::io::Error::other(source),
        })?;
        let config_dirs = std::env::var_os("XDG_CONFIG_DIRS")
            .map(|val| std::env::split_paths(&val).collect())
            .unwrap_or_default();
        Ok(Env {
            config_home: strategy.config_dir(),
            config_dirs,
        })
    }

    /// Registry candidates in search order: `$XDG_CONFIG_HOME` first, then
    /// each `$XDG_CONFIG_DIRS` entry in order.
    fn candidates(&self) -> Vec<PathBuf> {
        std::iter::once(self.config_home.clone())
            .chain(self.config_dirs.iter().cloned())
            .map(|dir| dir.join("silktouch").join("registry.toml"))
            .collect()
    }
}

/// Parse `path` as a registry file. A missing or unreadable file is
/// [`Error::Io`]. A present but invalid file is [`Error::RegistryParse`].
fn load_registry(path: &Path) -> Result<Registry> {
    let raw = fs::read_to_string(path).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&raw).map_err(|source| Error::RegistryParse {
        path: path.to_path_buf(),
        source,
    })
}

/// Locate and parse the registry.
///
/// `explicit` (`--config PATH`) beats discovery entirely. A nonexistent
/// explicit path is an error (surfaced via [`Error::Io`], naming the path).
/// Otherwise `$XDG_CONFIG_HOME/silktouch/registry.toml` is tried first,
/// falling back to `~/.config/...` when the variable is unset or not
/// absolute (this is `etcetera`'s `Xdg::config_dir` behaviour exactly), then
/// each `$XDG_CONFIG_DIRS` entry in order, first match wins. No registry
/// found anywhere is `Ok(None)`: only an *explicit* miss is an error.
pub fn discover(explicit: Option<&Path>) -> Result<Option<(PathBuf, Registry)>> {
    if let Some(path) = explicit {
        let registry = load_registry(path)?;
        return Ok(Some((path.to_path_buf(), registry)));
    }

    let env = Env::from_process()?;
    discover_in(&env)
}

/// The filesystem touching half of [`discover`], split out so
/// [`Env::from_process`]'s real env reading half stays a single small
/// function and this half can be exercised (see tests) against a
/// hand built [`Env`], reading only whatever temp files a test creates.
fn discover_in(env: &Env) -> Result<Option<(PathBuf, Registry)>> {
    for candidate in env.candidates() {
        if candidate.is_file() {
            let registry = load_registry(&candidate)?;
            return Ok(Some((candidate, registry)));
        }
    }
    Ok(None)
}

/// Expand `~` and `$VAR` in a registry supplied path string. An undefined
/// variable is [`Error::Expand`], naming both the variable and the raw,
/// unexpanded string.
fn expand(raw: &str) -> Result<PathBuf> {
    shellexpand::full(raw)
        .map(|expanded| PathBuf::from(expanded.into_owned()))
        .map_err(|e| Error::Expand {
            var: e.var_name,
            raw: raw.to_string(),
        })
}

impl Registry {
    /// Build a [`Config`] for the named set.
    ///
    /// Unknown `name` is [`Error::UnknownSet`], naming the registry path
    /// passed in (normally the path [`discover`] returned alongside this
    /// `Registry`). `placement = "symlink"` with no `target` is
    /// [`Error::PlacementMissingTarget`], naming the set.
    pub fn config_for(&self, name: &str, registry_path: &Path) -> Result<Config> {
        let entry = self.sets.get(name).ok_or_else(|| Error::UnknownSet {
            name: name.to_string(),
            path: registry_path.to_path_buf(),
        })?;

        let fragments = expand(&entry.fragments)?;
        let output = OutputTarget::Path(expand(&entry.output)?);

        let catch_all = entry
            .catch_all
            .clone()
            .or_else(|| self.catch_all.clone())
            .unwrap_or_else(|| "99-local.json".to_string());

        // Same precedence as `catch_all`: per-set beats global beats the
        // crate-wide built-in default. `--indent` (CLI) layers on top of
        // this in main.rs, after `config_for` returns.
        let indent = entry.indent.or(self.indent).unwrap_or(2);

        // Same precedence again, for the comments mode. `--comments` (CLI)
        // likewise layers on top in main.rs.
        let comments = match entry.comments.clone().or_else(|| self.comments.clone()) {
            Some(raw) => raw.parse::<Comments>()?,
            None => Comments::default(),
        };

        let placement = match entry.placement.unwrap_or(PlacementKind::None) {
            PlacementKind::None => Placement::None,
            PlacementKind::Symlink => {
                let target =
                    entry
                        .target
                        .as_ref()
                        .ok_or_else(|| Error::PlacementMissingTarget {
                            name: name.to_string(),
                            path: registry_path.to_path_buf(),
                        })?;
                Placement::Symlink {
                    target: expand(target)?,
                }
            }
        };

        let pairs = entry
            .merge
            .iter()
            .map(|(pointer, strategy)| (pointer.clone(), strategy.clone()));
        let strategies = StrategyTable::from_pairs(pairs)?;

        Ok(Config {
            fragments,
            output,
            catch_all,
            strategies,
            indent,
            allow_empty: false,
            placement,
            format: &Json,
            comments,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, contents: &str) -> PathBuf {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("mkdir");
        }
        fs::write(&path, contents).expect("write");
        path
    }

    // --- Tests against `discover_in` / `Env` built by hand: no process env
    // touched at all, so these run fully in parallel with everything else.

    #[test]
    fn env_config_home_is_tried_first() {
        let home = TempDir::new().unwrap();
        let config_home = TempDir::new().unwrap();
        write(
            config_home.path(),
            "silktouch/registry.toml",
            "indent = 4\n",
        );

        let _ = &home;
        let env = Env {
            config_home: config_home.path().to_path_buf(),
            config_dirs: vec![],
        };
        let (path, registry) = discover_in(&env).unwrap().expect("found");
        assert_eq!(
            path,
            config_home.path().join("silktouch").join("registry.toml")
        );
        assert_eq!(registry.indent, Some(4));
    }

    #[test]
    fn env_config_dirs_searched_in_order_first_match_wins() {
        let home = TempDir::new().unwrap();
        let config_home = TempDir::new().unwrap(); // no registry.toml here
        let dir_a = TempDir::new().unwrap();
        let dir_b = TempDir::new().unwrap();
        write(dir_a.path(), "silktouch/registry.toml", "indent = 1\n");
        write(dir_b.path(), "silktouch/registry.toml", "indent = 2\n");

        let _ = &home;
        let env = Env {
            config_home: config_home.path().to_path_buf(),
            config_dirs: vec![dir_a.path().to_path_buf(), dir_b.path().to_path_buf()],
        };
        let (path, registry) = discover_in(&env).unwrap().expect("found");
        assert_eq!(path, dir_a.path().join("silktouch").join("registry.toml"));
        assert_eq!(registry.indent, Some(1));

        // Reversed order picks the other one: order, not content, decides.
        let env_reversed = Env {
            config_dirs: vec![dir_b.path().to_path_buf(), dir_a.path().to_path_buf()],
            ..env
        };
        let (path, registry) = discover_in(&env_reversed).unwrap().expect("found");
        assert_eq!(path, dir_b.path().join("silktouch").join("registry.toml"));
        assert_eq!(registry.indent, Some(2));
    }

    #[test]
    fn env_nothing_found_is_ok_none() {
        let home = TempDir::new().unwrap();
        let config_home = TempDir::new().unwrap();
        let _ = &home;
        let env = Env {
            config_home: config_home.path().to_path_buf(),
            config_dirs: vec![],
        };
        assert!(discover_in(&env).unwrap().is_none());
    }

    #[test]
    fn explicit_path_beats_discovery() {
        let config_home = TempDir::new().unwrap();
        write(
            config_home.path(),
            "silktouch/registry.toml",
            "indent = 9\n",
        );

        let explicit_dir = TempDir::new().unwrap();
        let explicit = write(explicit_dir.path(), "explicit.toml", "indent = 3\n");

        // discover() with an explicit path never even looks at process env,
        // so no Env/mutex needed here.
        let (path, registry) = discover(Some(&explicit)).unwrap().expect("found");
        assert_eq!(path, explicit);
        assert_eq!(registry.indent, Some(3));
    }

    #[test]
    fn explicit_path_missing_is_an_error() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist.toml");
        let err = discover(Some(&missing)).expect_err("must error");
        match err {
            Error::Io { path, .. } => assert_eq!(path, missing),
            other => panic!("expected Error::Io, got {other:?}"),
        }
    }

    #[test]
    fn registry_parse_failure_names_path() {
        let dir = TempDir::new().unwrap();
        let bad = write(dir.path(), "bad.toml", "not valid toml [[[");
        let err = discover(Some(&bad)).expect_err("must error");
        match err {
            Error::RegistryParse { path, .. } => assert_eq!(path, bad),
            other => panic!("expected Error::RegistryParse, got {other:?}"),
        }
    }

    // --- Tests against `Registry::config_for`: pure parsing + expansion
    // logic, no discovery, no process env beyond the one `~`/`$VAR` test
    // below (guarded).

    fn parse(toml_text: &str) -> Registry {
        toml::from_str(toml_text).expect("valid registry toml")
    }

    #[test]
    fn per_set_catch_all_overrides_global() {
        let reg = parse(
            r#"
            catch_all = "zz-global.json"

            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"

            [sets.b]
            fragments = "/frag/b"
            output = "/out/b.json"
            catch_all = "zz-b-local.json"
            "#,
        );
        let path = Path::new("/registry.toml");
        let cfg_a = reg.config_for("a", path).unwrap();
        assert_eq!(cfg_a.catch_all, "zz-global.json");
        let cfg_b = reg.config_for("b", path).unwrap();
        assert_eq!(cfg_b.catch_all, "zz-b-local.json");
    }

    #[test]
    fn global_default_catch_all_when_neither_set() {
        let reg = parse(
            r#"
            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"
            "#,
        );
        let cfg = reg.config_for("a", Path::new("/registry.toml")).unwrap();
        assert_eq!(cfg.catch_all, "99-local.json");
    }

    // --- Per set `indent`, same precedence as `catch_all`. -----------------

    #[test]
    fn per_set_indent_overrides_global() {
        let reg = parse(
            r#"
            indent = 4

            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"

            [sets.b]
            fragments = "/frag/b"
            output = "/out/b.json"
            indent = 8
            "#,
        );
        let path = Path::new("/registry.toml");
        let cfg_a = reg.config_for("a", path).unwrap();
        assert_eq!(cfg_a.indent, 4, "a falls back to the global default");
        let cfg_b = reg.config_for("b", path).unwrap();
        assert_eq!(cfg_b.indent, 8, "b's own indent wins over the global one");
    }

    #[test]
    fn global_default_indent_when_neither_set() {
        let reg = parse(
            r#"
            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"
            "#,
        );
        let cfg = reg.config_for("a", Path::new("/registry.toml")).unwrap();
        assert_eq!(cfg.indent, 2, "the crate-wide built-in default");
    }

    // --- `comments`, same precedence as `catch_all`/`indent`. --------------

    #[test]
    fn comments_default_is_forbid_when_unset_anywhere() {
        let reg = parse(
            r#"
            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"
            "#,
        );
        let cfg = reg.config_for("a", Path::new("/registry.toml")).unwrap();
        assert_eq!(cfg.comments, Comments::Forbid);
    }

    #[test]
    fn global_comments_mode_is_applied() {
        let reg = parse(
            r#"
            comments = "protect"

            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"
            "#,
        );
        let cfg = reg.config_for("a", Path::new("/registry.toml")).unwrap();
        assert_eq!(cfg.comments, Comments::Protect);
    }

    #[test]
    fn per_set_comments_overrides_global() {
        let reg = parse(
            r#"
            comments = "protect"

            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"

            [sets.b]
            fragments = "/frag/b"
            output = "/out/b.json"
            comments = "strip"
            "#,
        );
        let path = Path::new("/registry.toml");
        let cfg_a = reg.config_for("a", path).unwrap();
        assert_eq!(cfg_a.comments, Comments::Protect);
        let cfg_b = reg.config_for("b", path).unwrap();
        assert_eq!(cfg_b.comments, Comments::Strip);
    }

    #[test]
    fn unknown_comments_mode_in_registry_errors_naming_the_value() {
        let reg = parse(
            r#"
            [sets.a]
            fragments = "/frag/a"
            output = "/out/a.json"
            comments = "yolo"
            "#,
        );
        match reg.config_for("a", Path::new("/registry.toml")) {
            Err(Error::UnknownCommentsMode(v)) => assert_eq!(v, "yolo"),
            other => panic!(
                "expected Error::UnknownCommentsMode, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn unknown_set_errors() {
        let reg = parse("");
        let path = Path::new("/registry.toml");
        match reg.config_for("nope", path) {
            Err(Error::UnknownSet { name, path: p }) => {
                assert_eq!(name, "nope");
                assert_eq!(p, path);
            }
            other => panic!("expected Error::UnknownSet, got {}", describe(&other)),
        }
    }

    #[test]
    fn symlink_placement_without_target_errors_naming_set() {
        let reg = parse(
            r#"
            [sets.helix]
            fragments = "/frag/helix"
            output = "/out/helix.json"
            placement = "symlink"
            "#,
        );
        let path = Path::new("/registry.toml");
        match reg.config_for("helix", path) {
            Err(Error::PlacementMissingTarget { name, path: p }) => {
                assert_eq!(name, "helix");
                assert_eq!(p, path);
            }
            other => panic!(
                "expected Error::PlacementMissingTarget, got {}",
                describe(&other)
            ),
        }
    }

    #[test]
    fn symlink_placement_with_target_resolves() {
        let reg = parse(
            r#"
            [sets.helix]
            fragments = "/frag/helix"
            output = "/out/helix.json"
            placement = "symlink"
            target = "/home/x/.config/helix/config.json"
            "#,
        );
        let cfg = reg
            .config_for("helix", Path::new("/registry.toml"))
            .unwrap();
        assert_eq!(
            cfg.placement,
            Placement::Symlink {
                target: PathBuf::from("/home/x/.config/helix/config.json")
            }
        );
    }

    #[test]
    fn merge_table_becomes_strategies() {
        use crate::pointer::{ArrayStrategy, Pointer};

        let reg = parse(
            r#"
            [sets.claude]
            fragments = "/frag/claude"
            output = "/out/claude.json"

            [sets.claude.merge]
            "/permissions/allow" = "concat-dedupe"
            "/some/ordered/list" = "replace"
            "#,
        );
        let cfg = reg
            .config_for("claude", Path::new("/registry.toml"))
            .unwrap();
        assert_eq!(
            cfg.strategies
                .lookup(&Pointer::parse("/permissions/allow").unwrap()),
            ArrayStrategy::ConcatDedupe
        );
        assert_eq!(
            cfg.strategies
                .lookup(&Pointer::parse("/some/ordered/list").unwrap()),
            ArrayStrategy::Replace
        );
    }

    // --- The one test that genuinely needs `~`/`$VAR` expansion against
    // real process env. Guarded by a mutex so it cannot race any other test
    // in this binary that also mutates process env (there are none today
    // outside this module, but the guard costs nothing and keeps that true
    // even if that changes).
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tilde_and_var_expansion() {
        let _lock = ENV_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let saved = std::env::var("SILKTOUCH_TEST_DOTFILES").ok();
        unsafe {
            std::env::set_var("SILKTOUCH_TEST_DOTFILES", "/opt/dotfiles");
        }

        let reg = parse(
            r#"
            [sets.claude]
            fragments = "$SILKTOUCH_TEST_DOTFILES/silktouch/claude"
            output = "$SILKTOUCH_TEST_DOTFILES/claude/settings.json"
            "#,
        );
        let cfg = reg
            .config_for("claude", Path::new("/registry.toml"))
            .unwrap();
        assert_eq!(
            cfg.fragments,
            PathBuf::from("/opt/dotfiles/silktouch/claude")
        );
        assert_eq!(
            cfg.output,
            OutputTarget::Path(PathBuf::from("/opt/dotfiles/claude/settings.json"))
        );

        match saved {
            Some(v) => unsafe { std::env::set_var("SILKTOUCH_TEST_DOTFILES", v) },
            None => unsafe { std::env::remove_var("SILKTOUCH_TEST_DOTFILES") },
        }
    }

    #[test]
    fn undefined_var_errors() {
        let reg = parse(
            r#"
            [sets.a]
            fragments = "$SILKTOUCH_DEFINITELY_UNDEFINED_VAR/frag"
            output = "/out/a.json"
            "#,
        );
        match reg.config_for("a", Path::new("/registry.toml")) {
            Err(Error::Expand { var, .. }) => assert_eq!(var, "SILKTOUCH_DEFINITELY_UNDEFINED_VAR"),
            other => panic!("expected Error::Expand, got {}", describe(&other)),
        }
    }

    /// `Config` holds a `&'static dyn Format`, which has no `Debug` impl, so
    /// `Result<Config, Error>` can't derive one either. Hence this instead
    /// of `{other:?}` in the panic messages above.
    fn describe(result: &Result<Config>) -> &'static str {
        match result {
            Ok(_) => "Ok(_)",
            Err(_) => "Err(_) of the wrong variant",
        }
    }
}
