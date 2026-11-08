//! What changed between two frames, so a server sends a window only that:
//! a typed key changes a line, the modeline and the cursor, not the screen.
//! Frames are compared as JSON values; arrays (a window's lines) may also
//! have moved, as when the view scrolls, and then their items are matched
//! by their text and only what differs within each is sent.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum Patch {
    /// The value, whole.
    Set(Value),
    /// An object's changed keys (missing keys are removed).
    Object {
        changed: Vec<(String, Patch)>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        removed: Vec<String>,
    },
    /// An array: its new length, items moved by `shift` (item i was item
    /// i + shift), and the items that differ from that.
    Array {
        len: usize,
        #[serde(default, skip_serializing_if = "is_zero")]
        shift: isize,
        items: Vec<(usize, Patch)>,
    },
}

fn is_zero(n: &isize) -> bool {
    *n == 0
}

/// What turns `old` into `new`; None when they're the same.
pub fn diff(old: &Value, new: &Value) -> Option<Patch> {
    if old == new {
        return None;
    }
    Some(match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            let changed: Vec<(String, Patch)> = b
                .iter()
                .filter_map(|(k, v)| match a.get(k) {
                    Some(old) => Some((k.clone(), diff(old, v)?)),
                    None => Some((k.clone(), Patch::Set(v.clone()))),
                })
                .collect();
            let removed: Vec<String> = a.keys().filter(|k| !b.contains_key(*k)).cloned().collect();
            Patch::Object { changed, removed }
        }
        (Value::Array(a), Value::Array(b)) => {
            let shift = best_shift(a, b);
            let items = (0..b.len())
                .filter_map(|i| {
                    let from = i as isize + shift;
                    let base = if from >= 0 { a.get(from as usize) } else { None };
                    match base {
                        Some(base) => diff(base, &b[i]).map(|p| (i, p)),
                        None => Some((i, Patch::Set(b[i].clone()))),
                    }
                })
                .collect();
            Patch::Array { len: b.len(), shift, items }
        }
        _ => Patch::Set(new.clone()),
    })
}

/// What identifies an item, for matching it after a move: its "text", when
/// it has one (a line), else all of it.
fn key(v: &Value) -> &Value {
    v.get("text").unwrap_or(v)
}

/// The move that matches the most items, when it beats not moving.
fn best_shift(a: &[Value], b: &[Value]) -> isize {
    let matches = |shift: isize| {
        (0..b.len())
            .filter(|&i| {
                let from = i as isize + shift;
                from >= 0 && a.get(from as usize).is_some_and(|x| key(x) == key(&b[i]))
            })
            .count()
    };
    let still = matches(0);
    if still == b.len() || b.len() < 4 {
        return 0;
    }
    let reach = a.len().max(b.len()) as isize;
    let (mut best, mut most) = (0, still);
    for shift in (-reach + 1)..reach {
        let n = matches(shift);
        if n > most {
            (best, most) = (shift, n);
        }
    }
    best
}

/// Applies `patch` to `value`; false when it doesn't fit (the window then
/// asks for the whole frame).
pub fn apply(value: &mut Value, patch: Patch) -> bool {
    match patch {
        Patch::Set(v) => {
            *value = v;
            true
        }
        Patch::Object { changed, removed } => {
            if !value.is_object() {
                *value = Value::Object(Map::new());
            }
            let map = value.as_object_mut().expect("an object");
            for k in removed {
                map.remove(&k);
            }
            changed.into_iter().all(|(k, p)| apply(map.entry(k).or_insert(Value::Null), p))
        }
        Patch::Array { len, shift, items } => {
            let Value::Array(old) = value else { return false };
            let mut new: Vec<Value> = (0..len)
                .map(|i| {
                    let from = i as isize + shift;
                    if from >= 0 { old.get(from as usize).cloned().unwrap_or(Value::Null) } else { Value::Null }
                })
                .collect();
            for (i, p) in items {
                let Some(slot) = new.get_mut(i) else { return false };
                if !apply(slot, p) {
                    return false;
                }
            }
            *old = new;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(old: Value, new: Value) -> Option<Patch> {
        let patch = diff(&old, &new);
        let mut applied = old.clone();
        if let Some(p) = patch.clone() {
            assert!(apply(&mut applied, p));
        }
        assert_eq!(applied, new);
        patch
    }

    #[test]
    fn a_patch_turns_one_into_the_other() {
        assert_eq!(round_trip(json!({"a": 1}), json!({"a": 1})), None);
        round_trip(json!({"a": 1, "b": [1, 2, 3], "c": "x"}), json!({"a": 2, "b": [1, 2], "d": null}));
        round_trip(json!([1, 2]), json!([1, 2, 3, 4, 5]));
        round_trip(json!({"a": [ {"x": 1} ]}), json!({"a": "now a string"}));
    }

    #[test]
    fn scrolled_lines_are_matched_by_their_text() {
        // As a window's lines are: the text, where each character starts, ...
        let line = |n: usize, top: usize| {
            let text = format!("let value_{n} = compute(alpha, beta, gamma);");
            json!({"text": text, "top": top, "line": n, "cols": (0..=text.len()).collect::<Vec<_>>(),
                   "styles": [[0, 3, {"color": "magenta", "bold": false}]]})
        };
        let old: Vec<Value> = (0..40).map(|n| line(n, n * 20)).collect();
        // Scrolled down by 10: the same text, other numbers and positions.
        let new: Vec<Value> = (10..50).map(|n| line(n, (n - 10) * 20)).collect();
        let patch = round_trip(Value::Array(old), Value::Array(new)).unwrap();
        let Patch::Array { shift, items, .. } = &patch else { panic!() };
        assert_eq!(*shift, 10);
        // The 30 kept lines change only their position; 10 are new.
        assert_eq!(items.iter().filter(|(_, p)| matches!(p, Patch::Set(_))).count(), 10);
        let whole = serde_json::to_string(&json!((10..50).map(|n| line(n, 0)).collect::<Vec<_>>())).unwrap().len();
        assert!(serde_json::to_string(&patch).unwrap().len() < whole);
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;
    use crate::buffer::Buffer;
    use crate::editor::Editor;
    use crate::frame::{Input, Viewport};
    use crate::lisp::Lisp;
    use crate::session::Session;

    /// Scrolling a real window sends what scrolled into view, not the frame.
    #[test]
    fn scrolling_sends_little() {
        let text: String = (1..=400).map(|i| format!("let value_{i} = compute(alpha, beta, gamma) + {i};\n")).collect();
        let mut buf = Buffer::new("long.rs");
        buf.insert(&text);
        buf.goto_char(0);
        let editor = Editor::new(buf);
        let mut session = Session::new(editor.clone(), Lisp::new(editor));
        let view = Viewport { width: 1320., height: 840. };
        let mut last = serde_json::to_value(session.frame(view)).unwrap();
        let mut report = Vec::new();
        for key in ["C-d", "C-d", "C-u", "C-e", "C-e", "j"] {
            session.handle(Input::Key { key: key.into(), text: String::new() });
            let now = serde_json::to_value(session.frame(view)).unwrap();
            let patch = diff(&last, &now).unwrap();
            let size = serde_json::to_string(&patch).unwrap().len();
            let whole = serde_json::to_string(&now).unwrap().len();
            let mut applied = last.clone();
            assert!(apply(&mut applied, patch.clone()));
            assert_eq!(applied, now);
            report.push((key, size, whole));
            last = now;
        }
        // Half a page brings half the lines new; a line, one; j, none.
        for (key, size, whole) in &report {
            let most = match *key {
                "C-d" | "C-u" => whole * 2 / 3,
                "C-e" => whole / 4,
                _ => whole / 10,
            };
            assert!(*size < most, "{key}: {size} of {whole}: {report:?}");
        }
    }
}
