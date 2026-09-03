//! Error type for silktouch.
//!
//! Invariant: every variant must name a path or a JSON pointer in its
//! message, so failures are always actionable without extra context.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("fragment directory missing: {0}")]
    FragmentDirMissing(PathBuf),

    #[error("no fragments found in {0} (pass --allow-empty)")]
    NoFragments(PathBuf),

    #[error("fragment root is not an object: {path} (found {found})")]
    FragmentRootNotObject { path: PathBuf, found: &'static str },

    #[error("output root is not an object: {path} (found {found})")]
    OutputRootNotObject { path: PathBuf, found: &'static str },

    #[error(
        "{path} contains comments, which are not enabled. Set comments = \"protect\" \
         (or pass --comments protect) to allow them"
    )]
    CommentsNotEnabled { path: PathBuf },

    #[error("failed to parse {path}")]
    Parse {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },

    #[error("I/O error on {path}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("output file missing: {0}")]
    OutputMissing(PathBuf),

    #[error(
        "{verb} needs a real output file to read. -o - writes to stdout and only works for combine"
    )]
    OutputIsStdout { verb: &'static str },

    #[error("catch all fragment {catch_all} does not sort last (after {after})")]
    CatchAllNotLast { catch_all: String, after: String },

    #[error("bad JSON pointer: {0}")]
    BadPointer(String),

    #[error("bad merge strategy {1:?} for pointer {0}")]
    BadStrategy(String, String),

    #[error("fragment name is not valid UTF-8: {path:?}")]
    NonUtf8Name { path: PathBuf },

    #[error("route type conflict at {0}")]
    RouteTypeConflict(String),

    #[error("unsupported patch operation: {0}")]
    UnsupportedOp(&'static str),

    #[error("placement conflict: {path} already exists and does not point at {output}")]
    PlacementConflict { path: PathBuf, output: PathBuf },

    #[error(
        "cannot create symlink at {target}: enable Developer Mode, or use placement = \"none\" with output pointed at the target: {source}"
    )]
    SymlinkUnsupported {
        target: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("failed to parse registry {path}")]
    RegistryParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("unknown set {name:?} (registry: {path})")]
    UnknownSet { name: String, path: PathBuf },

    #[error("set {name:?} has placement = \"symlink\" but no target (registry: {path})")]
    PlacementMissingTarget { name: String, path: PathBuf },

    #[error("failed to expand {var} in {raw:?}")]
    Expand { var: String, raw: String },

    #[error("unknown comments mode {0:?} (expected \"forbid\", \"protect\", or \"strip\")")]
    UnknownCommentsMode(String),

    #[error(
        "{path} holds comments and this change would rewrite it, dropping them. Move the key to a comment free fragment, or set comments = \"strip\""
    )]
    CommentsWouldBeLost { path: PathBuf },
}

pub type Result<T> = std::result::Result<T, Error>;
