//! The entire client-side "eval" story for emerged UIs: dotted-path get/set over
//! a JSON state object, JS-like truthiness, `{{path}}` interpolation, and the
//! [`Scope`] that maps `for_each` loop variables (`item`, `index`) onto absolute
//! state paths. There is deliberately **no** expression language — anything richer
//! is a server-side Boa script. These mirror `catalerum-core::model_ui`'s helpers
//! and are the conformance reference for the wasm renderer.

use serde_json::Value as Json;

/// Read a dotted path out of a JSON value. Missing → [`Json::Null`].
#[must_use]
pub fn get_path<'a>(root: &'a Json, path: &str) -> &'a Json {
    let mut cur = root;
    for seg in path.split('.') {
        match cur {
            Json::Object(map) => match map.get(seg) {
                Some(v) => cur = v,
                None => return &Json::Null,
            },
            Json::Array(arr) => match seg.parse::<usize>().ok().and_then(|i| arr.get(i)) {
                Some(v) => cur = v,
                None => return &Json::Null,
            },
            _ => return &Json::Null,
        }
    }
    cur
}

/// Write a value at a dotted path, creating intermediate objects as needed.
/// Array segments (numeric) are written positionally when the array is long
/// enough; otherwise the write is a no-op (the AI seeds arrays via state, not
/// sparse index writes).
pub fn set_path(root: &mut Json, path: &str, value: Json) {
    let mut segs = path.split('.').peekable();
    let mut cur = root;
    while let Some(seg) = segs.next() {
        let last = segs.peek().is_none();
        match cur {
            Json::Array(arr) => {
                let Ok(i) = seg.parse::<usize>() else { return };
                if i >= arr.len() {
                    return;
                }
                if last {
                    arr[i] = value;
                    return;
                }
                cur = &mut arr[i];
            }
            other => {
                if !other.is_object() {
                    *other = Json::Object(serde_json::Map::new());
                }
                let map = other.as_object_mut().expect("just set to object");
                if last {
                    map.insert(seg.to_string(), value);
                    return;
                }
                cur = map
                    .entry(seg.to_string())
                    .or_insert_with(|| Json::Object(serde_json::Map::new()));
            }
        }
    }
}

/// JS-like truthiness: `false`/`null`/`0`/`""`/`[]`/`{}`/absent are falsy.
#[must_use]
pub fn truthy(v: &Json) -> bool {
    match v {
        Json::Null => false,
        Json::Bool(b) => *b,
        Json::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Json::String(s) => !s.is_empty(),
        Json::Array(a) => !a.is_empty(),
        Json::Object(o) => !o.is_empty(),
    }
}

/// Render a JSON value as plain display text (strings verbatim, scalars
/// stringified, `null` → empty, containers → compact JSON).
#[must_use]
pub fn stringify(v: &Json) -> String {
    match v {
        Json::Null => String::new(),
        Json::String(s) => s.clone(),
        Json::Bool(b) => b.to_string(),
        Json::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// for_each scope
// ---------------------------------------------------------------------------

#[derive(Clone)]
enum ScopeVal {
    /// A loop item: the absolute state path of the array element it aliases
    /// (e.g. `tasks.0`), so `item.title` resolves to `tasks.0.title`.
    ItemBase(String),
    /// A loop index variable bound to a concrete row number.
    Index(usize),
}

/// Total rendered `for_each` rows a subtree may still produce. Seeded at the
/// root and divided among a `for_each`'s rows so nested loops cannot multiply
/// without bound (defence-in-depth against an adversarial AI-authored spec; the
/// server validator is the primary gate).
pub const ROW_BUDGET: usize = 4096;

/// The lexical scope a binding path resolves against: the root state plus any
/// enclosing `for_each` loop variables, and the remaining row budget. Cheap to
/// clone (a small `Vec`); threaded immutably down the render tree.
#[derive(Clone)]
pub struct Scope {
    binds: Vec<(String, ScopeVal)>,
    budget: usize,
}

impl Default for Scope {
    fn default() -> Scope {
        Scope {
            binds: Vec::new(),
            budget: ROW_BUDGET,
        }
    }
}

impl Scope {
    /// A child scope with a `for_each` item variable bound to an absolute path.
    #[must_use]
    pub fn with_item(&self, name: &str, base: String) -> Scope {
        let mut next = self.clone();
        next.binds
            .push((name.to_string(), ScopeVal::ItemBase(base)));
        next
    }

    /// A child scope with a `for_each` index variable bound to a row number.
    #[must_use]
    pub fn with_index(&self, name: &str, i: usize) -> Scope {
        let mut next = self.clone();
        next.binds.push((name.to_string(), ScopeVal::Index(i)));
        next
    }

    /// A child scope carrying a reduced row budget (for a `for_each`'s rows).
    #[must_use]
    pub fn with_budget(&self, budget: usize) -> Scope {
        let mut next = self.clone();
        next.budget = budget;
        next
    }

    /// The remaining row budget at this point in the tree.
    #[must_use]
    pub fn budget(&self) -> usize {
        self.budget
    }

    /// Snapshot the in-scope `for_each` bindings as a JSON object
    /// (`{ <item>: <value>, <index>: <n> }`) for the server event payload: a
    /// handler's `{{item.x}}` references resolve against `state ∪ scope`, and a
    /// script reads them at `input.scope`. Item vars resolve to their aliased
    /// state value, index vars to the row number; inner bindings shadow outer.
    #[must_use]
    pub fn resolve(&self, data: &Json) -> Json {
        let mut obj = serde_json::Map::new();
        for (name, val) in &self.binds {
            let resolved = match val {
                ScopeVal::ItemBase(base) => get_path(data, base).clone(),
                ScopeVal::Index(i) => Json::from(*i),
            };
            obj.insert(name.clone(), resolved);
        }
        Json::Object(obj)
    }

    fn lookup(&self, head: &str) -> Option<&ScopeVal> {
        // Innermost binding wins (shadowing).
        self.binds
            .iter()
            .rev()
            .find(|(n, _)| n == head)
            .map(|(_, v)| v)
    }
}

fn split_head(path: &str) -> (&str, &str) {
    match path.split_once('.') {
        Some((h, r)) => (h, r),
        None => (path, ""),
    }
}

fn join(base: &str, rest: &str) -> String {
    if rest.is_empty() {
        base.to_string()
    } else {
        format!("{base}.{rest}")
    }
}

/// Resolve a (possibly scope-relative) binding path to its absolute state path,
/// for reads/writes against the data object. Returns `None` for an `index`
/// variable (a bare number, not a state location).
#[must_use]
pub fn abs_data_path(scope: &Scope, path: &str) -> Option<String> {
    let (head, rest) = split_head(path);
    match scope.lookup(head) {
        Some(ScopeVal::ItemBase(base)) => Some(join(base, rest)),
        Some(ScopeVal::Index(_)) => None,
        None => Some(path.to_string()),
    }
}

/// Resolve a (possibly scope-relative) path to an owned JSON value. Loop `index`
/// variables resolve to their row number; everything else reads from `data`.
#[must_use]
pub fn resolve_value(scope: &Scope, data: &Json, path: &str) -> Json {
    let (head, rest) = split_head(path);
    match scope.lookup(head) {
        Some(ScopeVal::Index(i)) => {
            if rest.is_empty() {
                Json::from(*i)
            } else {
                Json::Null
            }
        }
        Some(ScopeVal::ItemBase(base)) => get_path(data, &join(base, rest)).clone(),
        None => get_path(data, path).clone(),
    }
}

/// Replace every `{{ path }}` span in `template` with the stringified, scope-
/// resolved value. Unterminated/empty spans are left verbatim. The result is
/// inserted only into auto-escaping Leptos text/attribute positions (or run
/// through the XSS-safe markdown renderer), so no manual escaping happens here.
#[must_use]
pub fn interpolate(template: &str, data: &Json, scope: &Scope) -> String {
    if !template.contains("{{") {
        return template.to_string();
    }
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("}}") {
            Some(end) => {
                let expr = after[..end].trim();
                if expr.is_empty() {
                    out.push_str("{{");
                    rest = after;
                } else {
                    out.push_str(&stringify(&resolve_value(scope, data, expr)));
                    rest = &after[end + 2..];
                }
            }
            None => {
                // No closing `}}` — emit the rest verbatim.
                out.push_str("{{");
                rest = after;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn get_set_roundtrip() {
        let mut v = json!({});
        set_path(&mut v, "form.email", json!("a@b.c"));
        assert_eq!(get_path(&v, "form.email"), &json!("a@b.c"));
        assert_eq!(get_path(&v, "form.missing"), &Json::Null);
        set_path(&mut v, "form.email", json!("x"));
        assert_eq!(get_path(&v, "form.email"), &json!("x"));
    }

    #[test]
    fn set_into_array_element() {
        let mut v = json!({ "tasks": [{ "done": false }, { "done": false }] });
        set_path(&mut v, "tasks.1.done", json!(true));
        assert_eq!(get_path(&v, "tasks.1.done"), &json!(true));
        // Out-of-range index is a no-op (not a panic).
        set_path(&mut v, "tasks.9.done", json!(true));
        assert_eq!(get_path(&v, "tasks.9"), &Json::Null);
    }

    #[test]
    fn truthy_rules() {
        for f in [
            json!(0),
            json!(""),
            json!([]),
            json!({}),
            Json::Null,
            json!(false),
        ] {
            assert!(!truthy(&f), "{f:?} should be falsy");
        }
        for t in [json!(1), json!("x"), json!([1]), json!(true)] {
            assert!(truthy(&t), "{t:?} should be truthy");
        }
    }

    #[test]
    fn scope_resolves_loop_vars() {
        let data = json!({ "tasks": [{ "title": "a" }, { "title": "b" }] });
        let scope = Scope::default()
            .with_item("task", "tasks.1".to_string())
            .with_index("i", 1);
        assert_eq!(resolve_value(&scope, &data, "task.title"), json!("b"));
        assert_eq!(resolve_value(&scope, &data, "i"), json!(1));
        assert_eq!(
            abs_data_path(&scope, "task.title").unwrap(),
            "tasks.1.title"
        );
        assert!(abs_data_path(&scope, "i").is_none());
        // A non-scoped path falls through to the root.
        assert_eq!(abs_data_path(&scope, "form.x").unwrap(), "form.x");
    }

    #[test]
    fn interpolation() {
        let data = json!({ "form": { "name": "Jane" }, "n": 3 });
        let scope = Scope::default();
        assert_eq!(
            interpolate("Hi {{ form.name }} ({{n}})", &data, &scope),
            "Hi Jane (3)"
        );
        // Missing → empty; literal braces with no close left verbatim.
        assert_eq!(interpolate("a{{form.missing}}b", &data, &scope), "ab");
        assert_eq!(interpolate("plain", &data, &scope), "plain");
        assert_eq!(interpolate("{{ }}x", &data, &scope), "{{ }}x");
    }

    #[test]
    fn interpolation_with_loop_item() {
        let data = json!({ "rows": [{ "label": "one" }, { "label": "two" }] });
        let scope = Scope::default().with_item("row", "rows.0".to_string());
        assert_eq!(interpolate("[{{row.label}}]", &data, &scope), "[one]");
    }
}
