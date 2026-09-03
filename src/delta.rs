//! Classifying the difference between the combined `base` and the on disk
//! `output` into routable [`Change`]s.
//!
//! [`json_patch::diff`] is used here as a **change detector and nothing else**.
//! Its RFC 6902 ops are never applied: not individually, not in sequence. That
//! is deliberate: RFC 6902 array indices are positions in a document that is
//! being mutated as the patch is applied, so `[{"op":"remove","path":"/tags/1"},
//! {"op":"remove","path":"/tags/2"}]` names two elements whose indices only make
//! sense relative to each other. Reading values back out of a partially patched
//! document is the classic way to route the wrong array element.
//!
//! Instead, every op is reduced to a *path*, and every value is read from the
//! original, unmodified `base` and `output`. For paths inside a `concat-dedupe`
//! array the index is discarded entirely: the array is marked dirty, and its
//! membership delta is computed once, by set difference, from the two original
//! documents. Position never enters into it, which is exactly what a drop in
//! list means.
//!
//! # Why an empty membership delta emits nothing
//!
//! `combine` re-emits `concat-dedupe` array elements in *fragment* order, while
//! the external output file keeps whatever order the foreign program wrote. Raw
//! `json_patch::diff` therefore reports a reordered but identical array as
//! different forever. Dropping array changes whose `added` and `removed` are
//! both empty is what makes `in_sync = changes.is_empty()` true for a properly
//! synchronised set: without it, `silktouch diff` would exit 1 permanently and
//! no pre commit hook using it could ever pass. It is also what makes law
//! C (idempotence) hold.
//!
//! # `jsonptr` stays inside this module
//!
//! `json-patch` speaks [`jsonptr::PointerBuf`]. The rest of the crate speaks
//! [`crate::pointer::Pointer`]. The conversion happens in `from_jsonptr` and
//! `jsonptr` appears in no exported type.

use std::collections::HashSet;

use json_patch::jsonptr;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::pointer::{ArrayStrategy, Pointer, StrategyTable};

/// One routable difference between `base` and `output`.
///
/// Each variant names the path it acts on, and no two changes in a
/// [`classify`] result share a path.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Path present in both with differing values: overwrite at the owner.
    Set { path: Pointer, value: Value },
    /// Path present in `output` only: insert into the catch all.
    Insert { path: Pointer, value: Value },
    /// Path present in `base` only: delete from every fragment declaring it.
    Unset { path: Pointer },
    /// A `concat-dedupe` array, routed by membership rather than by position:
    /// `removed` elements are dropped from every fragment contributing them,
    /// `added` elements are appended to the catch all's array at `path`.
    ///
    /// Never constructed with both lists empty.
    ArrayMembers {
        path: Pointer,
        added: Vec<Value>,
        removed: Vec<Value>,
    },
}

impl Change {
    /// The path this change acts on.
    #[must_use]
    pub fn path(&self) -> &Pointer {
        match self {
            Change::Set { path, .. }
            | Change::Insert { path, .. }
            | Change::Unset { path }
            | Change::ArrayMembers { path, .. } => path,
        }
    }
}

/// Classify the difference from `base` to `output` into changes, deduped by
/// path, in first seen order.
///
/// `base` is the document [`crate::merge::combine_values`] produces from the
/// current fragments. `output` is what is actually on disk. The result is what
/// `route` replays back into the fragments.
///
/// Guarantees the router relies on:
///
/// - **No change ever carries an array index in its path.** Any op inside an
///   array collapses to the *outermost* enclosing array's path: either an
///   [`Change::ArrayMembers`] (`concat-dedupe`) or a whole array
///   [`Change::Set`]/[`Change::Unset`] (`replace`, or the value changed type).
///   Outermost, because an inner array nested inside another array is reached
///   only through an index. See `outermost_array_ancestor`. This matters
///   because no fragment declares an indexed path, so an indexed `Set` would
///   have no owner to route to.
/// - **Paths are unique**, in first seen order.
/// - **`classify(x, x, _)` is empty**, and so is a `concat-dedupe` array that
///   was merely reordered.
///
/// Errors only on `move`/`copy`/`test` ops, which `json_patch::diff` does not
/// currently emit. The arms exist so a future change in that crate becomes a
/// loud failure rather than a silently misrouted change.
pub fn classify(base: &Value, output: &Value, strategies: &StrategyTable) -> Result<Vec<Change>> {
    let ops = json_patch::diff(base, output);

    let mut changes: Vec<Change> = Vec::new();
    let mut seen: HashSet<Pointer> = HashSet::new();

    for op in &ops.0 {
        let path = from_jsonptr(op.path());

        // Step 3/4: an op anywhere inside an array is really a statement about
        // that array's membership, so hoist it to the outermost array ancestor,
        // the only ancestor guaranteed not to be indexed itself.
        if let Some(arr) = outermost_array_ancestor(&path, base) {
            let out_val = arr.resolve(output);
            let concat = strategies.lookup(&arr) == ArrayStrategy::ConcatDedupe;
            match out_val {
                // Still an array, merged by membership: defer to the second
                // pass, which reads both originals exactly once.
                Some(Value::Array(_)) if concat => {
                    push_unique(
                        &mut changes,
                        &mut seen,
                        Change::ArrayMembers {
                            path: arr,
                            added: Vec::new(),
                            removed: Vec::new(),
                        },
                    );
                }
                // `replace` strategy, or the value stopped being an array:
                // the whole array is overwritten at the owner.
                Some(v) => push_unique(
                    &mut changes,
                    &mut seen,
                    Change::Set {
                        path: arr,
                        value: v.clone(),
                    },
                ),
                None => push_unique(&mut changes, &mut seen, Change::Unset { path: arr }),
            }
            continue;
        }

        // Step 5: no array involved, so the op maps straight across. Values
        // come from `output`, never from the op's own `value` field, so `Add`
        // and `Replace` are handled identically and always reflect the real
        // output document.
        let change = match op {
            json_patch::PatchOperation::Add(_) => Change::Insert {
                path: path.clone(),
                value: resolved(&path, output)?,
            },
            json_patch::PatchOperation::Replace(_) => Change::Set {
                path: path.clone(),
                value: resolved(&path, output)?,
            },
            json_patch::PatchOperation::Remove(_) => Change::Unset { path },
            json_patch::PatchOperation::Move(_) => return Err(Error::UnsupportedOp("move")),
            json_patch::PatchOperation::Copy(_) => return Err(Error::UnsupportedOp("copy")),
            json_patch::PatchOperation::Test(_) => return Err(Error::UnsupportedOp("test")),
        };
        push_unique(&mut changes, &mut seen, change);
    }

    // Step 6: fill in every dirty array's membership delta from the untouched
    // originals, and drop the ones that turn out to be pure reorderings.
    let mut result = Vec::with_capacity(changes.len());
    for change in changes {
        match change {
            Change::ArrayMembers { path, .. } => {
                let (added, removed) = membership_delta(&path, base, output);
                if !(added.is_empty() && removed.is_empty()) {
                    result.push(Change::ArrayMembers {
                        path,
                        added,
                        removed,
                    });
                }
            }
            other => result.push(other),
        }
    }

    // The router's precondition, checked where it is established rather than
    // where it is relied on. Debug only: it is a restatement of what
    // `outermost_array_ancestor` proves, not a runtime policy.
    #[cfg(debug_assertions)]
    for change in &result {
        debug_assert!(
            !steps_into_array(change.path(), base),
            "classify emitted an indexed path {}: {change:?}",
            change.path().as_str()
        );
    }

    Ok(result)
}

/// Append `change` unless a change for its path was already recorded.
fn push_unique(changes: &mut Vec<Change>, seen: &mut HashSet<Pointer>, change: Change) {
    if seen.insert(change.path().clone()) {
        changes.push(change);
    }
}

/// Convert a `json-patch` pointer into ours. `Token::decoded` already undoes
/// the `~0`/`~1` escapes, and [`Pointer`] stores decoded segments, so nothing
/// is escaped a second time in between.
fn from_jsonptr(p: &jsonptr::Pointer) -> Pointer {
    p.tokens()
        .fold(Pointer::root(), |acc, tok| acc.child(&tok.decoded()))
}

/// The **outermost** ancestor of `ptr` (itself included) that is an array in
/// `base`, the shallowest one, not the innermost.
///
/// [`Pointer::ancestors`] is longest first and inclusive, so the outermost hit
/// is the *last* match: for `/servers/0/args/2`, where `/servers` is itself an
/// array, this is `/servers`, not `/servers/0/args`.
///
/// # Why the outermost, and not the innermost
///
/// Two reasons, and they agree.
///
/// *It is the only index free answer.* The router's standing guarantee is that
/// no [`Change`] path carries an array index, because no fragment declares one.
/// If the chosen array's own path contained an index segment, some ancestor of
/// it would itself have to be an array, contradicting it being the outermost.
/// Picking the innermost has no such property: for `/servers/0/args/2` it
/// yields `/servers/0/args`, and for `base = {"b": [[]]}` it yields `/b/0`,
/// both of which name a position inside a `concat-dedupe` array that no
/// fragment can own.
///
/// *It is the right semantics.* A `concat-dedupe` array's elements arrive
/// whole, each from whichever fragment contributed it. Position is not
/// meaningful and no fragment owns a slot. So the only edit that can be routed
/// is exchanging an element. Editing "inside" an element, at any depth, is
/// therefore removing the old element and adding the new one, which is a
/// statement about the *outermost* array's membership.
fn outermost_array_ancestor(ptr: &Pointer, base: &Value) -> Option<Pointer> {
    ptr.ancestors()
        .filter(|a| matches!(a.resolve(base), Some(Value::Array(_))))
        .last()
}

/// Whether `path` steps *into* an array in `base`, i.e. some segment of it is
/// consumed as an array index.
///
/// Ending *at* an array is fine (`/tags`). Passing *through* one is the
/// violation (`/tags/0`, `/servers/0/args`). A path that does not resolve in
/// `base` at all (a fresh [`Change::Insert`]) steps into nothing, so it is
/// clean by construction.
#[cfg(debug_assertions)]
fn steps_into_array(path: &Pointer, base: &Value) -> bool {
    let mut cur = base;
    for seg in path.segments() {
        cur = match cur {
            Value::Array(_) => return true,
            Value::Object(map) => match map.get(seg) {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
    }
    false
}

/// The value at `path` in `output`, which the op asserts is there.
fn resolved(path: &Pointer, output: &Value) -> Result<Value> {
    path.resolve(output)
        .cloned()
        .ok_or_else(|| Error::BadPointer(path.as_str()))
}

/// The array at `path` in `v`, or an empty slice if it is missing or not an
/// array.
fn array_at<'a>(path: &Pointer, v: &'a Value) -> &'a [Value] {
    match path.resolve(v) {
        Some(Value::Array(items)) => items.as_slice(),
        _ => &[],
    }
}

/// `(added, removed)` for the arrays at `path`, by full [`Value`] equality.
///
/// Order insensitive on both sides by construction, so a reordered array
/// yields two empty lists. `classify` only calls this where both sides are
/// arrays.
fn membership_delta(path: &Pointer, base: &Value, output: &Value) -> (Vec<Value>, Vec<Value>) {
    let before = array_at(path, base);
    let after = array_at(path, output);

    let added = after
        .iter()
        .filter(|v| !before.contains(v))
        .cloned()
        .collect();
    let removed = before
        .iter()
        .filter(|v| !after.contains(v))
        .cloned()
        .collect();
    (added, removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn p(s: &str) -> Pointer {
        Pointer::parse(s).expect("valid pointer")
    }

    fn plain(base: &Value, output: &Value) -> Vec<Change> {
        classify(base, output, &StrategyTable::default()).expect("classify")
    }

    fn table(pairs: &[(&str, &str)]) -> StrategyTable {
        StrategyTable::from_pairs(
            pairs
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect::<Vec<_>>(),
        )
        .expect("valid table")
    }

    /// The one `ArrayMembers` in `changes`, or a panic naming what was found.
    fn only_array_members(changes: &[Change]) -> (&Pointer, &[Value], &[Value]) {
        match changes {
            [
                Change::ArrayMembers {
                    path,
                    added,
                    removed,
                },
            ] => (path, added, removed),
            other => panic!("expected exactly one ArrayMembers, got {other:?}"),
        }
    }

    /// No change may carry a path whose last segment is an array index. No
    /// fragment declares one, so such a change could never be routed.
    fn assert_no_indexed_paths(changes: &[Change]) {
        for change in changes {
            for seg in change.path().segments() {
                assert!(
                    seg.parse::<usize>().is_err(),
                    "indexed path {} in {change:?}",
                    change.path().as_str()
                );
            }
        }
    }

    // ---- the index shift trap ------------------------------------------

    /// The purest form of the trap. `diff` turns `["a","b","c"] -> ["a"]` into
    /// **two identical ops**, `remove /tags/1` twice, because the second one
    /// speaks about the array the first one already shortened. Resolving each
    /// op's path against `base` therefore yields "b" twice and never mentions
    /// "c". Applying them and reading back yields "b" then "c" but only if the
    /// order is right. Membership set difference sidesteps the question: it
    /// never looks at an index at all.
    #[test]
    fn remove_at_index_reports_the_base_element_not_the_shifted_one() {
        let base = json!({"tags": ["a", "b", "c"]});
        let output = json!({"tags": ["a"]});

        // Pin the shape this test is about: /tags/1 twice, not /tags/1 + /tags/2.
        let paths: Vec<String> = json_patch::diff(&base, &output)
            .0
            .iter()
            .map(|op| op.path().as_str().to_string())
            .collect();
        assert_eq!(paths, vec!["/tags/1", "/tags/1"]);

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/tags"));
        // "b" is what index 1 named in `base`. "c" is the element the second op
        // meant, which naming by index against `base` would have missed.
        assert_eq!(removed, [json!("b"), json!("c")]);
        assert_ne!(removed, [json!("b"), json!("b")], "resolved against base");
        assert!(added.is_empty());
        assert_no_indexed_paths(&changes);
    }

    /// The other half: an index in a `remove` op can name an element that is
    /// still present in `output` and must not be routed as a removal. Here
    /// `diff` emits `replace /tags/1 = "c"` then `remove /tags/2`, and
    /// `base` at `/tags/2` *is* "c", the one element of the three that
    /// survives untouched.
    #[test]
    fn a_remove_ops_index_does_not_name_the_element_that_went_away() {
        let base = json!({"tags": ["a", "b", "c"]});
        let output = json!({"tags": ["a", "c"]});

        let ops = json_patch::diff(&base, &output);
        let paths: Vec<String> = ops
            .0
            .iter()
            .map(|op| op.path().as_str().to_string())
            .collect();
        assert_eq!(paths, vec!["/tags/1", "/tags/2"]);
        assert_eq!(p("/tags/2").resolve(&base), Some(&json!("c")));

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/tags"));
        assert_eq!(removed, [json!("b")]);
        assert!(
            !removed.contains(&json!("c")),
            "\"c\" is still in output and must never be routed as removed"
        );
        assert!(added.is_empty());
        assert_no_indexed_paths(&changes);
    }

    // ---- a replace at an index is really one add and one remove ---------

    #[test]
    fn replace_at_index_decomposes_into_added_and_removed() {
        let base = json!({"tags": ["a", "b", "c"]});
        let output = json!({"tags": ["a", "x", "c"]});

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/tags"));
        assert_eq!(added, [json!("x")]);
        assert_eq!(removed, [json!("b")]);

        // The load bearing half: nothing anywhere in the result is a Set at an
        // indexed path.
        assert_no_indexed_paths(&changes);
        assert!(
            !changes.iter().any(|c| matches!(c, Change::Set { .. })),
            "{changes:?}"
        );
    }

    // ---- outermost ancestor --------------------------------------------

    /// An array inside an object inside an array. The deepest array ancestor
    /// of `/servers/0/args/2` is `/servers/0/args`, whose own path contains
    /// the index `0`, unroutable, since no fragment declares one. Hoisting to
    /// `/servers` instead is both index free and the right semantics: a
    /// `concat-dedupe` element arrives whole from one fragment, so editing
    /// inside it exchanges the whole element.
    #[test]
    fn an_array_nested_inside_an_array_element_hoists_to_the_outer_array() {
        let base = json!({"servers": [{"cmd": "x", "args": ["--a", "--b", "--c"]}]});
        let output = json!({"servers": [{"cmd": "x", "args": ["--a", "--b", "--z"]}]});

        let ops = json_patch::diff(&base, &output);
        assert_eq!(ops.0[0].path().as_str(), "/servers/0/args/2", "{ops:?}");

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        // /servers, not /servers/0/args: the latter carries an index.
        assert_eq!(path, &p("/servers"));
        assert_eq!(added, [json!({"cmd": "x", "args": ["--a", "--b", "--z"]})]);
        assert_eq!(
            removed,
            [json!({"cmd": "x", "args": ["--a", "--b", "--c"]})]
        );
        assert_no_indexed_paths(&changes);
    }

    /// The shape the property tests found: an array *directly* inside an
    /// array. `/b/0` resolves to an array in `base`, so the innermost rule
    /// used to emit `Set { path: "/b/0" }` beside the `/b` membership change,
    /// and `route` then misfiled it or failed with `RouteTypeConflict`.
    #[test]
    fn an_array_directly_inside_an_array_hoists_to_the_outer_array() {
        let base = json!({"b": [[]]});
        let output = json!({"b": [null, []]});

        assert!(
            matches!(p("/b/0").resolve(&base), Some(Value::Array(_))),
            "the inner element really is an array, which is what used to trap"
        );

        let changes = plain(&base, &output);
        assert_no_indexed_paths(&changes);
        assert!(
            !changes.iter().any(|c| matches!(c, Change::Set { .. })),
            "no Set at an element position: {changes:?}"
        );
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/b"));
        assert_eq!(added, [json!(null)]);
        assert!(removed.is_empty(), "{removed:?}");
    }

    // ---- type changes ---------------------------------------------------

    #[test]
    fn array_to_scalar_under_concat_dedupe_is_a_set() {
        let base = json!({"tags": ["a", "b"]});
        let output = json!({"tags": "none"});

        let changes = plain(&base, &output);
        assert_eq!(
            changes,
            vec![Change::Set {
                path: p("/tags"),
                value: json!("none"),
            }]
        );
    }

    #[test]
    fn array_to_object_under_concat_dedupe_is_a_set() {
        let base = json!({"tags": ["a"]});
        let output = json!({"tags": {"k": 1}});
        assert_eq!(
            plain(&base, &output),
            vec![Change::Set {
                path: p("/tags"),
                value: json!({"k": 1}),
            }]
        );
    }

    #[test]
    fn array_deleted_outright_is_an_unset_of_the_array() {
        let base = json!({"tags": ["a", "b"]});
        let output = json!({});
        assert_eq!(
            plain(&base, &output),
            vec![Change::Unset { path: p("/tags") }]
        );
    }

    // ---- replace strategy ----------------------------------------------

    #[test]
    fn replace_strategy_array_is_set_wholesale() {
        let base = json!({"tags": ["a", "b"]});
        let output = json!({"tags": ["b", "c"]});
        let changes = classify(&base, &output, &table(&[("/tags", "replace")])).unwrap();
        assert_eq!(
            changes,
            vec![Change::Set {
                path: p("/tags"),
                value: json!(["b", "c"]),
            }]
        );
    }

    #[test]
    fn replace_strategy_reorder_is_still_a_set() {
        // Under `replace` order is meaningful, so unlike concat-dedupe a pure
        // reordering is a real change.
        let base = json!({"tags": ["a", "b"]});
        let output = json!({"tags": ["b", "a"]});
        let changes = classify(&base, &output, &table(&[("/tags", "replace")])).unwrap();
        assert_eq!(
            changes,
            vec![Change::Set {
                path: p("/tags"),
                value: json!(["b", "a"]),
            }]
        );
    }

    // ---- reordering is not a change under concat-dedupe -----------------

    #[test]
    fn reordering_a_concat_dedupe_array_yields_no_changes() {
        let base = json!({"tags": ["a", "b", "c"]});
        let output = json!({"tags": ["c", "b", "a"]});

        // `diff` definitely reports differences here. classify must not.
        assert!(!json_patch::diff(&base, &output).0.is_empty());
        assert_eq!(plain(&base, &output), vec![]);
    }

    #[test]
    fn reordering_nested_arrays_yields_no_changes() {
        let base = json!({"a": {"b": ["x", "y"]}, "c": [1, 2, 3]});
        let output = json!({"a": {"b": ["y", "x"]}, "c": [3, 1, 2]});
        assert_eq!(plain(&base, &output), vec![]);
    }

    // ---- collapsing ------------------------------------------------------

    #[test]
    fn many_ops_on_one_array_collapse_to_a_single_change() {
        let base = json!({"tags": ["a", "b", "c", "d"]});
        let output = json!({"tags": ["a", "c", "e", "f"]});

        assert!(
            json_patch::diff(&base, &output).0.len() > 1,
            "test needs several ops to be meaningful"
        );

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/tags"));
        assert_eq!(added, [json!("e"), json!("f")]);
        assert_eq!(removed, [json!("b"), json!("d")]);
    }

    // ---- plain object cases ---------------------------------------------

    #[test]
    fn new_key_is_an_insert() {
        assert_eq!(
            plain(&json!({"a": 1}), &json!({"a": 1, "b": 2})),
            vec![Change::Insert {
                path: p("/b"),
                value: json!(2),
            }]
        );
    }

    #[test]
    fn deleted_key_is_an_unset() {
        assert_eq!(
            plain(&json!({"a": 1, "b": 2}), &json!({"a": 1})),
            vec![Change::Unset { path: p("/b") }]
        );
    }

    #[test]
    fn changed_scalar_is_a_set() {
        assert_eq!(
            plain(&json!({"a": 1}), &json!({"a": 2})),
            vec![Change::Set {
                path: p("/a"),
                value: json!(2),
            }]
        );
    }

    #[test]
    fn insert_unset_and_set_together() {
        let base = json!({"a": 1, "b": 2, "n": {"deep": true}});
        let output = json!({"a": 9, "n": {"deep": true}, "c": 3});
        let changes = plain(&base, &output);
        assert_eq!(changes.len(), 3, "{changes:?}");
        for want in [
            Change::Set {
                path: p("/a"),
                value: json!(9),
            },
            Change::Unset { path: p("/b") },
            Change::Insert {
                path: p("/c"),
                value: json!(3),
            },
        ] {
            assert!(changes.contains(&want), "missing {want:?} in {changes:?}");
        }
    }

    #[test]
    fn object_to_scalar_is_a_set_at_the_object() {
        assert_eq!(
            plain(&json!({"a": {"b": 1}}), &json!({"a": 5})),
            vec![Change::Set {
                path: p("/a"),
                value: json!(5),
            }]
        );
    }

    // ---- new nested paths ------------------------------------------------

    #[test]
    fn a_wholly_new_nested_subtree_is_one_insert() {
        let base = json!({"a": {}});
        let output = json!({"a": {"b": {"c": 1}}});
        let changes = plain(&base, &output);
        assert_eq!(
            changes,
            vec![Change::Insert {
                path: p("/a/b"),
                value: json!({"c": 1}),
            }],
            "the insert lands at the highest wholly-new path"
        );
    }

    #[test]
    fn a_new_leaf_under_an_existing_object_is_an_insert() {
        let base = json!({"a": {"b": {"x": 0}}});
        let output = json!({"a": {"b": {"x": 0, "c": 1}}});
        assert_eq!(
            plain(&base, &output),
            vec![Change::Insert {
                path: p("/a/b/c"),
                value: json!(1),
            }]
        );
    }

    #[test]
    fn a_new_array_is_an_insert_of_the_whole_array() {
        // No array ancestor exists in `base`, so this is a plain insert.
        let base = json!({});
        let output = json!({"tags": ["a", "b"]});
        assert_eq!(
            plain(&base, &output),
            vec![Change::Insert {
                path: p("/tags"),
                value: json!(["a", "b"]),
            }]
        );
    }

    // ---- identity ---------------------------------------------------------

    #[test]
    fn identical_documents_yield_no_changes() {
        for v in [
            json!({}),
            json!({"a": 1}),
            json!({"a": {"b": {"c": [1, 2, 3]}}}),
            json!({"servers": [{"args": ["--x"]}], "flag": true, "n": null}),
            json!({"a~b": 1, "a/b": 2, "": 3}),
        ] {
            assert_eq!(plain(&v, &v), vec![], "not empty for {v}");
            let with_strategies = classify(&v, &v, &table(&[("/a", "replace")])).unwrap();
            assert_eq!(with_strategies, vec![], "not empty for {v}");
        }
    }

    // ---- emptied nested object -------------------------------------------

    /// `remove_from` never prunes empty ancestors, so `combine` stays stable.
    /// The delta side must agree and report only the leaf that went away.
    #[test]
    fn emptying_a_nested_object_unsets_only_the_leaf() {
        let base = json!({"a": {"b": {"c": 1}}});
        let output = json!({"a": {"b": {}}});
        let changes = plain(&base, &output);
        assert_eq!(changes, vec![Change::Unset { path: p("/a/b/c") }]);
        assert!(
            !changes.iter().any(|c| c.path() == &p("/a/b")),
            "must not remove the now-empty object itself: {changes:?}"
        );
    }

    // ---- deep equality ----------------------------------------------------

    #[test]
    fn array_membership_uses_deep_equality() {
        // The {"a":1,"b":2} element is structurally equal on both sides but has
        // moved. Only the genuinely new/gone scalars may be reported.
        let base = json!({"items": [{"a": 1, "b": 2}, "z"]});
        let output = json!({"items": ["w", {"a": 1, "b": 2}]});

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/items"));
        assert_eq!(added, [json!("w")]);
        assert_eq!(removed, [json!("z")]);
    }

    #[test]
    fn a_nested_field_change_inside_an_array_element_swaps_the_element() {
        // Not a "Set at /items/0/a": the whole element is exchanged, because
        // membership is all a concat-dedupe array has.
        let base = json!({"items": [{"a": 1}]});
        let output = json!({"items": [{"a": 2}]});

        let changes = plain(&base, &output);
        let (path, added, removed) = only_array_members(&changes);
        assert_eq!(path, &p("/items"));
        assert_eq!(added, [json!({"a": 2})]);
        assert_eq!(removed, [json!({"a": 1})]);
        assert_no_indexed_paths(&changes);
    }

    // ---- misc -------------------------------------------------------------

    #[test]
    fn escaped_segments_survive_the_jsonptr_round_trip() {
        let base = json!({"a/b": {"c~d": 1}});
        let output = json!({"a/b": {"c~d": 2}});
        let changes = plain(&base, &output);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].path().segments(), &["a/b", "c~d"]);
        assert_eq!(changes[0].path().as_str(), "/a~1b/c~0d");
    }

    #[test]
    fn paths_are_unique_and_in_first_seen_order() {
        let base = json!({"one": ["a", "b", "c"], "two": 1});
        let output = json!({"one": ["c", "d", "e"], "two": 2});
        let changes = plain(&base, &output);

        let mut paths: Vec<String> = changes.iter().map(|c| c.path().as_str()).collect();
        let len = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), len, "duplicate paths in {changes:?}");
        assert_eq!(changes.len(), 2, "{changes:?}");
        assert_eq!(changes[0].path(), &p("/one"), "first-seen order");
    }

    #[test]
    fn a_replace_strategy_array_nested_under_a_wildcard_pattern() {
        let strategies = table(&[("/servers/*/args", "replace")]);
        let base = json!({"servers": {"s": {"args": ["--a", "--b"]}}});
        let output = json!({"servers": {"s": {"args": ["--b", "--a"]}}});
        assert_eq!(
            classify(&base, &output, &strategies).unwrap(),
            vec![Change::Set {
                path: p("/servers/s/args"),
                value: json!(["--b", "--a"]),
            }]
        );
    }
}
