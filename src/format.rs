//! Format boundary: confines all parse/serialise behaviour behind one trait.
//!
//! This exists so that adding TOML/YAML later touches only this module. v1
//! is JSON only.

use std::path::Path;
use std::str::FromStr;

use serde::Serialize;
use serde_json::Value;

use crate::error::{Error, Result};

/// How comments in fragment (and output) text are treated.
///
/// Comments are never part of the [`Value`] model. `serde_json` discards
/// them at parse, so a fragment `split`/`route` never touches, keeps
/// whatever comments it had by construction. The interesting case is a
/// fragment that *does* get rewritten: its comments would otherwise vanish
/// silently at exactly the moment the tool edits the user's file. This
/// setting controls what happens then. See [`crate::fragment::FragmentSet::write_back`]
/// for where the [`Comments::Protect`] check actually fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Comments {
    /// A comment in a fragment or output file is a parse error naming the
    /// file, exactly today's (pre JSONC) behaviour. The default, so no
    /// existing behaviour changes and nobody loses a comment without
    /// opting in.
    #[default]
    Forbid,
    /// Comments are accepted. If `split`/`route` needs to rewrite a
    /// fragment that had comments, that is [`Error::CommentsWouldBeLost`]
    /// naming the file, rather than silently dropping them.
    Protect,
    /// Comments are accepted. A rewritten fragment silently loses its
    /// comments.
    Strip,
}

impl FromStr for Comments {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "forbid" => Ok(Self::Forbid),
            "protect" => Ok(Self::Protect),
            "strip" => Ok(Self::Strip),
            other => Err(Error::UnknownCommentsMode(other.to_string())),
        }
    }
}

/// Options controlling how a [`Format`] parses text into a [`Value`]. The
/// read side equivalent of [`WriteOpts`], kept as its own struct (rather
/// than a bare `Comments` parameter) so a later format specific read option
/// has somewhere to go without another `parse` signature change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ReadOpts {
    /// How comments in the text being parsed are treated.
    pub comments: Comments,
}

/// Options controlling how a [`Format`] serialises a [`Value`] back to text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteOpts {
    /// Number of spaces per indent level.
    pub indent: usize,
    /// Whether the serialised output ends with exactly one trailing `\n`.
    pub trailing_newline: bool,
}

impl Default for WriteOpts {
    fn default() -> Self {
        Self {
            indent: 2,
            trailing_newline: true,
        }
    }
}

/// A single file format's parse/serialise behaviour.
///
/// Object safe: used as `&dyn Format` so callers can pick a format at
/// runtime based on file extension.
pub trait Format {
    /// Short, lowercase name of the format, e.g. `"json"`.
    fn name(&self) -> &'static str;

    /// Canonical file extension for the format, e.g. `"json"`.
    fn extension(&self) -> &'static str;

    /// Parse `text` into a [`Value`], honouring `opts.comments`. `path` is
    /// used only to name the file in any resulting [`Error::Parse`] or
    /// [`Error::Io`].
    fn parse(&self, text: &str, path: &Path, opts: &ReadOpts) -> Result<Value>;

    /// Whether `text` holds comments in this format's comment dialect that
    /// a rewrite (serialising it again from the parsed [`Value`] alone) would
    /// discard. Used to populate `crate::fragment::Fragment::had_comments`
    /// at load time.
    ///
    /// Defaults to `false`: a format with no comment dialect (or none
    /// implemented yet) never has anything to protect.
    fn has_comments(&self, text: &str) -> bool {
        let _ = text;
        false
    }

    /// Serialise `value` to text according to `opts`.
    fn serialize(&self, value: &Value, opts: &WriteOpts) -> Result<String>;
}

/// The JSON format.
///
/// `parse` reads plain JSON when `opts.comments == Comments::Forbid`
/// (today's behaviour, unchanged: any comment is a [`Error::Parse`]), and
/// otherwise strips `//`, `/* */` (and, per the underlying crate, `#`)
/// comments first via [`json_strip_comments`]: **not** a JSON5 parser, so
/// nothing else about JSON5 (unquoted keys, single quoted strings, hex
/// numbers) is accepted. Only comments, plus trailing commas as a free side
/// effect of the same crate. See the README's "Comments in fragments"
/// section for why JSON5 was rejected: its unquoted keys and single quoted
/// strings would be silently rewritten into standard JSON the moment a
/// fragment is serialised again.
pub struct Json;

impl Format for Json {
    fn name(&self) -> &'static str {
        "json"
    }

    fn extension(&self) -> &'static str {
        "json"
    }

    fn parse(&self, text: &str, path: &Path, opts: &ReadOpts) -> Result<Value> {
        match opts.comments {
            Comments::Forbid => serde_json::from_str(text).map_err(|source| {
                // serde_json reports a stray `//` as "key must be a string",
                // which sends the reader hunting for a malformed key. If the
                // text really does hold comments, say that instead.
                if self.has_comments(text) {
                    Error::CommentsNotEnabled {
                        path: path.to_path_buf(),
                    }
                } else {
                    Error::Parse {
                        path: path.to_path_buf(),
                        source,
                    }
                }
            }),
            Comments::Protect | Comments::Strip => {
                let stripped = strip_comments(text, path)?;
                serde_json::from_str(&stripped).map_err(|source| Error::Parse {
                    path: path.to_path_buf(),
                    source,
                })
            }
        }
    }

    fn has_comments(&self, text: &str) -> bool {
        let mut stripped = text.to_string();
        match json_strip_comments::strip(&mut stripped) {
            // A malformed comment opener (a lone `/` not followed by `/` or
            // `*`) is reported by the crate as an error. Treat that
            // conservatively as "has comments" too, since whatever it is
            // would also change under a real strip then parse.
            Err(_) => true,
            Ok(()) => comment_diff(text.as_bytes(), stripped.as_bytes()),
        }
    }

    fn serialize(&self, value: &Value, opts: &WriteOpts) -> Result<String> {
        let mut buf = Vec::new();
        let indent = " ".repeat(opts.indent);
        let fmt = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
        let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
        // Serialising into an in memory Vec<u8> cannot fail: Value's Serialize
        // impl is infallible for JSON's data model, and there is no I/O.
        value
            .serialize(&mut ser)
            .expect("serializing a Value to an in-memory buffer cannot fail");

        // buf is guaranteed valid UTF-8: serde_json only ever writes UTF-8.
        let mut out = String::from_utf8(buf).expect("serde_json output is valid UTF-8");

        if opts.trailing_newline {
            out.push('\n');
        }

        Ok(out)
    }
}

/// Look up a [`Format`] implementation by file extension (without the
/// leading dot), e.g. `"json"` => `Some(&Json)`.
#[must_use]
pub fn for_extension(ext: &str) -> Option<&'static dyn Format> {
    match ext {
        "json" => Some(&Json),
        _ => None,
    }
}

/// Strip JSONC comments from `text` via [`json_strip_comments`], which
/// walks the input with a string literal aware state machine: unlike a
/// naive `//`/`/* */` search and replace, it does not misfire on `//`
/// inside a string, quoted plainly or immediately after an escaped quote.
/// See `tests::comment_inside_string_survives_intact` below.
///
/// The only failure mode is a malformed comment opener (a lone `/` not
/// followed by `/` or `*`), surfaced as [`Error::Io`] naming `path`:
/// there is no dedicated error variant for it because the underlying crate
/// itself reports it as `io::Error`, and it is rare enough (an actual `/`
/// standing alone in a fragment) not to warrant one.
fn strip_comments(text: &str, path: &Path) -> Result<String> {
    let mut stripped = text.to_string();
    json_strip_comments::strip(&mut stripped).map_err(|source| Error::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(stripped)
}

/// Whether stripping comments from `raw` (yielding `stripped`, byte for byte
/// the same length, comments are blanked to spaces in place, never
/// removed) changed anything beyond a lone trailing comma.
///
/// `json_strip_comments` also normalises a trailing comma before `}`/`]`
/// to a space, a side effect of the same crate, accepted here rather than
/// reimplemented. That is not a *comment*, so a fragment using only a
/// trailing comma must not count as
/// [`crate::fragment::Fragment::had_comments`]: distinguished here by the
/// fact that a real comment always opens with `/` or `#` and spans at least
/// its own opener, while the crate only ever blanks a bare trailing comma
/// as a single, isolated byte.
fn comment_diff(raw: &[u8], stripped: &[u8]) -> bool {
    debug_assert_eq!(raw.len(), stripped.len(), "strip must preserve length");
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == stripped[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < raw.len() && raw[i] != stripped[i] {
            i += 1;
        }
        let is_lone_trailing_comma = i - start == 1 && raw[start] == b',';
        if !is_lone_trailing_comma {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `serde_json` reports a stray `//` as "key must be a string". Under
    /// `Forbid` that sends the reader hunting for a malformed key instead of
    /// the comment they actually wrote, so the error names the real cause.
    #[test]
    fn forbid_reports_comments_as_comments() {
        let err = Json
            .parse(
                "{\n  // hi\n  \"a\": 1\n}",
                Path::new("f.json"),
                &ReadOpts {
                    comments: Comments::Forbid,
                },
            )
            .expect_err("a comment under Forbid must fail");
        assert!(matches!(err, Error::CommentsNotEnabled { .. }), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("f.json"), "{msg}");
        assert!(msg.contains("comments"), "{msg}");
    }

    /// ...but genuinely malformed JSON must still get the real parse error,
    /// not be mislabelled as a comment problem.
    #[test]
    fn forbid_still_reports_real_parse_errors() {
        let err = Json
            .parse(
                "{\"a\": }",
                Path::new("f.json"),
                &ReadOpts {
                    comments: Comments::Forbid,
                },
            )
            .expect_err("malformed JSON must fail");
        assert!(matches!(err, Error::Parse { .. }), "{err:?}");
    }

    /// `Error::Parse` carries its cause via `#[source]`. Embedding `{source}`
    /// in the Display string too made anyhow's `{:#}` print it twice.
    #[test]
    fn parse_error_display_does_not_repeat_its_source() {
        let err = Json
            .parse(
                "{\"a\": }",
                Path::new("f.json"),
                &ReadOpts {
                    comments: Comments::Forbid,
                },
            )
            .expect_err("malformed JSON must fail");
        let msg = err.to_string();
        assert!(
            !msg.contains("expected value"),
            "source leaked into Display: {msg}"
        );
    }
    use std::path::PathBuf;

    fn json() -> &'static dyn Format {
        &Json
    }

    fn forbid() -> ReadOpts {
        ReadOpts {
            comments: Comments::Forbid,
        }
    }

    fn protect() -> ReadOpts {
        ReadOpts {
            comments: Comments::Protect,
        }
    }

    fn strip() -> ReadOpts {
        ReadOpts {
            comments: Comments::Strip,
        }
    }

    #[test]
    fn round_trip() {
        let text = r#"{"b":1,"a":{"nested":[1,2,3]},"c":null}"#;
        let path = PathBuf::from("test.json");
        let value1 = json().parse(text, &path, &forbid()).unwrap();

        let opts = WriteOpts::default();
        let serialized = json().serialize(&value1, &opts).unwrap();

        let value2 = json().parse(&serialized, &path, &forbid()).unwrap();
        assert_eq!(value1, value2);
    }

    #[test]
    fn indent_four_spaces() {
        let path = PathBuf::from("test.json");
        let value = json().parse(r#"{"a":1}"#, &path, &forbid()).unwrap();
        let opts = WriteOpts {
            indent: 4,
            trailing_newline: false,
        };
        let s = json().serialize(&value, &opts).unwrap();
        assert_eq!(s, "{\n    \"a\": 1\n}");
    }

    #[test]
    fn indent_two_spaces() {
        let path = PathBuf::from("test.json");
        let value = json().parse(r#"{"a":1}"#, &path, &forbid()).unwrap();
        let opts = WriteOpts {
            indent: 2,
            trailing_newline: false,
        };
        let s = json().serialize(&value, &opts).unwrap();
        assert_eq!(s, "{\n  \"a\": 1\n}");
    }

    #[test]
    fn trailing_newline_behaviour() {
        let path = PathBuf::from("test.json");
        let value = json().parse(r#"{"a":1}"#, &path, &forbid()).unwrap();

        let with_nl = json()
            .serialize(
                &value,
                &WriteOpts {
                    indent: 2,
                    trailing_newline: true,
                },
            )
            .unwrap();
        assert!(with_nl.ends_with('\n'));
        assert!(!with_nl.ends_with("\n\n"));

        let without_nl = json()
            .serialize(
                &value,
                &WriteOpts {
                    indent: 2,
                    trailing_newline: false,
                },
            )
            .unwrap();
        assert!(!without_nl.ends_with('\n'));
    }

    #[test]
    fn key_order_preserved() {
        let path = PathBuf::from("test.json");
        let value = json().parse(r#"{"b":1,"a":2}"#, &path, &forbid()).unwrap();
        let opts = WriteOpts {
            indent: 2,
            trailing_newline: false,
        };
        let s = json().serialize(&value, &opts).unwrap();
        let pos_b = s.find("\"b\"").expect("key b present");
        let pos_a = s.find("\"a\"").expect("key a present");
        assert!(
            pos_b < pos_a,
            "expected \"b\" before \"a\" (preserve_order feature not active?): {s}"
        );
    }

    #[test]
    fn parse_error_names_the_file() {
        let path = PathBuf::from("/some/weird/path/broken.json");
        let err = json()
            .parse("{not valid json", &path, &forbid())
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("/some/weird/path/broken.json"),
            "error message should contain the path: {msg}"
        );
    }

    // ---- Comments (JSONC) ---------------------------------------------

    #[test]
    fn line_and_block_comments_are_stripped_under_protect_and_strip() {
        let path = PathBuf::from("test.json");
        let text = r#"{
            // a line comment
            "a": 1, /* a block
                       comment */
            "b": 2
        }"#;
        for opts in [protect(), strip()] {
            let value = json().parse(text, &path, &opts).unwrap();
            assert_eq!(value, serde_json::json!({"a": 1, "b": 2}));
        }
    }

    /// The crate choice regression test: a `//` inside a string literal,
    /// quoted plainly, or immediately after an escaped quote, must never
    /// be treated as a comment opener. A naive `//` search and truncate
    /// stripper corrupts both of these.
    #[test]
    fn comment_inside_string_survives_intact() {
        let path = PathBuf::from("test.json");

        let plain = r#"{"a": "x//y"}"#;
        let value = json().parse(plain, &path, &protect()).unwrap();
        assert_eq!(value, serde_json::json!({"a": "x//y"}));

        // The string is `x"//y` (an escaped quote immediately followed by
        // `//`): still not a comment, because the escaped quote does not
        // end the string.
        let escaped = r#"{"a": "x\"//y"}"#;
        let value = json().parse(escaped, &path, &protect()).unwrap();
        assert_eq!(value, serde_json::json!({"a": "x\"//y"}));
    }

    #[test]
    fn trailing_comma_is_accepted_as_a_free_side_effect() {
        let path = PathBuf::from("test.json");
        let value = json()
            .parse(r#"{"a": 1, "b": [1, 2,],}"#, &path, &protect())
            .unwrap();
        assert_eq!(value, serde_json::json!({"a": 1, "b": [1, 2]}));
    }

    #[test]
    fn forbid_mode_rejects_a_comment_naming_the_file() {
        let path = PathBuf::from("/frags/10-base.json");
        let err = json()
            .parse("{\"a\": 1} // trailing\n", &path, &forbid())
            .unwrap_err();
        // Not Error::Parse: serde_json's own message for a stray `//` is
        // "key must be a string", which describes a malformed key rather
        // than the comment the user actually wrote.
        assert!(matches!(err, Error::CommentsNotEnabled { .. }), "{err:?}");
        assert!(err.to_string().contains("10-base.json"), "{err}");
        assert!(err.to_string().contains("comments"), "{err}");
    }

    #[test]
    fn has_comments_true_for_line_and_block_comments() {
        assert!(json().has_comments("{\"a\": 1} // trailing\n"));
        assert!(json().has_comments("{/* x */\"a\": 1}"));
    }

    #[test]
    fn has_comments_false_for_plain_json_and_for_comment_lookalikes_in_strings() {
        assert!(!json().has_comments(r#"{"a": 1}"#));
        assert!(!json().has_comments(r#"{"a": "x//y"}"#));
        assert!(!json().has_comments(r#"{"a": "x\"//y"}"#));
    }

    #[test]
    fn has_comments_false_for_trailing_comma_alone() {
        assert!(!json().has_comments(r#"{"a": 1,}"#));
    }

    #[test]
    fn comments_from_str_parses_all_three_and_rejects_unknown_naming_the_value() {
        assert_eq!("forbid".parse::<Comments>().unwrap(), Comments::Forbid);
        assert_eq!("protect".parse::<Comments>().unwrap(), Comments::Protect);
        assert_eq!("strip".parse::<Comments>().unwrap(), Comments::Strip);

        let err = "yolo".parse::<Comments>().unwrap_err();
        assert!(err.to_string().contains("yolo"));
        match err {
            Error::UnknownCommentsMode(v) => assert_eq!(v, "yolo"),
            other => panic!("expected UnknownCommentsMode, got {other:?}"),
        }
    }

    #[test]
    fn comments_default_is_forbid() {
        assert_eq!(Comments::default(), Comments::Forbid);
    }
}
