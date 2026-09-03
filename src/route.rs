//! Replaying classified [`Change`]s back into the fragments they came from.
//!
//! This is the second half of `split`: [`crate::delta::classify`] says *what*
//! differs between the combined `base` and the on disk `output`. This module
//! decides *which file* absorbs each difference.
//!
//! | change | destination |
//! |---|---|
//! | [`Change::Set`] | the **owner**: the last (highest precedence) fragment declaring the path |
//! | [`Change::Insert`] | the **catch all** |
//! | [`Change::Unset`] | **every** fragment declaring the path |
//! | [`Change::ArrayMembers`] | removals from every contributor, additions to the catch all |
//!
//! # Why deletions and array removals hit every fragment
//!
//! Overwrites are routed to a single file because the lower fragments were
//! already shadowed: whatever they say about that path never reached the
//! output. Removals are the opposite. `combine` folds a deletion away only if
//! *no* fragment declares the path, and it concatenates `concat-dedupe` arrays
//! across fragments, so an element left behind in even one low precedence
//! fragment is resurrected by the very next `combine`, and law B
//! (`combine(split(F, O)) == O`) fails. Removal is therefore exhaustive by
//! necessity, not by tidiness.
//!
//! # Ownership is read live, never precomputed
//!
//! [`FragmentSet::owner_of`] and [`FragmentSet::declarers_of`] answer from the
//! fragments' *current* values, which this module is mutating as it goes.
//! Changes are applied in the order [`crate::delta::classify`] returned them
//! and ownership is queried again per change, so a change landing under a path an
//! earlier [`Change::Unset`] just removed sees the new reality.
//!
//! # The catch all is only touched on demand
//!
//! [`FragmentSet::catch_all_index`] *synthesises* an in memory fragment when no
//! file by that name exists. Calling it speculatively would be a bug: a run
//! that only overwrites at owners must leave no trace, or `write_back` creates
//! an empty `99-local.json` and breaks the rule of silence. It is therefore
//! called only at the moment something is actually about to be written into
//! the catch all.
//!
//! # Empty containers are left in place
//!
//! An object emptied by a deletion stays, matching
//! [`Pointer::remove_from`](crate::pointer::Pointer::remove_from). Pruning
//! ancestors would make the next `combine` differ from this one for no reason.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::delta::Change;
use crate::error::{Error, Result};
use crate::fragment::FragmentSet;
use crate::pointer::{ArrayStrategy, Pointer, StrategyTable};

/// What a [`route`] call did.
#[derive(Debug, Default, PartialEq)]
pub struct RouteReport {
    /// Indices into `set.fragments` of the fragments actually modified,
    /// ascending and deduped. A change that would write a value already
    /// present modifies nothing and is not reported.
    pub touched: Vec<usize>,
    /// A catch all fragment existed only in memory (never on disk) **and**
    /// this call wrote into it, i.e. `write_back` is about to create the file.
    pub created_catch_all: bool,
}

/// Route every change into the fragments, mutating `set` in place.
///
/// Changes are applied in slice order. See the module docs for why that order
/// and live ownership queries matter. Nothing is written to disk. The caller
/// decides that by calling [`FragmentSet::write_back`].
///
/// `strategies` is the same table [`crate::delta::classify`] was given. It is
/// used to check classify's contract, not to make routing decisions, since a
/// classified change already encodes the strategy that produced it.
///
/// # Errors
///
/// [`Error::RouteTypeConflict`] when the catch all already holds something of
/// the wrong shape where a value has to go: something that is not an object on
/// the way down to an inserted key, or something that is not an array where
/// array members must be appended.
pub fn route(
    set: &mut FragmentSet,
    changes: &[Change],
    strategies: &StrategyTable,
) -> Result<RouteReport> {
    let mut state = Routing::default();

    for change in changes {
        match change {
            Change::Set { path, value } => set_at_owner(set, path, value, &mut state)?,
            Change::Insert { path, value } => {
                insert_into_catch_all(set, path, value, &mut state)?;
            }
            Change::Unset { path } => unset_everywhere(set, path, &mut state),
            Change::ArrayMembers {
                path,
                added,
                removed,
            } => {
                // `classify` only reduces an array to a membership delta under
                // `concat-dedupe`. A `replace` array arrives as a whole array
                // `Set`. Asserted rather than branched on, so a hand built
                // change set violating it fails loudly instead of quietly
                // appending into an array that the next `combine` will discard.
                debug_assert_eq!(
                    strategies.lookup(path),
                    ArrayStrategy::ConcatDedupe,
                    "ArrayMembers at {} but its strategy is not concat-dedupe",
                    path.as_str()
                );
                array_members(set, path, added, removed, &mut state)?;
            }
        }
    }

    Ok(state.finish(set))
}

/// Accumulated effects of the changes applied so far.
///
/// Fragments are tracked by **name**, not by index: writing to the catch all
/// can synthesise a fragment and insert it into the vector, shifting the index
/// of every fragment after it. Names are stable across that insertion (and
/// unique, being file names in one directory), so they are resolved back to
/// indices once, at the end.
#[derive(Debug, Default)]
struct Routing {
    touched: BTreeSet<String>,
}

impl Routing {
    /// Record that the named fragment's value actually changed.
    fn mark(&mut self, name: &str) {
        if !self.touched.contains(name) {
            self.touched.insert(name.to_string());
        }
    }

    /// Resolve tracked names back to indices and settle `created_catch_all`.
    fn finish(self, set: &FragmentSet) -> RouteReport {
        let mut touched: Vec<usize> = self
            .touched
            .iter()
            .filter_map(|name| set.fragments.iter().position(|f| &f.name == name))
            .collect();
        touched.sort_unstable();

        // Written into (so it is in `touched`) and never read from disk (so
        // `write_back` will create it). A catch all that was only synthesised,
        // never written to, is deliberately not reported: it is not dirty and
        // no file will appear.
        let created_catch_all = self.touched.contains(&set.catch_all)
            && set
                .fragments
                .iter()
                .any(|f| f.name == set.catch_all && f.raw.is_none());

        RouteReport {
            touched,
            created_catch_all,
        }
    }
}

/// Overwrite `path` at its owner, or file it in the catch all if it has none.
fn set_at_owner(
    set: &mut FragmentSet,
    path: &Pointer,
    value: &Value,
    state: &mut Routing,
) -> Result<()> {
    let Some(idx) = set.owner_of(path) else {
        // Defensive. `classify` only emits `Set` for a path present in `base`,
        // and every path in `base` was declared by some fragment. Treat an
        // ownerless overwrite as an insertion rather than dropping it.
        return insert_into_catch_all(set, path, value, state);
    };

    let frag = &mut set.fragments[idx];
    let slot = path
        .resolve_mut(&mut frag.value)
        .expect("the owner declares the path by construction");
    if *slot != *value {
        *slot = value.clone();
        state.mark(&frag.name);
    }
    Ok(())
}

/// Insert `value` at `path` in the catch all, creating missing objects.
fn insert_into_catch_all(
    set: &mut FragmentSet,
    path: &Pointer,
    value: &Value,
    state: &mut Routing,
) -> Result<()> {
    let (parent, last) = split_last(path)?;
    let idx = set.catch_all_index();
    let frag = &mut set.fragments[idx];

    let map = object_at(&parent, &mut frag.value)?;
    if map.get(last.as_str()) != Some(value) {
        // `insert` on an existing key keeps its position under
        // `preserve_order`, so inserting it again never reorders the file.
        map.insert(last, value.clone());
        state.mark(&frag.name);
    }
    Ok(())
}

/// Remove `path` from every fragment declaring it.
fn unset_everywhere(set: &mut FragmentSet, path: &Pointer, state: &mut Routing) {
    for idx in set.declarers_of(path) {
        let frag = &mut set.fragments[idx];
        if path.remove_from(&mut frag.value).is_some() {
            state.mark(&frag.name);
        }
    }
}

/// Apply a membership delta: `removed` from every contributor, `added` to the
/// catch all.
fn array_members(
    set: &mut FragmentSet,
    path: &Pointer,
    added: &[Value],
    removed: &[Value],
    state: &mut Routing,
) -> Result<()> {
    if !removed.is_empty() {
        for frag in &mut set.fragments {
            // Only fragments that actually contribute an array here: a
            // fragment whose value at `path` is a scalar was overridden
            // wholesale by `combine` and contributed no elements.
            let Some(Value::Array(arr)) = path.resolve_mut(&mut frag.value) else {
                continue;
            };
            let before = arr.len();
            arr.retain(|e| !removed.contains(e));
            if arr.len() != before {
                state.mark(&frag.name);
            }
        }
    }

    if added.is_empty() {
        return Ok(());
    }

    let (parent, last) = split_last(path)?;
    let idx = set.catch_all_index();
    let frag = &mut set.fragments[idx];

    let map = object_at(&parent, &mut frag.value)?;
    let slot = map.entry(last).or_insert_with(|| Value::Array(Vec::new()));
    let Value::Array(arr) = slot else {
        return Err(Error::RouteTypeConflict(path.as_str()));
    };

    let mut appended = false;
    for v in added {
        // Skipping elements already present is what makes law C
        // (idempotence) hold: routing the same delta twice must not grow the
        // catch all's array.
        if !arr.contains(v) {
            arr.push(v.clone());
            appended = true;
        }
    }
    if appended {
        state.mark(&frag.name);
    }
    Ok(())
}

/// Split a pointer that is not the root into its parent and its last decoded segment.
fn split_last(path: &Pointer) -> Result<(Pointer, String)> {
    let parent = path.parent().ok_or_else(root_conflict)?;
    let last = path
        .segments()
        .last()
        .expect("a non-root pointer has a last segment")
        .clone();
    Ok((parent, last))
}

/// The value at `parent`, as an object, creating missing steps on the way.
fn object_at<'a>(parent: &Pointer, v: &'a mut Value) -> Result<&'a mut Map<String, Value>> {
    match parent.ensure_object_path(v)? {
        Value::Object(map) => Ok(map),
        _ => Err(Error::RouteTypeConflict(parent.as_str())),
    }
}

/// Nothing can be filed *at* the document root: a fragment root is an object,
/// and replacing it wholesale would discard every other key in the file.
/// Defensive. `classify` recurses into two objects rather than emitting a
/// root level change.
fn root_conflict() -> Error {
    Error::RouteTypeConflict("<document root>".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::classify;
    use crate::merge::combine_values;
    use serde_json::json;
    use std::path::PathBuf;

    /// A fragment as if loaded from disk: `raw` is `Some`, so it is never
    /// mistaken for a synthesised catch all.
    fn frag(name: &str, value: Value) -> crate::fragment::Fragment {
        crate::fragment::Fragment {
            path: PathBuf::from("/frags").join(name),
            name: name.to_string(),
            original: value.clone(),
            raw: Some(value.to_string()),
            value,
            had_comments: false,
        }
    }

    fn set_of(fragments: Vec<crate::fragment::Fragment>) -> FragmentSet {
        FragmentSet {
            dir: PathBuf::from("/frags"),
            fragments,
            catch_all: "99-local.json".to_string(),
        }
    }

    fn p(s: &str) -> Pointer {
        Pointer::parse(s).expect("valid pointer")
    }

    fn plain() -> StrategyTable {
        StrategyTable::default()
    }

    // ---- Set: the owner is the last declarer -----------------------------

    #[test]
    fn set_hits_only_the_last_declarer() {
        let mut set = set_of(vec![
            frag("10-a.json", json!({"a": 1})),
            frag("20-b.json", json!({"a": 2})),
            frag("30-c.json", json!({"a": 3})),
        ]);

        let report = route(
            &mut set,
            &[Change::Set {
                path: p("/a"),
                value: json!(9),
            }],
            &plain(),
        )
        .unwrap();

        assert_eq!(report.touched, vec![2]);
        assert!(!report.created_catch_all);
        // The shadowed fragments must come away byte identical.
        assert!(!set.fragments[0].is_dirty());
        assert!(!set.fragments[1].is_dirty());
        assert_eq!(set.fragments[0].value, json!({"a": 1}));
        assert_eq!(set.fragments[1].value, json!({"a": 2}));
        assert_eq!(set.fragments[2].value, json!({"a": 9}));
    }

    #[test]
    fn set_of_a_whole_array_overwrites_at_the_owner() {
        // The `replace` strategy: `classify` hands the whole array over as a
        // `Set`, so a `Set` value may legitimately be an array.
        let strategies =
            StrategyTable::from_pairs([("/list".to_string(), "replace".to_string())]).unwrap();
        let mut set = set_of(vec![
            frag("10-a.json", json!({"list": [1, 2]})),
            frag("20-b.json", json!({"list": [3]})),
        ]);

        let report = route(
            &mut set,
            &[Change::Set {
                path: p("/list"),
                value: json!([7, 8]),
            }],
            &strategies,
        )
        .unwrap();

        assert_eq!(report.touched, vec![1]);
        assert_eq!(set.fragments[0].value, json!({"list": [1, 2]}));
        assert_eq!(set.fragments[1].value, json!({"list": [7, 8]}));
    }

    #[test]
    fn set_to_an_identical_value_touches_nothing() {
        let mut set = set_of(vec![frag("10-a.json", json!({"a": 1}))]);

        let report = route(
            &mut set,
            &[Change::Set {
                path: p("/a"),
                value: json!(1),
            }],
            &plain(),
        )
        .unwrap();

        assert_eq!(report, RouteReport::default());
        assert!(!set.fragments[0].is_dirty());
    }

    // ---- Unset: every declarer -------------------------------------------

    #[test]
    fn unset_removes_from_every_declarer() {
        let mut set = set_of(vec![
            frag("10-a.json", json!({"a": 1, "keep": true})),
            frag("20-b.json", json!({"a": 2})),
            frag("30-c.json", json!({"other": 3})),
        ]);

        let report = route(&mut set, &[Change::Unset { path: p("/a") }], &plain()).unwrap();

        assert_eq!(report.touched, vec![0, 1]);
        assert_eq!(set.fragments[0].value, json!({"keep": true}));
        assert_eq!(set.fragments[1].value, json!({}));
        assert!(!set.fragments[2].is_dirty());
    }

    #[test]
    fn unset_leaves_the_emptied_object_in_place() {
        let mut set = set_of(vec![frag("10-a.json", json!({"a": {"b": {"c": 1}}}))]);

        route(&mut set, &[Change::Unset { path: p("/a/b/c") }], &plain()).unwrap();

        assert_eq!(set.fragments[0].value, json!({"a": {"b": {}}}));
    }

    // ---- Insert: the catch all -------------------------------------------

    #[test]
    fn insert_lands_in_the_catch_all_creating_intermediates() {
        let mut set = set_of(vec![
            frag("10-base.json", json!({"x": 1})),
            frag("99-local.json", json!({"kept": true})),
        ]);

        let report = route(
            &mut set,
            &[Change::Insert {
                path: p("/a/b"),
                value: json!({"c": 1}),
            }],
            &plain(),
        )
        .unwrap();

        assert_eq!(report.touched, vec![1]);
        // The catch all existed on disk, so nothing was created.
        assert!(!report.created_catch_all);
        assert!(!set.fragments[0].is_dirty());
        assert_eq!(
            set.fragments[1].value,
            json!({"kept": true, "a": {"b": {"c": 1}}})
        );
    }

    #[test]
    fn insert_into_an_absent_catch_all_reports_it_created() {
        let mut set = set_of(vec![frag("10-base.json", json!({"x": 1}))]);

        let report = route(
            &mut set,
            &[Change::Insert {
                path: p("/a"),
                value: json!(1),
            }],
            &plain(),
        )
        .unwrap();

        assert!(report.created_catch_all);
        assert_eq!(report.touched, vec![1]);
        assert_eq!(set.fragments.len(), 2);
        assert_eq!(set.fragments[1].name, "99-local.json");
        assert_eq!(set.fragments[1].value, json!({"a": 1}));
        assert!(set.fragments[1].is_dirty());
    }

    #[test]
    fn a_run_of_overwrites_never_materialises_the_catch_all() {
        let mut set = set_of(vec![frag("10-base.json", json!({"a": 1}))]);

        let report = route(
            &mut set,
            &[Change::Set {
                path: p("/a"),
                value: json!(2),
            }],
            &plain(),
        )
        .unwrap();

        assert!(!report.created_catch_all);
        // No speculative synthesis: an empty 99-local.json must not appear.
        assert_eq!(set.fragments.len(), 1);
    }

    // ---- ArrayMembers ----------------------------------------------------

    #[test]
    fn array_removal_hits_every_contributing_fragment() {
        // Leaving "b" in the lower fragment would resurrect it on the next
        // `combine`, because `concat-dedupe` concatenates across fragments.
        let mut set = set_of(vec![
            frag("10-a.json", json!({"tags": ["a", "b"]})),
            frag("20-b.json", json!({"tags": ["b", "c"]})),
            frag("30-c.json", json!({"tags": "not an array"})),
        ]);

        let report = route(
            &mut set,
            &[Change::ArrayMembers {
                path: p("/tags"),
                added: vec![],
                removed: vec![json!("b")],
            }],
            &plain(),
        )
        .unwrap();

        assert_eq!(report.touched, vec![0, 1]);
        assert_eq!(set.fragments[0].value, json!({"tags": ["a"]}));
        assert_eq!(set.fragments[1].value, json!({"tags": ["c"]}));
        assert!(!set.fragments[2].is_dirty());
    }

    #[test]
    fn array_addition_creates_the_array_and_its_ancestors_in_the_catch_all() {
        let mut set = set_of(vec![
            frag("10-base.json", json!({"p": {"allow": ["x"]}})),
            frag("99-local.json", json!({})),
        ]);

        let report = route(
            &mut set,
            &[Change::ArrayMembers {
                path: p("/p/allow"),
                added: vec![json!("y")],
                removed: vec![],
            }],
            &plain(),
        )
        .unwrap();

        assert_eq!(report.touched, vec![1]);
        assert!(!set.fragments[0].is_dirty());
        assert_eq!(set.fragments[1].value, json!({"p": {"allow": ["y"]}}));
    }

    #[test]
    fn routing_the_same_addition_twice_does_not_duplicate_it() {
        let mut set = set_of(vec![
            frag("10-base.json", json!({"p": {"allow": ["x"]}})),
            frag("99-local.json", json!({})),
        ]);
        let changes = [Change::ArrayMembers {
            path: p("/p/allow"),
            added: vec![json!("y")],
            removed: vec![],
        }];

        route(&mut set, &changes, &plain()).unwrap();
        let second = route(&mut set, &changes, &plain()).unwrap();

        assert_eq!(set.fragments[1].value, json!({"p": {"allow": ["y"]}}));
        // The second pass changed nothing at all.
        assert_eq!(second, RouteReport::default());
    }

    #[test]
    fn a_non_array_in_the_catch_all_is_a_route_type_conflict() {
        let mut set = set_of(vec![
            frag("10-base.json", json!({"p": {"allow": ["x"]}})),
            frag("99-local.json", json!({"p": {"allow": 5}})),
        ]);

        let err = route(
            &mut set,
            &[Change::ArrayMembers {
                path: p("/p/allow"),
                added: vec![json!("y")],
                removed: vec![],
            }],
            &plain(),
        )
        .unwrap_err();

        assert!(matches!(err, Error::RouteTypeConflict(ref s) if s == "/p/allow"));
        assert!(err.to_string().contains("/p/allow"));
    }

    // ---- the report ------------------------------------------------------

    #[test]
    fn touched_is_ascending_deduped_and_exact() {
        let mut set = set_of(vec![
            frag("10-a.json", json!({"a": 1, "b": 2})),
            frag("20-b.json", json!({"a": 9, "c": 3})),
            frag("30-c.json", json!({"untouched": true})),
            frag("99-local.json", json!({})),
        ]);

        let report = route(
            &mut set,
            &[
                // owner of /a is fragment 1 …
                Change::Set {
                    path: p("/a"),
                    value: json!(5),
                },
                // … owner of /b is fragment 0 …
                Change::Set {
                    path: p("/b"),
                    value: json!(6),
                },
                // … and /c is declared only by fragment 1, hit a second time.
                Change::Unset { path: p("/c") },
                Change::Insert {
                    path: p("/new"),
                    value: json!(true),
                },
            ],
            &plain(),
        )
        .unwrap();

        assert_eq!(report.touched, vec![0, 1, 3]);
        assert!(!report.created_catch_all);
        assert!(!set.fragments[2].is_dirty());
        assert_eq!(set.fragments[0].value, json!({"a": 1, "b": 6}));
        assert_eq!(set.fragments[1].value, json!({"a": 5}));
        assert_eq!(set.fragments[3].value, json!({"new": true}));
    }

    // ---- round trip: a mini law B ----------------------------------------

    /// `Value` equality, except that arrays compare as multisets.
    ///
    /// Law B holds only up to array element order under `concat-dedupe`:
    /// `combine` emits elements once more in fragment order, so an element routed to
    /// the catch all comes back last however the external program ordered it.
    fn eq_up_to_array_order(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len()
                    && x.iter()
                        .all(|(k, v)| y.get(k).is_some_and(|w| eq_up_to_array_order(v, w)))
            }
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len()
                    && x.iter().all(|v| y.contains(v))
                    && y.iter().all(|v| x.contains(v))
            }
            _ => a == b,
        }
    }

    #[test]
    fn round_trip_combine_edit_classify_route_combine() {
        let strategies = plain();
        let mut set = set_of(vec![
            frag(
                "10-base.json",
                json!({
                    "model": "opus",
                    "permissions": {"allow": ["Read", "Bash"]},
                    "nested": {"keep": true}
                }),
            ),
            frag(
                "20-hooks.json",
                json!({
                    "hooks": {"pre": "x"},
                    "permissions": {"allow": ["Write"]}
                }),
            ),
            frag("99-local.json", json!({"model": "sonnet"})),
        ]);

        let base = combine_values(&set.fragments, &strategies);
        assert_eq!(
            base,
            json!({
                "model": "sonnet",
                "permissions": {"allow": ["Read", "Bash", "Write"]},
                "nested": {"keep": true},
                "hooks": {"pre": "x"}
            })
        );

        // An external program rewrites the combined file: one overwrite, one
        // brand new key, one deletion, and one array membership change in
        // both directions.
        let output = json!({
            "model": "haiku",
            "permissions": {"allow": ["Read", "Write", "Edit"]},
            "nested": {"keep": true},
            "newKey": {"deep": 1}
        });

        let changes = classify(&base, &output, &strategies).unwrap();
        let report = route(&mut set, &changes, &strategies).unwrap();
        assert!(!report.created_catch_all);
        assert!(!report.touched.is_empty());

        // Each edit landed where the routing table says it should.
        assert_eq!(
            set.fragments[0].value,
            json!({
                "model": "opus",
                "permissions": {"allow": ["Read"]},
                "nested": {"keep": true}
            })
        );
        assert_eq!(
            set.fragments[1].value,
            json!({"permissions": {"allow": ["Write"]}})
        );
        assert_eq!(
            set.fragments[2].value,
            json!({
                "model": "haiku",
                "permissions": {"allow": ["Edit"]},
                "newKey": {"deep": 1}
            })
        );

        // Law B: combining the routed fragments reproduces the external edit.
        let recombined = combine_values(&set.fragments, &strategies);
        assert!(
            eq_up_to_array_order(&recombined, &output),
            "recombined {recombined} != output {output}"
        );

        // Law C, in miniature: a second split against the same output is a
        // no op, and `classify` now sees nothing left to do.
        let again = classify(&recombined, &output, &strategies).unwrap();
        assert!(again.is_empty(), "expected no residual changes: {again:?}");
    }
}
