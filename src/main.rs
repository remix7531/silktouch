//! `silktouch`: the CLI. Thin over the library: parses flags, resolves a
//! [`Config`] (either from `--fragments`/`--output` or a registry `--set`),
//! calls into `combine`/`split`/`diff`, and translates the result into
//! stdout/stderr text and an [`ExitCode`].
//!
//! `sync` (`split` then `combine`) lives here and only here. The plan is
//! explicit that nothing in the library may depend on it.
//!
//! A later wave adds `completions` and `man` subcommands. [`Command`] is
//! left with room for them.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, anyhow};
use clap::{Args, CommandFactory, Parser, Subcommand};
use clap_complete::engine::ArgValueCompleter;

use silktouch::fragment::FragmentSet;
use silktouch::pointer::{Pointer, StrategyTable};
use silktouch::{
    Change, Combined, Config, OutputTarget, Placement, combine, diff, placement, registry, split,
};

mod completions;
mod man;

/// silktouch: split a combined config file into fragments, and combine
/// fragments back into one file.
#[derive(Parser)]
#[command(
    name = "silktouch",
    version,
    about = "Bidirectional config fragment merger.",
    long_about = "\
silktouch takes a config file apart without breaking it.

`combine` folds every *.json fragment in a directory into one output file,\n\
in filename order, later fragments winning on conflicts. `split` runs the\n\
relation the other way: it reads that output file back and routes every\n\
difference into the fragment that should own it: values overwritten in\n\
place, new keys filed into a catch all fragment, deletions removed from\n\
every fragment that declared them. Hand edit the fragments and run\n\
`combine`, or hand edit the output and run `split`. Both directions of the\n\
same relation.",
    after_help = "\
EXAMPLES:
    Pipe the combined document straight into jq, touching no files:
        silktouch combine --fragments d/ -o - | jq .permissions.allow

    Drive a set registered in registry.toml by name:
        silktouch diff --set claude

    Override one array's merge strategy for a single run:
        silktouch combine --fragments d/ -o out.json --merge '/some/list=replace'"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fold fragments into one combined document.
    #[command(
        long_about = "Discovers every *.json fragment in --fragments (or a registry --set), \
folds them left to right by filename, and writes the result to --output, \
creating it and any missing parent directories. A write that would not \
change the file's content is skipped, so its mtime stays stable. With \
placement = \"symlink\" (registry sets only), also ensures the symlink \
exists. -o - writes the combined document to stdout instead and touches no \
files at all: no output file, no placement."
    )]
    Combine(TargetArgs),

    /// Route an externally edited output file back into its fragments.
    #[command(
        long_about = "Recomputes the expected output from the current fragments, diffs that \
against the real --output file on disk, and writes each difference back \
into the fragment that owns it: a changed value overwrites in place, a new \
key is filed into the catch all fragment, and a deleted key is removed \
from every fragment that declared it. Only fragments that actually change \
are rewritten. Errors, changing nothing, if --output is missing or is not \
a JSON object: a missing file is never treated as \"delete everything\"."
    )]
    Split(TargetArgs),

    /// Split, then combine: reconciles fragments and output in the safe order.
    #[command(
        long_about = "Sugar for `split` followed by `combine`, in that order: first any \
external edits sitting in --output are absorbed into the fragments, then \
the fragments are recombined back into --output. Running the two steps \
in the other order would silently discard the external edit, which is why \
this exists as one command instead of two manual steps."
    )]
    Sync(TargetArgs),

    /// Report whether the fragments and the output file agree.
    #[command(
        long_about = "Compares the fragments' combined value against the on disk --output \
file and prints the differences `split` would make (one line per \
changed pointer, showing the change and where it would land) without \
writing anything, ever. Also reports symlink placement drift for a \
registry --set. Exit code is the API, following diff(1): 0 when \
everything agrees, 1 when it doesn't, 2 on error."
    )]
    Diff(TargetArgs),

    /// Print a shell completion script to stdout.
    #[command(
        hide = true,
        long_about = "Writes a static completion script for SHELL to stdout, via \
clap_complete's ahead of time generator: pipe it wherever your shell \
expects completions to live. This does not, by itself, complete --set, \
--merge or --catch-all against live data (registered set names, merge \
strategies, *.json fragments on disk). That needs the separate \
COMPLETE=<shell> dynamic completion protocol clap_complete provides (see \
`man silktouch`). Hidden from the top level --help to keep the four verbs \
above legible. Still a real subcommand, documented in the man page."
    )]
    Completions {
        /// Shell to generate a completion script for.
        #[arg(value_enum)]
        shell: clap_complete::aot::Shell,
    },

    /// Print the silktouch(1) man page to stdout, in roff.
    #[command(
        hide = true,
        long_about = "Renders silktouch(1) to stdout: clap's own flag reference plus \
hand written sections covering what --help has no room for: the combine \
and split algorithms, the array merge strategies, the round trip laws, \
exit statuses, file locations, the registry format and worked examples. \
Pipe it into `man -l -` to read it, or install it as a real man page. \
Hidden from the top level --help to keep the four verbs above legible. \
Still a real subcommand, documented in the man page it prints."
    )]
    Man,
}

/// Flags shared by every subcommand.
#[derive(Args)]
struct TargetArgs {
    /// Registry set name (see registry.toml). Mutually exclusive with --fragments/--output.
    #[arg(
        long,
        conflicts_with_all = ["fragments", "output"],
        add = ArgValueCompleter::new(completions::complete_set)
    )]
    set: Option<String>,

    /// Fragment directory to read *.json fragments from. Mutually exclusive with --set.
    #[arg(
        long,
        requires = "output",
        conflicts_with = "set",
        required_unless_present = "set"
    )]
    fragments: Option<PathBuf>,

    /// Combined output file, or "-" for stdout. "-" only makes sense for `combine`. It writes the combined document to stdout and touches no files: no output file is created, and placement is not applied. Mutually exclusive with --set.
    #[arg(
        short = 'o',
        long,
        requires = "fragments",
        conflicts_with = "set",
        required_unless_present = "set",
        value_name = "FILE|-"
    )]
    output: Option<String>,

    /// Registry file to use instead of the discovered one (`$XDG_CONFIG_HOME/silktouch/registry.toml`, then each `$XDG_CONFIG_DIRS` entry).
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Array merge strategy override: POINTER=STRATEGY, where STRATEGY is concat-dedupe or replace. Repeatable. Layers over (and overrides) a registry set's [sets.NAME.merge] table.
    #[arg(
        long = "merge",
        value_name = "POINTER=STRATEGY",
        add = ArgValueCompleter::new(completions::complete_merge_strategy)
    )]
    merges: Vec<String>,

    /// Indentation width, in spaces, for written JSON. Overrides the registry and the crate default (2).
    #[arg(long, value_name = "N")]
    indent: Option<usize>,

    /// Catch all fragment filename for new keys and appended array elements. Overrides the registry and the crate default (99-local.json).
    #[arg(
        long = "catch-all",
        value_name = "NAME",
        add = ArgValueCompleter::new(completions::complete_catch_all)
    )]
    catch_all: Option<String>,

    /// How comments in source fragments (and the output file) are treated: forbid (default) rejects any comment as a parse error naming the file (today's behaviour, unchanged). protect accepts JSONC comments (// and /* */) but errors, naming the file, rather than silently dropping them, if split/sync would need to rewrite a fragment that has them. strip accepts comments and silently drops them on such a rewrite. The combined output never contains a comment, in any mode. Overrides the registry and the crate default (forbid).
    #[arg(long, value_name = "MODE")]
    comments: Option<String>,

    /// Treat a missing or empty fragment directory as {} instead of an error.
    #[arg(long)]
    allow_empty: bool,

    /// Print routed changes in more detail.
    #[arg(short, long)]
    verbose: bool,
}

/// Parse one `--merge` value into a `(pointer, strategy name)` pair.
fn parse_merge_flag(raw: &str) -> anyhow::Result<(String, String)> {
    let (pointer, strategy) = raw.split_once('=').ok_or_else(|| {
        anyhow!("--merge {raw:?}: expected POINTER=STRATEGY, e.g. /some/list=replace")
    })?;
    Ok((pointer.to_string(), strategy.to_string()))
}

/// Resolve `args` into a [`Config`], from either a registry `--set` or an
/// explicit `--fragments`/`--output` pair, with the shared flags layered on
/// top, with `--merge` overriding last.
fn build_config(args: &TargetArgs) -> anyhow::Result<Config> {
    let mut cfg = if let Some(name) = &args.set {
        let (registry_path, reg) = registry::discover(args.config.as_deref())
            .context("locating registry")?
            .ok_or_else(|| {
                anyhow!(
                    "no registry found; pass --config PATH or create \
                     $XDG_CONFIG_HOME/silktouch/registry.toml"
                )
            })?;

        // Errors from `config_for` already name both the set and the
        // registry path, so no extra context is layered on here.
        let mut cfg = reg.config_for(name, &registry_path)?;

        if !args.merges.is_empty() {
            // `config_for` above already validated that `name` exists, so
            // this lookup cannot fail.
            let entry = reg
                .sets
                .get(name)
                .expect("config_for validated this set exists");
            let base_pairs = entry.merge.iter().map(|(p, s)| (p.clone(), s.clone()));
            let cli_pairs = args
                .merges
                .iter()
                .map(|raw| parse_merge_flag(raw))
                .collect::<anyhow::Result<Vec<_>>>()?;
            cfg.strategies =
                StrategyTable::from_pairs(base_pairs.chain(cli_pairs)).context("--merge")?;
        }
        cfg
    } else {
        let fragments = args
            .fragments
            .clone()
            .expect("clap requires --fragments together with --output");
        let output_raw = args
            .output
            .clone()
            .expect("clap requires --output together with --fragments");
        let output = if output_raw == "-" {
            OutputTarget::Stdout
        } else {
            OutputTarget::Path(PathBuf::from(output_raw))
        };

        let mut cfg = Config {
            fragments,
            output,
            ..Config::default()
        };
        if !args.merges.is_empty() {
            let pairs = args
                .merges
                .iter()
                .map(|raw| parse_merge_flag(raw))
                .collect::<anyhow::Result<Vec<_>>>()?;
            cfg.strategies = StrategyTable::from_pairs(pairs).context("--merge")?;
        }
        cfg
    };

    if let Some(indent) = args.indent {
        cfg.indent = indent;
    }
    if let Some(catch_all) = &args.catch_all {
        cfg.catch_all.clone_from(catch_all);
    }
    if let Some(comments) = &args.comments {
        cfg.comments = comments.parse().context("--comments")?;
    }
    if args.allow_empty {
        cfg.allow_empty = true;
    }

    Ok(cfg)
}

/// Human readable label for a `Config`'s fragments/output pair, for error
/// context: so an error always names what it was operating on.
fn describe_target(cfg: &Config) -> String {
    match &cfg.output {
        OutputTarget::Path(p) => {
            format!(
                "fragments {} <-> output {}",
                cfg.fragments.display(),
                p.display()
            )
        }
        OutputTarget::Stdout => format!("fragments {} -> stdout", cfg.fragments.display()),
    }
}

fn json_compact(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_else(|_| "?".to_string())
}

fn fragment_name(set: &FragmentSet, idx: usize) -> &str {
    set.fragments.get(idx).map_or("?", |f| f.name.as_str())
}

fn owner_name(set: &FragmentSet, path: &Pointer) -> String {
    set.owner_of(path)
        .map_or_else(|| "?".to_string(), |i| fragment_name(set, i).to_string())
}

fn declarer_names(set: &FragmentSet, path: &Pointer) -> String {
    let names: Vec<&str> = set
        .declarers_of(path)
        .into_iter()
        .map(|i| fragment_name(set, i))
        .collect();
    if names.is_empty() {
        "?".to_string()
    } else {
        names.join(", ")
    }
}

/// One `diff` line per change: the pointer, what changed, and where it
/// would land, e.g. `/permissions/allow  +"Bash"  -> 99-local.json`.
fn describe_routed_change(change: &Change, set: &FragmentSet) -> Vec<String> {
    let path = change.path().as_str();
    match change {
        Change::Set { value, .. } => vec![format!(
            "{path}  ~{}  -> {}",
            json_compact(value),
            owner_name(set, change.path())
        )],
        Change::Insert { value, .. } => {
            vec![format!(
                "{path}  +{}  -> {}",
                json_compact(value),
                set.catch_all
            )]
        }
        Change::Unset { .. } => vec![format!(
            "{path}  -  -> {}",
            declarer_names(set, change.path())
        )],
        Change::ArrayMembers { added, removed, .. } => removed
            .iter()
            .map(|v| {
                format!(
                    "{path}  -{}  -> {}",
                    json_compact(v),
                    declarer_names(set, change.path())
                )
            })
            .chain(
                added
                    .iter()
                    .map(|v| format!("{path}  +{}  -> {}", json_compact(v), set.catch_all)),
            )
            .collect(),
    }
}

/// The same change, without a routing destination: used for `--verbose`
/// on `split`/`sync`, where the routing has already happened.
fn describe_change_plain(change: &Change) -> Vec<String> {
    let path = change.path().as_str();
    match change {
        Change::Set { value, .. } => vec![format!("{path}  ~{}", json_compact(value))],
        Change::Insert { value, .. } => vec![format!("{path}  +{}", json_compact(value))],
        Change::Unset { .. } => vec![format!("{path}  -")],
        Change::ArrayMembers { added, removed, .. } => removed
            .iter()
            .map(|v| format!("{path}  -{}", json_compact(v)))
            .chain(
                added
                    .iter()
                    .map(|v| format!("{path}  +{}", json_compact(v))),
            )
            .collect(),
    }
}

/// Report a symlink that `combine` just created. A placement that was
/// already correct yields `None` and prints nothing, per the rule of silence.
fn report_placement(outcome: &Combined, output: &std::path::Path) {
    if let Some(linked) = &outcome.linked {
        println!("linked {} -> {}", linked.display(), output.display());
    }
}

fn run_combine(args: &TargetArgs) -> anyhow::Result<()> {
    let cfg = build_config(args)?;
    let outcome = combine(&cfg).with_context(|| format!("combine: {}", describe_target(&cfg)))?;

    match &cfg.output {
        OutputTarget::Stdout => print!("{}", outcome.text),
        OutputTarget::Path(path) => {
            if let Some(wrote) = &outcome.wrote {
                println!("wrote {}", wrote.display());
                if args.verbose {
                    println!("  indent {}, catch-all {}", cfg.indent, cfg.catch_all);
                }
            }
            report_placement(&outcome, path);
        }
    }
    Ok(())
}

fn run_split(args: &TargetArgs) -> anyhow::Result<()> {
    let cfg = build_config(args)?;
    let outcome = split(&cfg).with_context(|| format!("split: {}", describe_target(&cfg)))?;

    for path in &outcome.written {
        println!("wrote {}", path.display());
    }
    if args.verbose {
        for change in &outcome.changes {
            for line in describe_change_plain(change) {
                println!("  {line}");
            }
        }
    }
    Ok(())
}

fn run_sync(args: &TargetArgs) -> anyhow::Result<()> {
    let cfg = build_config(args)?;

    // split, then combine: reversing the order would discard external
    // edits sitting in the output file. This ordering is `sync`'s entire
    // reason to exist.
    let split_outcome =
        split(&cfg).with_context(|| format!("sync (split): {}", describe_target(&cfg)))?;
    for path in &split_outcome.written {
        println!("wrote {}", path.display());
    }
    if args.verbose {
        for change in &split_outcome.changes {
            for line in describe_change_plain(change) {
                println!("  {line}");
            }
        }
    }

    let combined =
        combine(&cfg).with_context(|| format!("sync (combine): {}", describe_target(&cfg)))?;
    match &cfg.output {
        OutputTarget::Stdout => print!("{}", combined.text),
        OutputTarget::Path(path) => {
            if let Some(wrote) = &combined.wrote {
                println!("wrote {}", wrote.display());
            }
            report_placement(&combined, path);
        }
    }
    Ok(())
}

/// Returns the process exit code directly (`diff` follows `diff(1)`: 0
/// same, 1 different, 2 error, the 2 is produced by `main`'s error
/// handling, not here).
fn run_diff(args: &TargetArgs) -> anyhow::Result<u8> {
    let cfg = build_config(args)?;
    let outcome = diff(&cfg).with_context(|| format!("diff: {}", describe_target(&cfg)))?;

    // Read only, purely to name routing destinations in the printed lines
    // below. `diff` itself never touches the fragments.
    let set = FragmentSet::load(
        &cfg.fragments,
        cfg.format,
        &cfg.catch_all,
        cfg.allow_empty,
        cfg.comments,
    )
    .with_context(|| format!("diff: fragments {}", cfg.fragments.display()))?;

    for change in &outcome.changes {
        for line in describe_routed_change(change, &set) {
            println!("{line}");
        }
    }

    let mut different = !outcome.in_sync;

    if let OutputTarget::Path(output_path) = &cfg.output {
        let status = placement::status(&cfg.placement, output_path)
            .with_context(|| format!("placement for {}", output_path.display()))?;
        match status {
            placement::PlacementStatus::NotApplicable | placement::PlacementStatus::Satisfied => {}
            placement::PlacementStatus::Missing => {
                if let Placement::Symlink { target } = &cfg.placement {
                    println!(
                        "placement: {} missing (would link to {})",
                        target.display(),
                        output_path.display()
                    );
                }
                different = true;
            }
            placement::PlacementStatus::Conflict { path } => {
                println!(
                    "placement: {} conflicts (does not point at {})",
                    path.display(),
                    output_path.display()
                );
                different = true;
            }
        }
    }

    Ok(u8::from(different))
}

/// Print a static completion script for `shell` to stdout, via
/// `clap_complete`'s ahead of time generator.
// Never actually fails today, but keeps the same `-> anyhow::Result<()>`
// signature as its sibling `run_*` handlers so the `match` in `main` below
// can treat every arm uniformly via `.map(|()| 0)`.
#[allow(clippy::unnecessary_wraps)]
fn run_completions(shell: clap_complete::aot::Shell) -> anyhow::Result<()> {
    let mut cmd = Cli::command();
    let name = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
    Ok(())
}

/// Print `silktouch(1)`, in roff, to stdout.
fn run_man() -> anyhow::Result<()> {
    man::render(Cli::command(), &mut std::io::stdout()).context("rendering man page")
}

fn main() -> ExitCode {
    // Must run before anything else touches stdout: under `COMPLETE=<shell>`
    // this either prints a completion answer (or the shell integration
    // snippet) and exits, or (the overwhelming common case, `COMPLETE`
    // unset) returns immediately and falls through to the real CLI. See
    // `completions` module docs for why this is a separate protocol from
    // the `completions` subcommand below.
    clap_complete::CompleteEnv::with_factory(Cli::command).complete();

    let cli = Cli::parse();

    let result: anyhow::Result<u8> = match &cli.command {
        Command::Combine(args) => run_combine(args).map(|()| 0),
        Command::Split(args) => run_split(args).map(|()| 0),
        Command::Sync(args) => run_sync(args).map(|()| 0),
        Command::Diff(args) => run_diff(args),
        Command::Completions { shell } => run_completions(*shell).map(|()| 0),
        Command::Man => run_man().map(|()| 0),
    };

    match result {
        Ok(code) => ExitCode::from(code),
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use assert_cmd::Command;
    use clap_complete::aot::Shell;
    use tempfile::TempDir;

    fn bin() -> Command {
        Command::cargo_bin("silktouch").expect("silktouch binary is built")
    }

    // ---- (a) static script generation, clap_complete::aot::generate. ----

    #[test]
    fn completions_nonempty_for_every_shell() {
        for shell in [
            Shell::Bash,
            Shell::Zsh,
            Shell::Fish,
            Shell::Elvish,
            Shell::PowerShell,
        ] {
            let output = bin()
                .args(["completions", &shell.to_string()])
                .output()
                .expect("run");
            assert!(output.status.success(), "{shell} exited nonzero");
            assert!(
                !output.stdout.is_empty(),
                "{shell} produced no completion script"
            );
        }
    }

    #[test]
    fn completions_is_hidden_from_top_level_help() {
        let output = bin().arg("--help").output().expect("run");
        let help = String::from_utf8_lossy(&output.stdout);
        assert!(!help.contains("completions"));
        assert!(!help.contains("\n  man"));
    }

    // ---- `man` subcommand. ----

    #[test]
    fn man_subcommand_prints_a_nonempty_roff_page() {
        let output = bin().arg("man").output().expect("run");
        assert!(output.status.success());
        let page = String::from_utf8_lossy(&output.stdout);
        assert!(page.contains(".TH"));
        assert!(page.contains(".SH \"THE COMBINE ALGORITHM\""));
    }

    // ---- (b) dynamic value completion, exercised through the real
    // binary via the `COMPLETE=<shell>` protocol clap_complete's
    // `unstable-dynamic` engine uses (see `completions` module docs). One
    // env var per word of shell protocol state, plus the typed so far
    // words appended after `--`, mirroring what the generated bash
    // integration snippet itself sends when a real shell asks for a
    // completion at the given word index. ----

    /// Ask the binary to complete word `index` of `words` (0-based,
    /// including the program name at index 0), under a registry rooted at
    /// `xdg_config_home` (so `--set` completion has something to find).
    fn dynamic_complete(xdg_config_home: &std::path::Path, words: &[&str], index: usize) -> String {
        let output = bin()
            .env("COMPLETE", "bash")
            .env("XDG_CONFIG_HOME", xdg_config_home)
            // Deliberately blanked so an ambient $XDG_CONFIG_DIRS on the
            // machine running the tests can't leak in a real registry:
            // mirrors `tests/common::isolate_xdg`.
            .env("XDG_CONFIG_DIRS", "")
            .env("_CLAP_COMPLETE_INDEX", index.to_string())
            .env("_CLAP_COMPLETE_COMP_TYPE", "9")
            .env("_CLAP_COMPLETE_SPACE", "true")
            .arg("--")
            .args(words)
            .output()
            .expect("run");
        assert!(output.status.success(), "dynamic completion exited nonzero");
        String::from_utf8(output.stdout).expect("utf8")
    }

    #[test]
    fn dynamic_completion_offers_registered_set_names() {
        let xdg = TempDir::new().unwrap();
        let registry_dir = xdg.path().join("silktouch");
        std::fs::create_dir_all(&registry_dir).unwrap();
        std::fs::write(
            registry_dir.join("registry.toml"),
            "[sets.claude]\nfragments = \"/tmp/does-not-need-to-exist\"\noutput = \"/tmp/out.json\"\n",
        )
        .unwrap();

        // --set only exists on a subcommand's TargetArgs, not on the root
        // Cli, so a subcommand must come first: silktouch(0) combine(1)
        // --set(2) claude prefix(3), completing word 3.
        let candidates = dynamic_complete(xdg.path(), &["silktouch", "combine", "--set", "cla"], 3);
        assert_eq!(candidates.trim(), "claude");
    }

    #[test]
    fn dynamic_completion_offers_merge_strategies_after_equals() {
        let xdg = TempDir::new().unwrap();
        let candidates = dynamic_complete(
            xdg.path(),
            &["silktouch", "combine", "--merge", "/some/list=rep"],
            3,
        );
        assert_eq!(candidates.trim(), "/some/list=replace");
    }

    #[test]
    fn dynamic_completion_is_silent_for_a_missing_registry() {
        let xdg = TempDir::new().unwrap();
        let candidates = dynamic_complete(xdg.path(), &["silktouch", "combine", "--set", ""], 3);
        assert_eq!(candidates.trim(), "");
    }

    #[test]
    fn dynamic_completion_offers_json_fragments_for_catch_all() {
        let root = TempDir::new().unwrap();
        let frags = root.path().join("frags");
        std::fs::create_dir_all(&frags).unwrap();
        std::fs::write(frags.join("10-base.json"), "{}").unwrap();
        std::fs::write(frags.join("99-local.json"), "{}").unwrap();

        let xdg = TempDir::new().unwrap();
        let candidates = dynamic_complete(
            xdg.path(),
            &[
                "silktouch",
                "combine",
                "--fragments",
                frags.to_str().unwrap(),
                "-o",
                "-",
                "--catch-all",
                "",
            ],
            7,
        );
        let mut lines: Vec<&str> = candidates.lines().collect();
        lines.sort_unstable();
        assert_eq!(lines, vec!["10-base.json", "99-local.json"]);
    }

    #[test]
    fn dynamic_completion_is_silent_for_a_nonexistent_fragment_directory() {
        let xdg = TempDir::new().unwrap();
        let candidates = dynamic_complete(
            xdg.path(),
            &[
                "silktouch",
                "combine",
                "--fragments",
                "/nonexistent/fragments/dir",
                "-o",
                "-",
                "--catch-all",
                "",
            ],
            7,
        );
        assert_eq!(candidates.trim(), "");
    }

    // ---- `--comments` / `--indent`: CLI overrides the registry, which
    // overrides the crate default. ----------------------------------------

    fn fragments_dir(contents: &[(&str, &str)]) -> TempDir {
        let dir = TempDir::new().unwrap();
        for (name, text) in contents {
            std::fs::write(dir.path().join(name), text).unwrap();
        }
        dir
    }

    fn write_registry(xdg_config_home: &std::path::Path, toml_text: &str) {
        let path = xdg_config_home.join("silktouch").join("registry.toml");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, toml_text).unwrap();
    }

    /// A registry driven run never touches state/data/cache. Isolate the
    /// other three XDG vars too, so no ambient registry on the machine
    /// running the tests can leak in.
    fn isolated(cmd: &mut Command, root: &std::path::Path) -> std::path::PathBuf {
        let config_home = root.join("xdg-config");
        cmd.env("XDG_CONFIG_HOME", &config_home)
            .env("XDG_CONFIG_DIRS", "")
            .env("XDG_STATE_HOME", root.join("xdg-state"))
            .env("XDG_DATA_HOME", root.join("xdg-data"))
            .env("XDG_CACHE_HOME", root.join("xdg-cache"));
        config_home
    }

    #[test]
    fn cli_comments_flag_overrides_the_registry_default() {
        // A fragment with a comment, and no --comments/registry setting at
        // all: `combine` must fail exactly as it did before this feature
        // existed (Comments::Forbid is the crate default).
        let frags = fragments_dir(&[("10-base.json", "{\n  \"a\": 1 // note\n}\n")]);
        let root = TempDir::new().unwrap();
        let output = root.path().join("out.json");
        bin()
            .args([
                "combine",
                "--fragments",
                frags.path().to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
            ])
            .assert()
            .code(2);
        assert!(!output.exists());

        // The same run with --comments protect succeeds, and the combined
        // output holds no comment.
        bin()
            .args([
                "combine",
                "--fragments",
                frags.path().to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--comments",
                "protect",
            ])
            .assert()
            .success();
        let written = std::fs::read_to_string(&output).unwrap();
        assert!(!written.contains("//"), "{written}");
        let value: serde_json::Value = serde_json::from_str(&written).unwrap();
        assert_eq!(value, serde_json::json!({"a": 1}));
    }

    #[test]
    fn cli_comments_flag_overrides_a_registry_set_to_forbid() {
        let frags = fragments_dir(&[("10-base.json", "{\n  \"a\": 1 // note\n}\n")]);
        let root = TempDir::new().unwrap();
        let output = root.path().join("out.json");

        let registry_toml = format!(
            "comments = \"forbid\"\n\n[sets.test]\nfragments = {:?}\noutput = {:?}\n",
            frags.path().to_str().unwrap(),
            output.to_str().unwrap(),
        );

        let mut cmd = bin();
        let xdg = isolated(&mut cmd, root.path());
        write_registry(&xdg, &registry_toml);
        cmd.args(["combine", "--set", "test", "--comments", "protect"])
            .assert()
            .success();

        assert!(output.exists());
    }

    #[test]
    fn cli_comments_flag_rejects_an_unknown_value_naming_it() {
        let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().unwrap();
        let output = root.path().join("out.json");
        let assert = bin()
            .args([
                "combine",
                "--fragments",
                frags.path().to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--comments",
                "yolo",
            ])
            .assert()
            .code(2);
        let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
        assert!(stderr.contains("yolo"), "{stderr:?}");
    }

    #[test]
    fn cli_indent_flag_overrides_a_registry_sets_own_indent() {
        let frags = fragments_dir(&[("10-base.json", r#"{"a":1}"#)]);
        let root = TempDir::new().unwrap();
        let output = root.path().join("out.json");

        // The set's own indent (8) would win over any global default, but
        // --indent must win over that in turn.
        let registry_toml = format!(
            "indent = 4\n\n[sets.test]\nfragments = {:?}\noutput = {:?}\nindent = 8\n",
            frags.path().to_str().unwrap(),
            output.to_str().unwrap(),
        );

        let mut cmd = bin();
        let xdg = isolated(&mut cmd, root.path());
        write_registry(&xdg, &registry_toml);
        cmd.args(["combine", "--set", "test", "--indent", "3"])
            .assert()
            .success();

        let written = std::fs::read_to_string(&output).unwrap();
        assert_eq!(written, "{\n   \"a\": 1\n}\n", "expected 3-space indent");
    }
}
