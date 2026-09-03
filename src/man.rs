//! `silktouch man`: renders `silktouch(1)`.
//!
//! `--help` answers "what are the flags". This answers "what does this
//! thing actually do to my files". The mechanical sections, NAME,
//! SYNOPSIS, OPTIONS, SUBCOMMANDS, come straight from `clap_mangen`
//! rendering `Cli::command()`, so they can never drift from the real CLI.
//! Everything after that is hand authored roff, appended as plain string
//! constants: the algorithm prose `--help` has no room for. It covers the
//! same ground as the README, at man page depth.
//!
//! Deliberately **not** routed through clap's `after_long_help`: that would
//! render straight into `--help` too, and bloat it with material `--help`
//! readers did not ask for. This module is the only place this prose lives.

use std::io::{self, Write};

use clap::Command;

/// Render the full man page to `out`: `clap_mangen`'s mechanical sections,
/// then the hand-authored ones below, in a fixed order.
pub fn render(cmd: Command, out: &mut dyn Write) -> io::Result<()> {
    let man = clap_mangen::Man::new(cmd).manual("silktouch manual");

    man.render_title(out)?;
    man.render_name_section(out)?;
    man.render_synopsis_section(out)?;
    man.render_options_section(out)?;
    man.render_subcommands_section(out)?;

    for section in SECTIONS {
        out.write_all(section.as_bytes())?;
    }
    Ok(())
}

/// Hand-authored sections, in the order they render.
const SECTIONS: &[&str] = &[
    DESCRIPTION,
    COMBINE_ALGORITHM,
    ARRAY_STRATEGIES,
    SPLIT_ALGORITHM,
    LAWS,
    EXIT_STATUS,
    FILES,
    CONFIGURATION,
    EXAMPLES,
    LIMITATIONS,
    SEE_ALSO,
];

const DESCRIPTION: &str = r"
.SH DESCRIPTION
Many programs keep their configuration in one file that the program itself
also rewrites at runtime. That combination defeats hand authoring: there
is no way to split the file into modular, version controlled pieces,
because the program overwrites the whole thing wholesale whenever it feels
like it.
.PP
The systemd drop in convention (\fIfoo.d/*.json\fR merged into one
effective file) solves half of this problem, the authoring half.
Nothing solves the other half: no widely available tool merges an
externally modified output file \fBback\fR into the fragments it came
from, so that a program's own runtime writes can survive being
version controlled at all.
.PP
\fBsilktouch\fR does both directions. It carries no knowledge of any
particular program. It operates purely on JSON structure. \fBcombine\fR
folds every fragment in a directory into one output file. \fBsplit\fR runs
the same relation the other way: it reads that output file back after
something else has edited it, typically the program itself, at
runtime, and routes every difference into the fragment that should own
it.
.PP
\fBsilktouch completions\fR \fISHELL\fR and \fBsilktouch man\fR print a
shell completion script and this manual page, respectively. Both are
hidden from \fB\-\-help\fR to keep the four verbs above legible at a
glance, but they are ordinary subcommands, invoked and documented like any
other.
";

const COMBINE_ALGORITHM: &str = r#"
.SH "THE COMBINE ALGORITHM"
\fBcombine\fR discovers every file named \fI*.json\fR directly inside the
fragment directory (not recursively). Filenames beginning with a period are
skipped.
.PP
The discovered fragments are sorted by filename, \fBas raw bytes\fR. This
is not a numeric or locale aware sort. Prefixing fragments with two digit
numbers (\fI10\-base.json\fR, \fI20\-hooks.json\fR, \fI99\-local.json\fR)
is a convention its users adopt so that byte order happens to match the
intended precedence order. The sort itself knows nothing about the
convention, and does not need to.
.PP
Every fragment's root value must be a JSON \fBobject\fR. A fragment whose
root is an array, string, number, boolean or null is an error, naming the
offending file. Nothing is written.
.PP
The sorted fragments are then folded left to right with a deep merge, one
pair of values at a time, by three rules:
.TP
.B "object + object"
Recurse into the merge, key by key.
.TP
.B "array + array"
Resolved by the array strategy registered for that JSON Pointer path:
see \fBARRAY STRATEGIES\fR below.
.TP
.B "everything else"
Including a type mismatch (an object meeting a scalar, an array meeting an
object, and so on): the later fragment's value wins outright, replacing
the earlier one in full.
.PP
The output document's key order is \fBfirst seen insertion order\fR
across the entire fold, the order in which each key first appeared,
not the order of whichever fragment last touched it. This is why
silktouch requires \fIserde_json\fR's \fBpreserve_order\fR feature:
without it, key order would depend on hashing, and two runs over
byte identical input could disagree.
"#;

const ARRAY_STRATEGIES: &str = r#"
.SH "ARRAY STRATEGIES"
When two fragments both set an array at the same JSON Pointer path,
silktouch needs a rule to combine them, because there is no single sane
default for every kind of array: some are option lists that should
accumulate across fragments. Some are ordered pipelines where a later
fragment should replace the earlier one outright.
.TP
.B concat\-dedupe
The default. Concatenate the arrays in fragment order, then drop later
occurrences of any value already seen, comparing by full structural
equality. The first occurrence of a value fixes its position. Later
duplicates are silently dropped.
.TP
.B replace
The later fragment's array replaces the earlier one wholesale. Nothing
from the earlier array survives, even values also present in the later
one.
.PP
The strategy for a given path is chosen by matching JSON Pointers, with
\fB*\fR as a wildcard matching exactly one path segment (\fI/servers/*/args\fR
matches \fI/servers/foo/args\fR and \fI/servers/bar/args\fR, but not
\fI/servers/foo/bar/args\fR). When more than one registered pattern
matches a path, the most specific match wins.
"#;

const SPLIT_ALGORITHM: &str = r#"
.SH "THE SPLIT ALGORITHM"
\fBcombine\fR is a pure, deterministic function of the fragments: the same
fragment directory contents always produce the same output, byte for
byte. That is the whole reason \fBsplit\fR needs \fBno state file\fR. It
recomputes \fIbase\fR, the output combine would produce right now, then
diffs \fIbase\fR against the real, possibly hand edited, output file on
disk, and routes every difference back into whichever fragment should own
it. There is no lockfile, no snapshot of a previous run, and no cache
directory: the fragments and the output file are the only state that
exists. (This is also why editing a fragment and the output file between
runs is unresolvable: see \fBLIMITATIONS\fR.)
.PP
Each JSON Pointer path is matched against one of four cases:
.TP
.B "in both, both objects"
Recurse into the object, applying these same four cases key by key.
.TP
.B "in both, differing otherwise"
A different scalar, a type change, or a differing array under the
\fBreplace\fR strategy: overwrite the value at the \fBowner\fR fragment
(below).
.TP
.B "in the output only"
A key the output file has that no fragment declares: insert it into the
catch all fragment.
.TP
.B "in the fragments' base only"
A key some fragment still declares, but the output file no longer has:
delete it from \fBevery\fR fragment that declares it.
.PP
\fBOwner\fR means the highest precedence fragment, the last one, by
filename sort order, that declares a value at that exact path. At
least one fragment declares it, by construction of \fIbase\fR. A fragment
earlier in the fold that also set the same path is left alone: its value
was already shadowed by the merge, and touching it again changes nothing
observable.
.PP
Arrays under \fBconcat\-dedupe\fR are routed \fBelement wise, by
membership\fR, not by index: an element present in \fIbase\fR but missing
from the output is removed from \fBevery\fR fragment that contributes it.
An element present in the output but missing from \fIbase\fR is appended
to the catch all fragment's array at that path. Membership routing, not
positional routing, is what makes sense here, because \fBconcat\-dedupe\fR
already discards position across fragment boundaries when it concatenates.
See \fBLAWS\fR below for the round trip consequence. Arrays under
\fBreplace\fR are simpler: the whole array is overwritten at the owner,
exactly like any other differing scalar.
.PP
The \fBcatch all\fR fragment absorbs anything with nowhere else to go:
new keys, and newly appended array elements. It defaults to
\fI99\-local.json\fR, and is created if it does not already exist. Its
filename \fBmust sort after every other fragment's filename\fR. If it does
not, \fBsplit\fR refuses to start, naming the offending filename:
otherwise whatever it absorbed today would be shadowed by a later fragment
on the very next \fBcombine\fR, silently breaking the round trip.
"#;

const LAWS: &str = r#"
.SH LAWS
silktouch is designed to satisfy three laws. They are also, word for
word, the crate's property tests.
.TP
.B "Law A: inverse"
\fBsplit(F, combine(F)) == F\fR, byte identical. If nothing has touched
the output file since the last \fBcombine\fR, splitting it back reproduces
the fragments exactly. Running \fBsplit\fR when nothing changed is a
safe no op.
.TP
.B "Law B: absorption"
\fBcombine(split(F, O)) == O\fR. An external edit to the output file
survives the round trip through the fragments and back. This holds
exactly for objects and scalar values. For arrays under \fBconcat\-dedupe\fR
it holds only \fBup to element order\fR: \fBcombine\fR always emits array
elements in fragment order, so once an element has been spread across
fragments (or newly appended to the catch all), a later \fBcombine\fR
reconstructs the same \fIset\fR of elements, but not necessarily in
whatever order an external program last wrote them in. Membership is what
a drop in style array means to silktouch. Ordering is not something the
round trip preserves once elements live in more than one fragment.
.TP
.B "Law C: idempotence"
\fBsplit(split(F, O), O) == split(F, O)\fR. Running \fBsplit\fR twice
against the same output file has no additional effect after the first
run: the second run diffs an already reconciled \fIbase\fR against the
same \fIO\fR, and finds nothing left to route.
"#;

const EXIT_STATUS: &str = r#"
.SH "EXIT STATUS"
\fBdiff\fR follows the \fBdiff\fR(1) convention:
.TP
.B 0
The fragments and the output file agree.
.TP
.B 1
They differ. The differences were printed to standard output.
.TP
.B 2
An error occurred: a missing fragment directory, a malformed fragment,
an unreadable output file, and so on.
.PP
Every other subcommand (\fBcombine\fR, \fBsplit\fR, \fBsync\fR) uses the
ordinary convention instead: 0 on success, 2 on error. There is no exit
status 1 outside \fBdiff\fR.
"#;

const FILES: &str = r#"
.SH FILES
.TP
.I $XDG_CONFIG_HOME/silktouch/registry.toml
The registry, tried first. Defaults to \fI~/.config/silktouch/registry.toml\fR
when \fB$XDG_CONFIG_HOME\fR is unset or is not an absolute path.
.TP
.I $XDG_CONFIG_DIRS
Searched next, as a read only fallback, in the order listed. The first
\fIregistry.toml\fR found there wins.
.TP
.B "\-\-config PATH"
Overrides discovery outright and is tried instead of anything above.
.PP
silktouch writes nothing to \fB$XDG_STATE_HOME\fR, \fB$XDG_DATA_HOME\fR or
\fB$XDG_CACHE_HOME\fR, and never creates those directories: the
stateless design described in \fBTHE SPLIT ALGORITHM\fR leaves nothing to
put there. The only files silktouch ever writes are the fragments
themselves, the combined output file, and, with \fBplacement =
"symlink"\fR in the registry, one symlink.
"#;

const CONFIGURATION: &str = r#"
.SH CONFIGURATION
All configuration lives in one \fIregistry.toml\fR, a table of named sets
plus two global defaults. Paths expand \fB~\fR and \fB$VARIABLES\fR. A
worked example, covering all three ways a set's output can reach the
program that reads it:
.PP
.RS 4
.nf
.na
indent = 2
catch_all = "99\-local.json"          # global default

# A \(em silktouch owns the file outright, wherever you point it.
[sets.claude]
fragments = "$DOTFILES/silktouch/claude"
output    = "~/.claude/settings.json"     # created, parent dirs and all

[sets.claude.merge]
"/permissions/allow" = "concat\-dedupe"    # the default, shown for clarity
"/some/ordered/list" = "replace"

# B \(em silktouch generates into a dotfiles repo; stow places it.
[sets.nvim]
fragments = "$DOTFILES/silktouch/nvim"
output    = "$DOTFILES/nvim/.config/nvim/settings.json"
placement = "none"                        # default
catch_all = "zz\-local.json"               # per\-set override

# C \(em silktouch generates into the repo and places it itself.
[sets.helix]
fragments = "$DOTFILES/silktouch/helix"
output    = "$DOTFILES/helix/.config/helix/config.json"
placement = "symlink"
target    = "~/.config/helix/config.json"
.ad
.fi
.RE
.PP
Set A points \fIoutput\fR straight at the real path the program reads.
silktouch needs no help from anything else. Set B (placement \fBnone\fR,
the default) has silktouch touch only \fIoutput\fR inside the repository.
Something else, typically \fBstow\fR(8), is responsible for
getting it to \fI$HOME\fR. Set C has silktouch both generate \fIoutput\fR
inside the repository \fBand\fR maintain a symlink at \fItarget\fR
pointing at it, with no help from \fBstow\fR(8) at all.
.PP
Merge strategy keys are JSON Pointers, matched as described in \fBARRAY
STRATEGIES\fR above. A set's own \fImerge\fR table, and its own
\fIcatch_all\fR, override the registry's global defaults.
"#;

const EXAMPLES: &str = r#"
.SH EXAMPLES
Pipe the combined document straight into \fBjq\fR(1), touching no files at
all:
.PP
.RS 4
.nf
.na
silktouch combine \-\-fragments d/ \-o \- | jq .permissions.allow
.ad
.fi
.RE
.PP
A pre commit hook that fails the commit while a set's fragments and
output file disagree, exactly what \fBdiff\fR(1) style exit codes are
for:
.PP
.RS 4
.nf
.na
#!/bin/sh
silktouch diff \-\-set claude || {
    echo "claude settings out of sync; run: silktouch sync \-\-set claude" >&2
    exit 1
}
.ad
.fi
.RE
.PP
Bootstrapping a set from a config file a program has already been writing
to for years, using \fBstow\fR(8) to adopt it into a dotfiles repository,
then seeding the fragments from its current contents with a single
\fBsplit\fR:
.PP
.RS 4
.nf
.na
stow \-\-adopt claude
silktouch split \-\-set claude
.ad
.fi
.RE
"#;

const LIMITATIONS: &str = r"
.SH LIMITATIONS
If a fragment \fBand\fR the output file both change between runs,
\fBsplit\fR treats the fragments as the base and the output file as the
truth, so the fragment side edit is overwritten silently: there is no
snapshot of the last \fBcombine\fR to detect the conflict against. See
\fBTHE SPLIT ALGORITHM\fR above. Mitigation: run \fBsync\fR often, or run
\fBdiff\fR first and look before routing anything.
";

const SEE_ALSO: &str = r#"
.SH "SEE ALSO"
.BR stow (8),
.BR jq (1),
.BR git (1)
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// A stand-in for `main.rs`'s real `Cli`, built the same way
    /// (`derive(Parser)`, a `Command` subcommand, `version`), so this
    /// module's own tests don't need to reach into the bin crate.
    #[derive(clap::Parser)]
    #[command(name = "silktouch", version)]
    struct TestCli {
        #[command(subcommand)]
        #[allow(dead_code)]
        command: TestCommand,
    }

    #[derive(clap::Subcommand)]
    enum TestCommand {
        Combine,
        Split,
        Sync,
        Diff,
        #[command(hide = true)]
        Completions,
        #[command(hide = true)]
        Man,
    }

    fn rendered() -> String {
        let mut out = Vec::new();
        render(TestCli::command(), &mut out).expect("render");
        String::from_utf8(out).expect("utf8")
    }

    /// Drift guard: does not prove the prose is right, but catches a
    /// subcommand or law added without a mention here.
    #[test]
    fn mentions_every_subcommand() {
        let page = rendered();
        for name in ["combine", "split", "sync", "diff"] {
            assert!(page.contains(name), "man page missing {name:?}");
        }
    }

    #[test]
    fn mentions_all_exit_codes() {
        let page = rendered();
        assert!(page.contains(".SH \"EXIT STATUS\""));
        for code in [".TP\n.B 0", ".TP\n.B 1", ".TP\n.B 2"] {
            assert!(page.contains(code), "man page missing exit code {code:?}");
        }
    }

    #[test]
    fn mentions_all_three_laws() {
        let page = rendered();
        for law in [
            "Law A",
            "Law B",
            "Law C",
            "inverse",
            "absorption",
            "idempotence",
        ] {
            assert!(page.contains(law), "man page missing {law:?}");
        }
    }

    #[test]
    fn renders_without_error_and_is_nonempty() {
        let page = rendered();
        assert!(!page.is_empty());
        assert!(page.contains(".TH"));
    }

    #[test]
    fn no_line_starts_with_an_unescaped_control_character() {
        // Every hand-authored section must not hand groff a bare `.` or
        // `'` at the start of a line unless it's an actual request (this
        // repo's sections only ever use requests groff knows, so any line
        // starting with those characters here is deliberate roff, not
        // prose that slipped past escaping).
        for section in SECTIONS {
            for line in section.lines() {
                if let Some(rest) = line.strip_prefix('.') {
                    let request: String = rest.chars().take_while(|c| c.is_alphabetic()).collect();
                    assert!(
                        !request.is_empty(),
                        "unescaped leading '.' in section: {line:?}"
                    );
                }
            }
        }
    }
}
