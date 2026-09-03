//! RFC 6901 JSON Pointers, wildcard patterns using `*`, and per path strategy lookup.
//!
//! Three types live here:
//!
//! - [`Pointer`]: a parsed RFC 6901 pointer, stored as *decoded* segments.
//! - [`Pattern`]: a pointer shape where a whole segment may be `*`.
//! - [`StrategyTable`]: patterns to [`ArrayStrategy`], most specific match wins.
//!
//! # The `shift_remove` rule
//!
//! This crate enables `serde_json/preserve_order`, so [`serde_json::Map`] is an
//! `IndexMap` and `Map::remove` is `swap_remove`: it fills the hole with the
//! *last* entry, scrambling key order. Fragments are only rewritten when their
//! value actually changed, so a reordering deletion would dirty untouched files
//! and break law A (`split(F, combine(F)) == F`, byte identical). Every object
//! removal in this crate must therefore use `Map::shift_remove`.

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// A parsed RFC 6901 JSON Pointer, held as decoded segments.
///
/// Segments never contain `~0`/`~1` escapes. [`Pointer::as_str`] brings them
/// back, so `Pointer::parse(&p.as_str()) == p` for every well formed pointer.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Pointer(Vec<String>);

impl Pointer {
    /// Parse an RFC 6901 pointer. `""` is the root. `"/"` is a single empty
    /// segment. Anything else must start with `/`.
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Ok(Pointer(Vec::new()));
        }
        let Some(rest) = s.strip_prefix('/') else {
            return Err(Error::BadPointer(s.to_string()));
        };
        let segments = rest
            .split('/')
            .map(|raw| unescape(raw, s))
            .collect::<Result<Vec<String>>>()?;
        Ok(Pointer(segments))
    }

    /// The escaped textual form. Round trips with [`Pointer::parse`].
    #[must_use]
    pub fn as_str(&self) -> String {
        let mut out = String::new();
        for seg in &self.0 {
            out.push('/');
            out.push_str(&escape(seg));
        }
        out
    }

    /// The empty pointer, denoting the whole document.
    #[must_use]
    pub fn root() -> Self {
        Pointer(Vec::new())
    }

    /// Whether this is the empty (whole document) pointer.
    #[must_use]
    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    /// The decoded segments.
    #[must_use]
    pub fn segments(&self) -> &[String] {
        &self.0
    }

    /// This pointer extended by one decoded segment.
    #[must_use]
    pub fn child(&self, seg: &str) -> Self {
        let mut segments = self.0.clone();
        segments.push(seg.to_string());
        Pointer(segments)
    }

    /// This pointer with its last segment dropped. `None` at the root.
    #[must_use]
    pub fn parent(&self) -> Option<Self> {
        if self.0.is_empty() {
            None
        } else {
            Some(Pointer(self.0[..self.0.len() - 1].to_vec()))
        }
    }

    /// Self, then each ancestor, longest first, ending at the root.
    ///
    /// `/a/b` yields `/a/b`, `/a`, `""`.
    pub fn ancestors(&self) -> impl Iterator<Item = Pointer> + use<> {
        let segments = self.0.clone();
        (0..=segments.len())
            .rev()
            .map(move |n| Pointer(segments[..n].to_vec()))
    }

    /// Resolve against `v`, or `None` if any step is missing or mistyped.
    ///
    /// Array steps parse the segment as a decimal index. A segment that is not
    /// numeric, or is out of range, is a miss, not an error.
    #[must_use]
    pub fn resolve<'a>(&self, v: &'a Value) -> Option<&'a Value> {
        let mut cur = v;
        for seg in &self.0 {
            cur = match cur {
                Value::Object(map) => map.get(seg)?,
                Value::Array(arr) => arr.get(array_index(seg)?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// Mutable [`Pointer::resolve`].
    pub fn resolve_mut<'a>(&self, v: &'a mut Value) -> Option<&'a mut Value> {
        let mut cur = v;
        for seg in &self.0 {
            cur = match cur {
                Value::Object(map) => map.get_mut(seg)?,
                Value::Array(arr) => arr.get_mut(array_index(seg)?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    /// Resolve against `v`, creating missing steps as empty objects.
    ///
    /// Errors with [`Error::RouteTypeConflict`] naming the offending prefix if
    /// an existing container along the way is not an object. The value finally
    /// arrived at may be of any type if it already existed.
    pub fn ensure_object_path<'a>(&self, v: &'a mut Value) -> Result<&'a mut Value> {
        let mut cur = v;
        for (i, seg) in self.0.iter().enumerate() {
            let Value::Object(map) = cur else {
                return Err(Error::RouteTypeConflict(
                    Pointer(self.0[..i].to_vec()).as_str(),
                ));
            };
            cur = map
                .entry(seg.as_str())
                .or_insert_with(|| Value::Object(Map::new()));
        }
        Ok(cur)
    }

    /// Remove the value at this pointer, returning it.
    ///
    /// Object keys are removed with `shift_remove`, preserving the order of the
    /// surviving siblings. Array elements are removed with a shift. Emptied
    /// ancestors are **never** pruned: removing `/a/b/c` from
    /// `{"a":{"b":{"c":1}}}` leaves `{"a":{"b":{}}}`, so the next `combine`
    /// still emits the object.
    pub fn remove_from(&self, v: &mut Value) -> Option<Value> {
        let (last, init) = self.0.split_last()?;
        let parent = Pointer(init.to_vec()).resolve_mut(v)?;
        match parent {
            // shift_remove, never remove: see the module docs.
            Value::Object(map) => map.shift_remove(last),
            Value::Array(arr) => {
                let idx = array_index(last)?;
                if idx < arr.len() {
                    Some(arr.remove(idx))
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

/// Decode one raw pointer segment. `whole` is only used for the error message.
fn unescape(raw: &str, whole: &str) -> Result<String> {
    if !raw.contains('~') {
        return Ok(raw.to_string());
    }
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c != '~' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('0') => out.push('~'),
            Some('1') => out.push('/'),
            _ => return Err(Error::BadPointer(whole.to_string())),
        }
    }
    Ok(out)
}

/// Encode one decoded segment. `~` first, or the `/` escapes get double encoded.
fn escape(seg: &str) -> String {
    if seg.contains(['~', '/']) {
        seg.replace('~', "~0").replace('/', "~1")
    } else {
        seg.to_string()
    }
}

/// A segment read as an array index: decimal digits only, no sign, no space.
fn array_index(seg: &str) -> Option<usize> {
    if seg.is_empty() || !seg.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    seg.parse::<usize>().ok()
}

/// One segment of a [`Pattern`].
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum Seg {
    /// Matches exactly this decoded segment.
    Lit(String),
    /// Matches any one segment.
    Wild,
}

/// A pointer shape in which a whole segment may be `*`.
///
/// `*` is only a wildcard as an entire segment: `/servers/*/args` wildcards the
/// server name, while `/a*b` is the literal segment `a*b`. RFC 6901 has no
/// escape for `*`, so a literal single-`*` segment is not expressible.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default)]
pub struct Pattern(Vec<Seg>);

impl Pattern {
    /// Parse a pointer in which `*` may stand in for a whole segment.
    pub fn parse(s: &str) -> Result<Self> {
        if s.is_empty() {
            return Ok(Pattern(Vec::new()));
        }
        let Some(rest) = s.strip_prefix('/') else {
            return Err(Error::BadPointer(s.to_string()));
        };
        let segments = rest
            .split('/')
            .map(|raw| {
                if raw == "*" {
                    Ok(Seg::Wild)
                } else {
                    unescape(raw, s).map(Seg::Lit)
                }
            })
            .collect::<Result<Vec<Seg>>>()?;
        Ok(Pattern(segments))
    }

    /// The pattern segments.
    #[must_use]
    pub fn segments(&self) -> &[Seg] {
        &self.0
    }

    /// Whether `p` matches: same length, and every segment matches its own.
    #[must_use]
    pub fn matches(&self, p: &Pointer) -> bool {
        self.0.len() == p.0.len()
            && self.0.iter().zip(&p.0).all(|(pat, seg)| match pat {
                Seg::Lit(lit) => lit == seg,
                Seg::Wild => true,
            })
    }

    /// Number of literal (not wildcard) segments.
    fn literal_count(&self) -> usize {
        self.0.iter().filter(|s| matches!(s, Seg::Lit(_))).count()
    }

    /// Order two patterns by specificity, more specific being greater.
    ///
    /// More literals wins. On a tie, the pattern whose first differing segment
    /// is the literal one wins (leftmost literal). Only meaningful between
    /// patterns matching the same pointer, which are necessarily the same
    /// length, and two same length patterns that compare `Equal` here have
    /// identical wildcard positions and identical literals, so they are the
    /// same pattern. Specificity is therefore a total order on the candidates
    /// and the result never depends on insertion order.
    fn specificity_cmp(&self, other: &Pattern) -> std::cmp::Ordering {
        use std::cmp::Ordering;
        match self.literal_count().cmp(&other.literal_count()) {
            Ordering::Equal => {}
            ord => return ord,
        }
        for (a, b) in self.0.iter().zip(&other.0) {
            match (a, b) {
                (Seg::Lit(_), Seg::Wild) => return Ordering::Greater,
                (Seg::Wild, Seg::Lit(_)) => return Ordering::Less,
                _ => {}
            }
        }
        Ordering::Equal
    }
}

/// How two arrays at the same path are merged.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ArrayStrategy {
    /// Concatenate in fragment order, dropping later duplicates by full value
    /// equality and keeping the first occurrence.
    #[default]
    ConcatDedupe,
    /// The later fragment's array wholly replaces the earlier.
    Replace,
}

impl ArrayStrategy {
    /// Parse a strategy name as written in the registry or `--merge`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "concat-dedupe" => Some(ArrayStrategy::ConcatDedupe),
            "replace" => Some(ArrayStrategy::Replace),
            _ => None,
        }
    }
}

/// Per path array strategies, resolved by most specific matching pattern.
#[derive(Clone, Debug, Default)]
pub struct StrategyTable(Vec<(Pattern, ArrayStrategy)>);

impl StrategyTable {
    /// Build from `(pointer, strategy name)` pairs, in any order.
    ///
    /// Duplicate identical patterns are allowed. The last one given wins, so a
    /// caller may layer `--merge` flags over registry entries.
    pub fn from_pairs<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Result<Self> {
        let mut entries = Vec::new();
        for (path, name) in pairs {
            let pattern = Pattern::parse(&path)?;
            let strategy = ArrayStrategy::from_name(&name)
                .ok_or_else(|| Error::BadStrategy(path.clone(), name))?;
            entries.push((pattern, strategy));
        }
        Ok(StrategyTable(entries))
    }

    /// The strategy for `p`: the most specific matching pattern, else the
    /// default [`ArrayStrategy::ConcatDedupe`].
    #[must_use]
    pub fn lookup(&self, p: &Pointer) -> ArrayStrategy {
        self.0
            .iter()
            .filter(|(pattern, _)| pattern.matches(p))
            // max_by keeps the last of equal elements. Equal specificity means
            // an identical pattern, so this is the documented last wins rule
            // for duplicates and not an insertion order dependence otherwise.
            .max_by(|(a, _), (b, _)| a.specificity_cmp(b))
            .map(|(_, strategy)| *strategy)
            .unwrap_or_default()
    }

    /// Whether the table holds no patterns.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> Pointer {
        Pointer::parse(s).expect("valid pointer")
    }

    // ---- parsing and escaping ------------------------------------------

    #[test]
    fn empty_is_root() {
        let ptr = p("");
        assert!(ptr.is_root());
        assert_eq!(ptr.segments().len(), 0);
        assert_eq!(ptr, Pointer::root());
        assert_eq!(ptr.as_str(), "");
    }

    #[test]
    fn slash_is_one_empty_segment() {
        let ptr = p("/");
        assert!(!ptr.is_root());
        assert_eq!(ptr.segments(), &[String::new()]);
        assert_eq!(ptr.as_str(), "/");
    }

    #[test]
    fn missing_leading_slash_is_bad_pointer() {
        let err = Pointer::parse("a/b").unwrap_err();
        assert!(matches!(err, Error::BadPointer(ref s) if s == "a/b"));
    }

    #[test]
    fn escapes_decode() {
        assert_eq!(p("/a~1b").segments(), &["a/b".to_string()]);
        assert_eq!(p("/a~0b").segments(), &["a~b".to_string()]);
        // ~01 is "~1" the literal text, not a second level escape.
        assert_eq!(p("/a~01b").segments(), &["a~1b".to_string()]);
        assert_eq!(p("/m~0n~1o").segments(), &["m~n/o".to_string()]);
    }

    #[test]
    fn dangling_tilde_is_bad_pointer() {
        assert!(matches!(Pointer::parse("/a~"), Err(Error::BadPointer(_))));
        assert!(matches!(Pointer::parse("/a~2b"), Err(Error::BadPointer(_))));
    }

    #[test]
    fn as_str_reescapes() {
        assert_eq!(p("/a~1b").as_str(), "/a~1b");
        assert_eq!(p("/a~0b").as_str(), "/a~0b");
        assert_eq!(Pointer::root().child("a/b").as_str(), "/a~1b");
        assert_eq!(Pointer::root().child("~").as_str(), "/~0");
    }

    #[test]
    fn round_trips() {
        for s in [
            "",
            "/",
            "//",
            "/a",
            "/a/b/c",
            "/a~1b",
            "/a~0b",
            "/a~01b",
            "/permissions/allow/0",
            "/ /x",
            "/a//b",
        ] {
            let ptr = p(s);
            assert_eq!(ptr.as_str(), s, "as_str changed {s:?}");
            assert_eq!(
                Pointer::parse(&ptr.as_str()).unwrap(),
                ptr,
                "round trip {s:?}"
            );
        }
    }

    // ---- navigation ----------------------------------------------------

    #[test]
    fn child_and_parent() {
        let ptr = Pointer::root().child("a").child("b");
        assert_eq!(ptr.as_str(), "/a/b");
        assert_eq!(ptr.parent().unwrap().as_str(), "/a");
        assert_eq!(ptr.parent().unwrap().parent().unwrap(), Pointer::root());
        assert_eq!(Pointer::root().parent(), None);
    }

    #[test]
    fn ancestors_are_longest_first_and_inclusive() {
        let got: Vec<String> = p("/a/b/c").ancestors().map(|a| a.as_str()).collect();
        assert_eq!(got, vec!["/a/b/c", "/a/b", "/a", ""]);

        let root: Vec<String> = Pointer::root().ancestors().map(|a| a.as_str()).collect();
        assert_eq!(root, vec![""]);
    }

    // ---- resolve -------------------------------------------------------

    #[test]
    fn resolve_objects() {
        let v = json!({"a": {"b": {"c": 1}}, "": {"x": 2}});
        assert_eq!(p("").resolve(&v), Some(&v));
        assert_eq!(p("/a/b/c").resolve(&v), Some(&json!(1)));
        assert_eq!(p("//x").resolve(&v), Some(&json!(2)));
        assert_eq!(p("/a/nope").resolve(&v), None);
        assert_eq!(p("/nope/b").resolve(&v), None);
    }

    #[test]
    fn resolve_through_wrong_type_is_none() {
        let v = json!({"a": 1});
        // A scalar has no children.
        assert_eq!(p("/a/b").resolve(&v), None);
    }

    #[test]
    fn resolve_array_indices() {
        let v = json!({"tags": ["x", "y", "z"]});
        assert_eq!(p("/tags/0").resolve(&v), Some(&json!("x")));
        assert_eq!(p("/tags/2").resolve(&v), Some(&json!("z")));
        assert_eq!(p("/tags/3").resolve(&v), None, "out of range");
        assert_eq!(p("/tags/nope").resolve(&v), None, "non-numeric");
        assert_eq!(p("/tags/-").resolve(&v), None, "end-of-array marker");
        assert_eq!(p("/tags/-1").resolve(&v), None, "negative");
        assert_eq!(p("/tags/ 1").resolve(&v), None, "leading space");
        assert_eq!(p("/tags/+1").resolve(&v), None, "leading plus");
    }

    #[test]
    fn resolve_mut_edits_in_place() {
        let mut v = json!({"a": {"b": 1}});
        *p("/a/b").resolve_mut(&mut v).unwrap() = json!(2);
        assert_eq!(v, json!({"a": {"b": 2}}));
        assert!(p("/a/zz").resolve_mut(&mut v).is_none());
    }

    // ---- ensure_object_path --------------------------------------------

    #[test]
    fn ensure_object_path_creates_intermediates() {
        let mut v = json!({});
        *p("/a/b/c").ensure_object_path(&mut v).unwrap() = json!(1);
        assert_eq!(v, json!({"a": {"b": {"c": 1}}}));
    }

    #[test]
    fn ensure_object_path_keeps_existing() {
        let mut v = json!({"a": {"keep": true}});
        *p("/a/b").ensure_object_path(&mut v).unwrap() = json!(1);
        assert_eq!(v, json!({"a": {"keep": true, "b": 1}}));
    }

    #[test]
    fn ensure_object_path_root_is_identity() {
        let mut v = json!({"a": 1});
        let got = Pointer::root().ensure_object_path(&mut v).unwrap();
        assert_eq!(*got, json!({"a": 1}));
    }

    #[test]
    fn ensure_object_path_conflicts_on_non_object() {
        let mut v = json!({"a": 5});
        let err = p("/a/b/c").ensure_object_path(&mut v).unwrap_err();
        match err {
            Error::RouteTypeConflict(at) => assert_eq!(at, "/a"),
            other => panic!("expected RouteTypeConflict, got {other:?}"),
        }
        // The failed call must not have mutated anything.
        assert_eq!(v, json!({"a": 5}));

        let mut arr = json!({"a": [1, 2]});
        assert!(matches!(
            p("/a/b").ensure_object_path(&mut arr),
            Err(Error::RouteTypeConflict(_))
        ));
    }

    // ---- remove_from ---------------------------------------------------

    /// Regression test for the `preserve_order` trap: `Map::remove` is
    /// `swap_remove` under that feature and would yield `["a", "d", "c"]`.
    #[test]
    fn remove_from_preserves_sibling_order() {
        let mut v = json!({"a": 1, "b": 2, "c": 3, "d": 4});
        assert_eq!(p("/b").remove_from(&mut v), Some(json!(2)));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["a", "c", "d"]);
        // Serialisation order is what actually decides whether a fragment is
        // rewritten, so assert it directly too.
        assert_eq!(v.to_string(), r#"{"a":1,"c":3,"d":4}"#);
    }

    #[test]
    fn remove_from_does_not_prune_empty_ancestors() {
        let mut v = json!({"a": {"b": {"c": 1}}});
        assert_eq!(p("/a/b/c").remove_from(&mut v), Some(json!(1)));
        assert_eq!(v, json!({"a": {"b": {}}}));
    }

    #[test]
    fn remove_from_array_shifts() {
        let mut v = json!({"tags": ["x", "y", "z"]});
        assert_eq!(p("/tags/1").remove_from(&mut v), Some(json!("y")));
        assert_eq!(v, json!({"tags": ["x", "z"]}));
        assert_eq!(p("/tags/9").remove_from(&mut v), None);
        assert_eq!(p("/tags/nope").remove_from(&mut v), None);
        assert_eq!(v, json!({"tags": ["x", "z"]}));
    }

    #[test]
    fn remove_from_root_and_missing() {
        let mut v = json!({"a": 1});
        assert_eq!(Pointer::root().remove_from(&mut v), None);
        assert_eq!(p("/zz").remove_from(&mut v), None);
        assert_eq!(p("/a/b").remove_from(&mut v), None, "parent is a scalar");
        assert_eq!(v, json!({"a": 1}));
    }

    // ---- patterns ------------------------------------------------------

    #[test]
    fn wildcard_match_table() {
        let cases: &[(&str, &str, bool)] = &[
            ("/a/b", "/a/b", true),
            ("/a/b", "/a/c", false),
            ("/a/*", "/a/b", true),
            ("/a/*", "/a/b/c", false),
            ("/*/b", "/a/b", true),
            ("/*/b", "/a/c", false),
            ("/*/*", "/a/b", true),
            ("/a/b", "/a", false),
            ("/a", "/a/b", false),
            ("", "", true),
            ("", "/a", false),
            ("/a/*/c", "/a/anything/c", true),
            ("/servers/*/args", "/servers/foo/args", true),
        ];
        for (pat, ptr, want) in cases {
            let pattern = Pattern::parse(pat).unwrap();
            assert_eq!(pattern.matches(&p(ptr)), *want, "{pat} vs {ptr}");
        }
    }

    #[test]
    fn wildcard_only_as_whole_segment() {
        let pattern = Pattern::parse("/a*b").unwrap();
        assert_eq!(pattern.segments(), &[Seg::Lit("a*b".to_string())]);
        assert!(pattern.matches(&p("/a*b")));
        assert!(!pattern.matches(&p("/axb")));
    }

    #[test]
    fn pattern_rejects_bad_syntax() {
        assert!(matches!(Pattern::parse("a/b"), Err(Error::BadPointer(_))));
        assert!(matches!(Pattern::parse("/a~9"), Err(Error::BadPointer(_))));
    }

    // ---- strategy table ------------------------------------------------

    fn table(pairs: &[(&str, &str)]) -> StrategyTable {
        StrategyTable::from_pairs(
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>(),
        )
        .expect("valid table")
    }

    #[test]
    fn lookup_defaults_to_concat_dedupe() {
        let t = StrategyTable::default();
        assert!(t.is_empty());
        assert_eq!(t.lookup(&p("/anything")), ArrayStrategy::ConcatDedupe);

        let t = table(&[("/x", "replace")]);
        assert_eq!(t.lookup(&p("/y")), ArrayStrategy::ConcatDedupe);
        assert_eq!(t.lookup(&p("/x")), ArrayStrategy::Replace);
    }

    #[test]
    fn bad_strategy_name_errors() {
        let err =
            StrategyTable::from_pairs([("/a/b".to_string(), "sinter".to_string())]).unwrap_err();
        match err {
            Error::BadStrategy(pointer, name) => {
                assert_eq!(pointer, "/a/b");
                assert_eq!(name, "sinter");
                // The message must name both, per the error module invariant.
                let msg = Error::BadStrategy(pointer, name).to_string();
                assert!(msg.contains("/a/b"), "{msg}");
                assert!(msg.contains("sinter"), "{msg}");
            }
            other => panic!("expected BadStrategy, got {other:?}"),
        }
    }

    #[test]
    fn bad_pointer_in_table_errors() {
        let err =
            StrategyTable::from_pairs([("a/b".to_string(), "replace".to_string())]).unwrap_err();
        assert!(matches!(err, Error::BadPointer(_)));
    }

    #[test]
    fn exact_beats_wildcard() {
        for order in [
            [("/a/b", "replace"), ("/a/*", "concat-dedupe")],
            [("/a/*", "concat-dedupe"), ("/a/b", "replace")],
        ] {
            let t = table(&order);
            assert_eq!(t.lookup(&p("/a/b")), ArrayStrategy::Replace);
            assert_eq!(t.lookup(&p("/a/c")), ArrayStrategy::ConcatDedupe);
        }
    }

    #[test]
    fn leftmost_literal_breaks_the_tie() {
        // Both match /a/b/c with one wildcard each. The first differing
        // segment is index 1, where /a/b/* has the literal.
        for order in [
            [("/a/*/c", "concat-dedupe"), ("/a/b/*", "replace")],
            [("/a/b/*", "replace"), ("/a/*/c", "concat-dedupe")],
        ] {
            let t = table(&order);
            assert_eq!(
                t.lookup(&p("/a/b/c")),
                ArrayStrategy::Replace,
                "leftmost literal must win regardless of insertion order"
            );
        }
    }

    #[test]
    fn more_literals_beats_leftmost() {
        // /*/b/c has two literals, /a/*/* has one, so the wildcard first
        // pattern wins despite losing at segment 0.
        for order in [
            [("/*/b/c", "replace"), ("/a/*/*", "concat-dedupe")],
            [("/a/*/*", "concat-dedupe"), ("/*/b/c", "replace")],
        ] {
            let t = table(&order);
            assert_eq!(t.lookup(&p("/a/b/c")), ArrayStrategy::Replace);
        }
    }

    #[test]
    fn specificity_is_order_independent_across_three() {
        let entries = [
            ("/a/*/*", "concat-dedupe"),
            ("/a/*/c", "concat-dedupe"),
            ("/a/b/c", "replace"),
        ];
        // Every permutation must agree.
        let perms = [
            [0, 1, 2],
            [0, 2, 1],
            [1, 0, 2],
            [1, 2, 0],
            [2, 0, 1],
            [2, 1, 0],
        ];
        for perm in perms {
            let ordered: Vec<(&str, &str)> = perm.iter().map(|&i| entries[i]).collect();
            let t = table(&ordered);
            assert_eq!(t.lookup(&p("/a/b/c")), ArrayStrategy::Replace, "{perm:?}");
        }
    }

    #[test]
    fn duplicate_identical_patterns_last_wins() {
        let t = table(&[("/a/b", "concat-dedupe"), ("/a/b", "replace")]);
        assert_eq!(t.lookup(&p("/a/b")), ArrayStrategy::Replace);
        let t = table(&[("/a/b", "replace"), ("/a/b", "concat-dedupe")]);
        assert_eq!(t.lookup(&p("/a/b")), ArrayStrategy::ConcatDedupe);
    }

    #[test]
    fn escaped_pattern_segments_match_decoded_pointers() {
        let t = table(&[("/a~1b/list", "replace")]);
        assert_eq!(t.lookup(&p("/a~1b/list")), ArrayStrategy::Replace);
        assert_eq!(
            t.lookup(&Pointer::root().child("a/b").child("list")),
            ArrayStrategy::Replace
        );
    }
}
