//! The fold that turns fragments into one combined document.
//!
//! [`combine_values`] is a pure left fold: deep merge each fragment's root
//! object into an accumulator, in slice order. It is infallible: fragment
//! roots are validated as objects at load time ([`crate::fragment`]), so
//! there is nothing left to fail on here.

use serde_json::{Map, Value};

use crate::fragment::Fragment;
use crate::pointer::{ArrayStrategy, Pointer, StrategyTable};

/// Fold `frags` left into one combined [`Value::Object`].
///
/// Each fragment is deep merged into the accumulator in slice order
/// (fragments are expected pre sorted by name, as [`crate::fragment::FragmentSet`]
/// guarantees):
///
/// - object merged with object: recurse per key
/// - array merged with array: the strategy `strategies.lookup` returns for
///   that path
/// - everything else, including type mismatches: the later fragment wins
///   wholesale
///
/// Output key order is first seen insertion order across the whole fold: a
/// key keeps the position of the fragment that introduced it even when a
/// later fragment overwrites its value. An empty slice yields `{}`.
#[must_use]
pub fn combine_values(frags: &[Fragment], strategies: &StrategyTable) -> Value {
    let mut acc = Value::Object(Map::new());
    for frag in frags {
        merge_into(&mut acc, &frag.value, &Pointer::root(), strategies);
    }
    acc
}

/// Merge `new` into `acc` in place, at `ptr`.
///
/// `ptr` accumulates object keys only: the fold never descends into array
/// elements, so an array index can never appear in a pointer passed to
/// [`StrategyTable::lookup`].
fn merge_into(acc: &mut Value, new: &Value, ptr: &Pointer, strategies: &StrategyTable) {
    match (&mut *acc, new) {
        (Value::Object(acc_map), Value::Object(new_map)) => {
            for (k, v) in new_map {
                match acc_map.get_mut(k) {
                    // Mutate the existing entry in place. Under
                    // `preserve_order`, remove then insert would move the key
                    // to the end and destroy first seen ordering.
                    Some(existing) => merge_into(existing, v, &ptr.child(k), strategies),
                    None => {
                        acc_map.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            *acc = match strategies.lookup(ptr) {
                ArrayStrategy::Replace => new.clone(),
                ArrayStrategy::ConcatDedupe => {
                    let mut result: Vec<Value> = Vec::with_capacity(a.len() + b.len());
                    for v in a.iter().chain(b.iter()) {
                        if !result.contains(v) {
                            result.push(v.clone());
                        }
                    }
                    Value::Array(result)
                }
            };
        }
        // Type mismatch, or any scalar collision: later wins wholesale.
        _ => {
            *acc = new.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    /// Build a `Fragment` from a name and a value, for merge tests only.
    /// `original`/`raw` are irrelevant to `combine_values`.
    fn frag(name: &str, value: Value) -> Fragment {
        Fragment {
            path: PathBuf::from(name),
            name: name.to_string(),
            original: value.clone(),
            value,
            raw: None,
            had_comments: false,
        }
    }

    fn combine(frags: &[Fragment]) -> Value {
        combine_values(frags, &StrategyTable::default())
    }

    // ---- object recursion ------------------------------------------------

    #[test]
    fn disjoint_keys_from_two_fragments_both_survive() {
        let frags = [
            frag("1.json", json!({"a": 1})),
            frag("2.json", json!({"b": 2})),
        ];
        assert_eq!(combine(&frags), json!({"a": 1, "b": 2}));
    }

    #[test]
    fn later_fragment_wins_for_scalar_collision() {
        let frags = [
            frag("1.json", json!({"a": 1})),
            frag("2.json", json!({"a": 2})),
        ];
        assert_eq!(combine(&frags), json!({"a": 2}));
    }

    // ---- type mismatch -----------------------------------------------------

    #[test]
    fn type_mismatch_object_then_scalar_scalar_wins() {
        let frags = [
            frag("1.json", json!({"a": {"nested": true}})),
            frag("2.json", json!({"a": "scalar"})),
        ];
        assert_eq!(combine(&frags), json!({"a": "scalar"}));
    }

    #[test]
    fn type_mismatch_scalar_then_object_object_wins() {
        let frags = [
            frag("1.json", json!({"a": "scalar"})),
            frag("2.json", json!({"a": {"nested": true}})),
        ];
        assert_eq!(combine(&frags), json!({"a": {"nested": true}}));
    }

    // ---- ConcatDedupe --------------------------------------------------

    #[test]
    fn concat_dedupe_concatenates_earlier_then_later() {
        let frags = [
            frag("1.json", json!({"a": [1, 2]})),
            frag("2.json", json!({"a": [3, 4]})),
        ];
        assert_eq!(combine(&frags), json!({"a": [1, 2, 3, 4]}));
    }

    #[test]
    fn concat_dedupe_drops_later_duplicates_keeps_first_occurrence() {
        let frags = [
            frag("1.json", json!({"a": [1, 2]})),
            frag("2.json", json!({"a": [2, 3]})),
        ];
        assert_eq!(combine(&frags), json!({"a": [1, 2, 3]}));
    }

    #[test]
    fn concat_dedupe_dedupes_by_full_deep_equality() {
        let frags = [
            frag("1.json", json!({"a": [{"a": 1}, "x"]})),
            frag("2.json", json!({"a": [{"a": 1}, "y"]})),
        ];
        // The two structurally equal {"a":1} objects collapse to one.
        assert_eq!(combine(&frags), json!({"a": [{"a": 1}, "x", "y"]}));
    }

    #[test]
    fn concat_dedupe_dedupes_across_prior_fold_step_duplicates() {
        // The accumulator arriving at fragment 3 may already contain
        // duplicates from folding 1 and 2. The final result must still have
        // none at all.
        let frags = [
            frag("1.json", json!({"a": [1, 1]})),
            frag("2.json", json!({"a": [1, 2]})),
        ];
        assert_eq!(combine(&frags), json!({"a": [1, 2]}));
    }

    // ---- Replace ---------------------------------------------------------

    #[test]
    fn replace_wholly_replaces_no_concatenation() {
        let strategies =
            StrategyTable::from_pairs([("/a".to_string(), "replace".to_string())]).unwrap();
        let frags = [
            frag("1.json", json!({"a": [1, 2, 3]})),
            frag("2.json", json!({"a": [4]})),
        ];
        assert_eq!(combine_values(&frags, &strategies), json!({"a": [4]}));
    }

    // ---- wildcard strategy lookup ---------------------------------------

    #[test]
    fn wildcard_strategy_path_selects_strategy_for_nested_array() {
        let strategies =
            StrategyTable::from_pairs([("/a/*".to_string(), "replace".to_string())]).unwrap();
        let frags = [
            frag("1.json", json!({"a": {"b": [1, 2]}})),
            frag("2.json", json!({"a": {"b": [3]}})),
        ];
        assert_eq!(
            combine_values(&frags, &strategies),
            json!({"a": {"b": [3]}})
        );
    }

    // ---- key order ---------------------------------------------------------

    #[test]
    fn first_seen_key_order_survives_a_later_overwrite() {
        let frags = [
            frag("1.json", json!({"b": 1, "a": 2})),
            frag("2.json", json!({"b": 3})),
        ];
        let out = combine(&frags);
        assert_eq!(out, json!({"b": 3, "a": 2}));
        assert_eq!(out.to_string(), r#"{"b":3,"a":2}"#);
    }

    // ---- determinism -------------------------------------------------------

    #[test]
    fn combine_is_deterministic_across_repeated_runs() {
        let frags = [
            frag("1.json", json!({"b": 1, "a": [1, 2], "c": {"x": 1}})),
            frag("2.json", json!({"b": 2, "a": [2, 3], "c": {"y": 2}})),
            frag("3.json", json!({"d": [1, 1, 2]})),
        ];
        let first = combine(&frags).to_string();
        for _ in 0..50 {
            assert_eq!(combine(&frags).to_string(), first);
        }
    }

    // ---- empty and multi fragment folds -------------------------------------

    #[test]
    fn empty_slice_gives_empty_object() {
        assert_eq!(combine(&[]), json!({}));
    }

    #[test]
    fn three_fragment_fold_middle_shadowed_by_last() {
        let frags = [
            frag("1.json", json!({"a": 1})),
            frag("2.json", json!({"a": 2})),
            frag("3.json", json!({"a": 3})),
        ];
        assert_eq!(combine(&frags), json!({"a": 3}));
    }
}
