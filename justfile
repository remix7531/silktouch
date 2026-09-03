# Task runner for silktouch.
#
# Expected environment: `nix develop` (there is no `cargo`, `cargo-deny`,
# `cargo-machete`, `typos`, or `nix` guaranteed on a bare PATH here: the
# flake's devShells.default provides all of them). Run recipes as:
#
#   nix develop -c just all
#   nix develop -c just clippy
#
# or, from inside an already entered `nix develop` shell, just `just all`.
#
# These recipes are the source of truth for the commands CI runs. The
# fmt/clippy/test/doc/machete jobs in .github/workflows/ci.yml invoke
# `just <recipe>` directly (after installing `just` via
# taiki-e/install-action) so the two cannot drift. The deny/typos jobs use
# their dedicated marketplace actions (EmbarkStudios/cargo-deny-action,
# crate-ci/typos-action) instead, since those provide caching/annotations
# a raw command doesn't: `just deny` / `just typos` below are the
# equivalent raw commands for local use and are kept identical by hand.
# The nix-build/flake-check CI jobs run `nix build` / `nix flake check`
# directly (no `just` involved) since they're already single commands
# with no drift surface. `just nix-build` mirrors that plus the artefact
# assertions for local use.

set shell := ["bash", "-uc"]

export RUSTFLAGS := "-Dwarnings"
export CARGO_INCREMENTAL := "0"
export RUSTDOCFLAGS := "-Dwarnings"

# Running bare `just` lists the recipes rather than running everything,
# since `all` is slow (compiles, builds the Nix package, etc.) and
# listing is the safer default for an unfamiliar invocation.
default:
    @just --list

# Run the full check suite CI runs, fast/cheap first so it fails early.
all: fmt clippy test doc deny machete typos nix-build

# --- fast, cargo-only checks -------------------------------------------

fmt:
    cargo fmt --all --check

# Applies formatting, unlike `fmt` above which only checks it.
fmt-fix:
    cargo fmt --all

clippy:
    cargo clippy --all-targets --all-features -- -Dwarnings

test:
    cargo test --all-features

doc:
    cargo doc --no-deps --all-features

# --- slower supply-chain / hygiene checks -------------------------------

deny:
    cargo deny check

machete:
    cargo machete

typos:
    typos

# --- slowest: builds the Nix package and checks its installed artefacts -

nix-build:
    nix build
    test -x result/bin/silktouch
    test -f result/share/man/man1/silktouch.1.gz
    test -f result/share/bash-completion/completions/silktouch.bash
    test -f result/share/zsh/site-functions/_silktouch
    test -f result/share/fish/vendor_completions.d/silktouch.fish
    @echo "nix-build: all expected artefacts present"

flake-check:
    nix flake check
