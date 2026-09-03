//! Shell completions: two independent mechanisms, both from `clap_complete`
//! 4.6.9.
//!
//! **(a) Static scripts** (`silktouch completions <shell>`): the ordinary
//! ahead of time generator, [`clap_complete::generate`] over
//! [`clap_complete::aot::Shell`]. These are plain, unchanging shell
//! functions, installed by `flake.nix` and by anyone who runs the
//! subcommand by hand. They know nothing about live data. `--set foo<TAB>`
//! in one of these scripts completes nothing beyond what clap's static
//! metadata already offers (flag names, `--merge`'s value name, etc).
//!
//! **(b) Dynamic value completion**: real, working, but through a
//! *different* protocol than (a). `clap_complete`'s ahead of time shell
//! generators (bash/zsh/fish/elvish/powershell) do not call back into the
//! binary at all. There is nothing in `src/aot/*.rs` that looks at
//! [`clap_complete::engine::ArgValueCompleter`]. The only mechanism in this
//! crate version that reinvokes the binary for a fresh, per keystroke
//! answer is the `unstable-dynamic` engine, activated by
//! `clap_complete::CompleteEnv` via the `COMPLETE=<shell>` environment
//! variable (wired up once, early in `main`, before `Cli::parse()`). A user
//! opts in with e.g. `source <(COMPLETE=bash silktouch)`. That sourced
//! snippet is what actually calls the three completers below for `--set`,
//! `--merge` and `--catch-all`. The functions here are registered onto
//! those `clap::Arg`s via `#[arg(add = ArgValueCompleter::new(...))]` in
//! `main.rs`, which needs clap's `unstable-ext` feature (for `Arg::add`)
//! alongside `clap_complete`'s `unstable-dynamic`.
//!
//! Every completer here is a hard dead end on failure: a missing or
//! unreadable registry, a malformed `registry.toml`, a fragment directory
//! that does not exist: all of it completes to an empty list, never a
//! panic, never stderr output, never a nonzero exit. A completion handler
//! that errors or panics breaks the user's shell, so every fallible step is
//! collapsed to `Option`/`Result` and discarded with `.ok()`.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use clap_complete::engine::CompletionCandidate;

use silktouch::registry;

/// Array merge strategy names, as accepted after `=` in `--merge
/// POINTER=STRATEGY`. Kept in sync with
/// [`silktouch::pointer::ArrayStrategy::from_name`] by hand. There are
/// exactly two, and their names are a stable public interface: registry
/// `[sets.NAME.merge]` tables use the same strings.
const STRATEGY_NAMES: [&str; 2] = ["concat-dedupe", "replace"];

/// Complete `--set NAME` from the discovered registry's set names.
///
/// Registry discovery walks `$XDG_CONFIG_HOME` and `$XDG_CONFIG_DIRS`, so
/// this respects an already typed `--config` the same way the real command
/// does: see [`typed_value_after`]. Any failure to locate or parse a
/// registry (no registry present, unreadable file, bad TOML) yields no
/// candidates.
pub fn complete_set(current: &OsStr) -> Vec<CompletionCandidate> {
    let config = typed_value_after("--config").map(PathBuf::from);
    complete_set_from(current, config.as_deref())
}

fn complete_set_from(current: &OsStr, config: Option<&Path>) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();
    let Ok(Some((_, reg))) = registry::discover(config) else {
        return Vec::new();
    };

    reg.sets
        .keys()
        .filter(|name| name.starts_with(prefix.as_ref()))
        .map(CompletionCandidate::new)
        .collect()
}

/// Complete the `STRATEGY` half of `--merge POINTER=STRATEGY`.
///
/// The pointer half is left untouched: only the text after the first `=`
/// is matched against the two known strategy names, and any candidate is
/// prefixed with `POINTER=` again on the way out via
/// [`CompletionCandidate::add_prefix`]. Before an `=` has been typed there
/// is nothing to complete. This never guesses at a pointer.
pub fn complete_merge_strategy(current: &OsStr) -> Vec<CompletionCandidate> {
    let current = current.to_string_lossy();
    let Some((pointer, partial)) = current.split_once('=') else {
        return Vec::new();
    };
    let prefix = format!("{pointer}=");

    STRATEGY_NAMES
        .iter()
        .filter(|name| name.starts_with(partial))
        .map(|name| CompletionCandidate::new(*name).add_prefix(prefix.clone()))
        .collect()
}

/// Complete `--catch-all NAME` from the `*.json` fragments already sitting
/// in the fragment directory, when one can be worked out from the rest of
/// the line typed so far (`--fragments DIR`, or `--set NAME` resolved
/// through the registry). No fragment directory determinable, or the
/// directory does not exist or is not readable: no candidates.
pub fn complete_catch_all(current: &OsStr) -> Vec<CompletionCandidate> {
    let fragments = typed_value_after("--fragments");
    let set = typed_value_after("--set");
    let config = typed_value_after("--config").map(PathBuf::from);

    let dir = resolve_fragment_dir(fragments.as_deref(), set.as_deref(), config.as_deref());
    complete_catch_all_in(current, dir.as_deref())
}

/// Whether `name` is a name `FragmentSet::load` would treat as a fragment.
///
/// Case sensitive on purpose: must match `FragmentSet::load`'s own `*.json`
/// rule exactly, or completions would offer names silktouch itself would
/// refuse to treat as fragments.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_fragment_name(name: &str) -> bool {
    !name.starts_with('.') && name.ends_with(".json")
}

fn complete_catch_all_in(current: &OsStr, dir: Option<&Path>) -> Vec<CompletionCandidate> {
    let prefix = current.to_string_lossy();

    let Some(dir) = dir else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| is_fragment_name(name))
        .filter(|name| name.starts_with(prefix.as_ref()))
        .collect();
    names.sort();

    names.into_iter().map(CompletionCandidate::new).collect()
}

/// Resolve the fragment directory implied by the arguments typed so far,
/// the same way [`silktouch::Config`] would: an explicit `--fragments DIR`
/// wins outright. Otherwise a `--set NAME` is resolved through the
/// registry (honouring a typed `--config` too). Anything that does not
/// resolve cleanly (no registry, unknown set name, a fragments path that
/// fails to expand) yields `None`, not a guess.
fn resolve_fragment_dir(
    fragments: Option<&str>,
    set: Option<&str>,
    config: Option<&Path>,
) -> Option<PathBuf> {
    if let Some(dir) = fragments {
        return Some(PathBuf::from(dir));
    }

    let name = set?;
    let (_, reg) = registry::discover(config).ok()??;
    let entry = reg.sets.get(name)?;
    let expanded = shellexpand::full(&entry.fragments).ok()?;
    Some(PathBuf::from(expanded.into_owned()))
}

/// Look up the value following a given flag among the arguments the shell
/// has actually typed so far.
///
/// `clap_complete::engine::ValueCompleter` only hands a completer the
/// current argument's own text. It has no view of the rest of the
/// command line. But under `CompleteEnv`'s `COMPLETE=<shell>` protocol, the
/// generated shell snippet reinvokes this very binary with the whole
/// typed so far word list appended after a `--`, and that reaches us as
/// ordinary process arguments. So the sibling arguments a completer needs
/// (`--fragments` or `--set` when completing `--catch-all`, `--config`
/// when completing `--set`) are read directly from [`std::env::args_os`]
/// rather than through any clap API. This is a hack forced by the shape of
/// the `unstable-dynamic` engine in this version, not a designed extension
/// point. It is confined to this module, and every caller treats its
/// result as advisory (worst case: no completion offered).
fn typed_value_after(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args_os()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let idx = args.iter().position(|arg| arg == flag)?;
    args.get(idx + 1).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    // These drive the `_from`/`_in` inner functions directly with an
    // explicit `config`/`dir` override, the same discipline `registry.rs`
    // uses for its own discovery tests: no real `$XDG_*` env touched, no
    // dependence on this test binary's own argv, so nothing here is
    // flaky under parallel `cargo test`.

    #[test]
    fn complete_set_empty_for_missing_registry() {
        let candidates = complete_set_from(
            OsStr::new(""),
            Some(Path::new("/nonexistent/registry.toml")),
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn complete_merge_strategy_before_equals_is_empty() {
        assert!(complete_merge_strategy(OsStr::new("/some/list")).is_empty());
    }

    #[test]
    fn complete_merge_strategy_filters_and_reattaches_pointer() {
        let candidates = complete_merge_strategy(OsStr::new("/some/list=rep"));
        let values: Vec<OsString> = candidates
            .iter()
            .map(|c| c.get_value().to_owned())
            .collect();
        assert_eq!(values, vec![OsString::from("/some/list=replace")]);
    }

    #[test]
    fn complete_merge_strategy_empty_partial_lists_both() {
        let candidates = complete_merge_strategy(OsStr::new("/some/list="));
        assert_eq!(candidates.len(), 2);
    }

    #[test]
    fn complete_catch_all_empty_for_no_directory() {
        assert!(complete_catch_all_in(OsStr::new(""), None).is_empty());
    }

    #[test]
    fn complete_catch_all_empty_for_nonexistent_directory() {
        let candidates =
            complete_catch_all_in(OsStr::new(""), Some(Path::new("/nonexistent/fragments")));
        assert!(candidates.is_empty());
    }

    #[test]
    fn complete_catch_all_lists_json_fragments() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("10-base.json"), "{}").unwrap();
        std::fs::write(dir.path().join("99-local.json"), "{}").unwrap();
        std::fs::write(dir.path().join(".hidden.json"), "{}").unwrap();
        std::fs::write(dir.path().join("readme.txt"), "").unwrap();

        let candidates = complete_catch_all_in(OsStr::new(""), Some(dir.path()));
        let values: Vec<String> = candidates
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();
        assert_eq!(values, vec!["10-base.json", "99-local.json"]);
    }

    #[test]
    fn resolve_fragment_dir_prefers_explicit_fragments() {
        let dir = resolve_fragment_dir(Some("/explicit/dir"), Some("ignored-set"), None);
        assert_eq!(dir, Some(PathBuf::from("/explicit/dir")));
    }

    #[test]
    fn resolve_fragment_dir_none_with_neither_flag() {
        assert!(resolve_fragment_dir(None, None, None).is_none());
    }

    #[test]
    fn resolve_fragment_dir_none_for_unresolvable_set() {
        let dir = resolve_fragment_dir(
            None,
            Some("nope"),
            Some(Path::new("/nonexistent/registry.toml")),
        );
        assert!(dir.is_none());
    }

    #[test]
    fn typed_value_after_does_not_panic() {
        // Just exercises the real std::env::args_os() path once, so the
        // "hack" documented above is at least known not to panic under a
        // typical `cargo test` invocation.
        let _ = typed_value_after("--config");
    }
}
