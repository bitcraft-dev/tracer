//! An OpenAPI document's schemas: checking a JSON value against one, and
//! making an example value from one.
//!
//! What's checked is the JSON Schema that OpenAPI 3.0, 3.1 and Swagger 2
//! use for bodies: `$ref` (within the document), `type` (one or a list,
//! `nullable` and `x-nullable`), `enum`, `const`, `allOf`, `anyOf`,
//! `oneOf`, `not`; an object's `properties`, `required`,
//! `additionalProperties`, `minProperties` and `maxProperties`; an array's
//! `items`, `minItems`, `maxItems` and `uniqueItems`; a string's
//! `minLength`, `maxLength`, `pattern` and the common `format`s
//! (`date-time`, `date`, `uuid`, `email`, `uri`, `ipv4`); a number's
//! `minimum`, `maximum`, the exclusive ones (as 3.0's booleans or 3.1's
//! numbers) and `multipleOf`. Anything else in a schema is let through.
//!
//! Fields an answer has that its schema doesn't name, where the schema
//! doesn't say whether others may come, are noted apart: they are drift,
//! not a failure.

use std::collections::BTreeSet;

use serde_json::{Map, Value};

/// How deep `$ref`s and nesting are followed before giving up: a schema
/// that refers to itself (a tree of comments) stops here.
const DEPTH: usize = 48;

/// What a check found wrong, at a JSON path such as `$.items[2].id`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Problem {
    pub at: String,
    pub what: String,
}

/// Checking against the schemas of one document, whose `$ref`s it follows.
pub struct Schemas<'a> {
    pub root: &'a Value,
}

/// What [`Schemas::check`] found.
#[derive(Debug, Default)]
pub struct Checked {
    pub problems: Vec<Problem>,
    /// Paths of fields that the schema doesn't name, such as `$.data.etag`.
    pub undocumented: Vec<String>,
}

impl<'a> Schemas<'a> {
    /// `schema`, its `$ref` followed (and the target's, and so on).
    pub fn resolve(&self, schema: &'a Value) -> &'a Value {
        let mut s = schema;
        for _ in 0..DEPTH {
            match s.get("$ref").and_then(Value::as_str) {
                Some(r) => match self.pointer(r) {
                    Some(target) => s = target,
                    None => return s,
                },
                None => return s,
            }
        }
        s
    }

    /// What a local `$ref` (`#/components/schemas/Pet`) points at.
    pub fn pointer(&self, reference: &str) -> Option<&'a Value> {
        let path = reference.strip_prefix('#')?;
        if path.is_empty() {
            return Some(self.root);
        }
        // Walked by hand, so `~1` in a key is a `/` and not a step.
        let mut at = self.root;
        for part in path.trim_start_matches('/').split('/') {
            let key = part.replace("~1", "/").replace("~0", "~");
            at = match at {
                Value::Object(m) => m.get(&key)?,
                Value::Array(a) => a.get(key.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(at)
    }

    /// The name a schema is known by: `Pet` for `#/components/schemas/Pet`.
    pub fn name_of(schema: &Value) -> Option<String> {
        let r = schema.get("$ref")?.as_str()?;
        Some(r.rsplit('/').next()?.to_string())
    }

    /// `value` against `schema`.
    pub fn check(&self, schema: &'a Value, value: &Value) -> Checked {
        let mut out = Checked::default();
        self.walk(schema, value, "$", &mut out, true, 0);
        out
    }

    /// Whether `value` passes `schema` with nothing wrong.
    fn passes(&self, schema: &'a Value, value: &Value, depth: usize) -> bool {
        let mut out = Checked::default();
        self.walk(schema, value, "$", &mut out, false, depth);
        out.problems.is_empty()
    }

    fn walk(&self, schema: &'a Value, value: &Value, at: &str, out: &mut Checked, extras: bool, depth: usize) {
        if depth > DEPTH {
            return;
        }
        let s = self.resolve(schema);
        let Some(map) = s.as_object() else {
            // `true`, `{}`: anything. `false`: nothing.
            if s == &Value::Bool(false) {
                problem(out, at, "is here, and the schema allows nothing here".into());
            }
            return;
        };

        // null, where it's allowed, is the whole check.
        if value.is_null() && nullable(map) {
            return;
        }
        if let Some(types) = types(map) {
            let fits = types.iter().any(|t| type_fits(t, value));
            if !fits {
                problem(out, at, format!("is {}, and the spec says {}", describe(value), types.join(" or ")));
                return;
            }
        } else if value.is_null() && !nullable(map) && has_shape(map) {
            problem(out, at, format!("is null, and the spec says {}", shape_word(map)));
            return;
        }
        if let Some(allowed) = map.get("enum").and_then(Value::as_array) {
            if !allowed.iter().any(|a| same(a, value)) {
                let shown: Vec<String> = allowed.iter().take(8).map(compact).collect();
                problem(out, at, format!("is {}, not one of {}", compact(value), shown.join(", ")));
            }
        }
        if let Some(c) = map.get("const") {
            if !same(c, value) {
                problem(out, at, format!("is {}, and the spec says always {}", compact(value), compact(c)));
            }
        }

        // Composition. Fields named by any branch count as documented.
        if let Some(all) = map.get("allOf").and_then(Value::as_array) {
            for branch in all {
                self.walk(branch, value, at, out, false, depth + 1);
            }
        }
        for (word, exactly_one) in [("anyOf", false), ("oneOf", true)] {
            let Some(branches) = map.get(word).and_then(Value::as_array) else { continue };
            let passing: Vec<usize> = (0..branches.len()).filter(|&i| self.passes(&branches[i], value, depth + 1)).collect();
            if passing.is_empty() {
                // Say what the nearest branch found, so the reason is readable.
                let mut best: Option<Checked> = None;
                for branch in branches {
                    let mut tried = Checked::default();
                    self.walk(branch, value, at, &mut tried, false, depth + 1);
                    if best.as_ref().is_none_or(|b| tried.problems.len() < b.problems.len()) {
                        best = Some(tried);
                    }
                }
                let names: Vec<String> = branches.iter().map(|b| Schemas::name_of(b).unwrap_or_else(|| shape_word(self.resolve(b).as_object().unwrap_or(&Map::new())))).collect();
                problem(out, at, format!("matches none of {word} [{}]", names.join(", ")));
                if let Some(b) = best {
                    out.problems.extend(b.problems.into_iter().take(3));
                }
            } else if exactly_one && passing.len() > 1 {
                problem(out, at, format!("matches {} of oneOf's branches, and must match exactly one", passing.len()));
            }
        }
        if let Some(not) = map.get("not") {
            if self.passes(not, value, depth + 1) {
                problem(out, at, "matches the schema under `not`".into());
            }
        }

        match value {
            Value::Object(fields) => self.object(map, fields, at, out, extras, depth),
            Value::Array(items) => self.array(map, items, at, out, depth),
            Value::String(text) => string(map, text, at, out),
            Value::Number(_) => number(map, value.as_f64().unwrap_or(0.0), at, out),
            _ => {}
        }
    }

    fn object(&self, map: &'a Map<String, Value>, fields: &Map<String, Value>, at: &str, out: &mut Checked, extras: bool, depth: usize) {
        let props = map.get("properties").and_then(Value::as_object);
        if let Some(props) = props {
            for (name, sub) in props {
                if let Some(v) = fields.get(name) {
                    self.walk(sub, v, &join(at, name), out, true, depth + 1);
                }
            }
        }
        if let Some(required) = map.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !fields.contains_key(name) {
                    problem(out, &join(at, name), "is missing, and the spec requires it".into());
                }
            }
        }
        let additional = map.get("additionalProperties");
        let patterns = map.get("patternProperties").and_then(Value::as_object);
        let known = self.known(map, depth);
        for (name, v) in fields {
            if props.is_some_and(|p| p.contains_key(name)) {
                continue;
            }
            if let Some(pp) = patterns {
                if let Some((_, sub)) = pp.iter().find(|(re, _)| regex_lite::Regex::new(re).is_ok_and(|r| r.is_match(name))) {
                    self.walk(sub, v, &join(at, name), out, true, depth + 1);
                    continue;
                }
            }
            match additional {
                Some(Value::Bool(false)) => problem(out, &join(at, name), "is here, and the spec allows no fields beyond those it names".into()),
                Some(sub @ Value::Object(_)) => self.walk(sub, v, &join(at, name), out, true, depth + 1),
                Some(_) => {}
                None => {
                    if extras && known.as_ref().is_some_and(|k| !k.contains(name)) {
                        out.undocumented.push(join(at, name));
                    }
                }
            }
        }
        if let Some(n) = map.get("minProperties").and_then(Value::as_u64) {
            if (fields.len() as u64) < n {
                problem(out, at, format!("has {} fields, fewer than the spec's minimum of {n}", fields.len()));
            }
        }
        if let Some(n) = map.get("maxProperties").and_then(Value::as_u64) {
            if (fields.len() as u64) > n {
                problem(out, at, format!("has {} fields, more than the spec's maximum of {n}", fields.len()));
            }
        }
    }

    /// The fields an object schema names, its `allOf`, `anyOf` and `oneOf`
    /// branches' too; `None` where it names none at all, or a branch lets
    /// any field through, so nothing can be said to be undocumented.
    fn known(&self, map: &'a Map<String, Value>, depth: usize) -> Option<BTreeSet<String>> {
        if depth > DEPTH {
            return None;
        }
        let mut names = BTreeSet::new();
        let mut any = false;
        if let Some(p) = map.get("properties").and_then(Value::as_object) {
            names.extend(p.keys().cloned());
            any = true;
        }
        if map.contains_key("additionalProperties") || map.contains_key("patternProperties") {
            return None;
        }
        for word in ["allOf", "anyOf", "oneOf"] {
            for branch in map.get(word).and_then(Value::as_array).into_iter().flatten() {
                let b = self.resolve(branch);
                let Some(bm) = b.as_object() else { return None };
                match self.known(bm, depth + 1) {
                    Some(more) => {
                        names.extend(more);
                        any = true;
                    }
                    None if bm.contains_key("properties") || bm.contains_key("additionalProperties") => return None,
                    None => {}
                }
            }
        }
        any.then_some(names)
    }

    fn array(&self, map: &'a Map<String, Value>, items: &[Value], at: &str, out: &mut Checked, depth: usize) {
        if let Some(n) = map.get("minItems").and_then(Value::as_u64) {
            if (items.len() as u64) < n {
                problem(out, at, format!("has {} items, fewer than the spec's minimum of {n}", items.len()));
            }
        }
        if let Some(n) = map.get("maxItems").and_then(Value::as_u64) {
            if (items.len() as u64) > n {
                problem(out, at, format!("has {} items, more than the spec's maximum of {n}", items.len()));
            }
        }
        if map.get("uniqueItems") == Some(&Value::Bool(true)) {
            for i in 0..items.len() {
                if let Some(j) = (0..i).find(|&j| same(&items[i], &items[j])) {
                    problem(out, &format!("{at}[{i}]"), format!("is the same as [{j}], and the spec says each item is different"));
                    break;
                }
            }
        }
        match map.get("items") {
            Some(Value::Array(each)) => {
                // Swagger's and draft 4's tuple form.
                for (i, (sub, v)) in each.iter().zip(items).enumerate() {
                    self.walk(sub, v, &format!("{at}[{i}]"), out, true, depth + 1);
                }
            }
            Some(sub) => {
                for (i, v) in items.iter().enumerate() {
                    self.walk(sub, v, &format!("{at}[{i}]"), out, true, depth + 1);
                    // A thousand broken rows say no more than a few.
                    if out.problems.len() > 50 {
                        break;
                    }
                }
            }
            None => {}
        }
    }

    /// An example of `schema`: its own `example`, `default`, `const` or
    /// first `enum` value where it has one, else made from its type.
    pub fn example(&self, schema: &'a Value) -> Value {
        self.make(schema, 0, &mut Vec::new())
    }

    fn make(&self, schema: &'a Value, depth: usize, seen: &mut Vec<String>) -> Value {
        let reference = schema.get("$ref").and_then(Value::as_str).map(String::from);
        if let Some(r) = &reference {
            // A schema met again inside itself stops the branch.
            if seen.contains(r) || depth > 12 {
                return Value::Null;
            }
            seen.push(r.clone());
        }
        let s = self.resolve(schema);
        let made = self.make_resolved(s, depth, seen);
        if reference.is_some() {
            seen.pop();
        }
        made
    }

    fn make_resolved(&self, s: &'a Value, depth: usize, seen: &mut Vec<String>) -> Value {
        let Some(map) = s.as_object() else { return Value::Null };
        for word in ["example", "default", "const"] {
            if let Some(v) = map.get(word) {
                return v.clone();
            }
        }
        if let Some(Value::Array(examples)) = map.get("examples") {
            if let Some(first) = examples.first() {
                return first.clone();
            }
        }
        if let Some(first) = map.get("enum").and_then(Value::as_array).and_then(|e| e.first()) {
            return first.clone();
        }
        if let Some(all) = map.get("allOf").and_then(Value::as_array) {
            let mut merged = Map::new();
            for branch in all {
                match self.make(branch, depth + 1, seen) {
                    Value::Object(m) => merged.extend(m),
                    other if merged.is_empty() => return other,
                    _ => {}
                }
            }
            return Value::Object(merged);
        }
        for word in ["oneOf", "anyOf"] {
            if let Some(first) = map.get(word).and_then(Value::as_array).and_then(|b| b.first()) {
                return self.make(first, depth + 1, seen);
            }
        }
        let ty = types(map).and_then(|t| t.into_iter().find(|t| t != "null")).unwrap_or_else(|| {
            if map.contains_key("properties") {
                "object".into()
            } else if map.contains_key("items") {
                "array".into()
            } else {
                String::new()
            }
        });
        match ty.as_str() {
            "object" => {
                let mut out = Map::new();
                if let Some(props) = map.get("properties").and_then(Value::as_object) {
                    for (name, sub) in props {
                        // A field that leads back into itself is left out.
                        match self.make(sub, depth + 1, seen) {
                            Value::Null if sub.get("$ref").is_some() => {}
                            v => {
                                out.insert(name.clone(), v);
                            }
                        }
                    }
                }
                Value::Object(out)
            }
            "array" => {
                let n = map.get("minItems").and_then(Value::as_u64).unwrap_or(1).clamp(1, 3);
                match map.get("items") {
                    Some(items @ Value::Object(_)) => match self.make(items, depth + 1, seen) {
                        // A list of itself (a tree's children) is empty.
                        Value::Null if items.get("$ref").is_some() => Value::Array(vec![]),
                        one => Value::Array((0..n).map(|_| one.clone()).collect()),
                    },
                    _ => Value::Array(vec![]),
                }
            }
            "string" => Value::String(sample_string(map)),
            "integer" => {
                let low = map.get("minimum").and_then(Value::as_i64).unwrap_or(1);
                Value::from(low.max(map.get("exclusiveMinimum").and_then(Value::as_i64).map_or(low, |m| m + 1)))
            }
            "number" => Value::from(map.get("minimum").and_then(Value::as_f64).unwrap_or(1.5)),
            "boolean" => Value::Bool(true),
            _ => Value::Null,
        }
    }
}

fn problem(out: &mut Checked, at: &str, what: String) {
    if out.problems.len() < 64 {
        out.problems.push(Problem { at: at.to_string(), what });
    }
}

/// `$.a` and `b` as `$.a.b`, or `$.a["odd key"]`.
pub fn join(at: &str, key: &str) -> String {
    if !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !key.starts_with(|c: char| c.is_ascii_digit()) {
        format!("{at}.{key}")
    } else {
        format!("{at}[{}]", serde_json::to_string(key).unwrap_or_default())
    }
}

/// Its `type`, as a list (3.1 allows several, `null` among them).
fn types(map: &Map<String, Value>) -> Option<Vec<String>> {
    match map.get("type")? {
        Value::String(t) => Some(vec![t.clone()]),
        Value::Array(ts) => Some(ts.iter().filter_map(Value::as_str).map(String::from).collect()),
        _ => None,
    }
}

fn nullable(map: &Map<String, Value>) -> bool {
    map.get("nullable") == Some(&Value::Bool(true))
        || map.get("x-nullable") == Some(&Value::Bool(true))
        || types(map).is_some_and(|t| t.iter().any(|t| t == "null"))
        || map.get("enum").and_then(Value::as_array).is_some_and(|e| e.contains(&Value::Null))
}

/// Whether it says what kind of value goes here, so null isn't one.
fn has_shape(map: &Map<String, Value>) -> bool {
    map.contains_key("properties") || map.contains_key("items")
}

fn shape_word(map: &Map<String, Value>) -> String {
    match types(map) {
        Some(t) => t.join(" or "),
        None if map.contains_key("properties") => "object".into(),
        None if map.contains_key("items") => "array".into(),
        None => "anything".into(),
    }
}

fn type_fits(ty: &str, v: &Value) -> bool {
    match ty {
        "object" => v.is_object(),
        "array" => v.is_array(),
        "string" => v.is_string(),
        "boolean" => v.is_boolean(),
        "null" => v.is_null(),
        "number" => v.is_number(),
        "integer" => v.is_i64() || v.is_u64() || v.as_f64().is_some_and(|f| f.fract() == 0.0),
        // `file` (Swagger), or something the spec made up: let it through.
        _ => true,
    }
}

/// What a value is, in words: `a string ("x")`, `an array of 3`.
pub fn describe(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => format!("a boolean ({b})"),
        Value::Number(n) if n.is_f64() && n.as_f64().is_some_and(|f| f.fract() != 0.0) => format!("a number ({n})"),
        Value::Number(n) => format!("an integer ({n})"),
        Value::String(s) => format!("a string ({})", compact(&Value::String(cut(s, 40)))),
        Value::Array(a) => format!("an array of {}", a.len()),
        Value::Object(_) => "an object".into(),
    }
}

fn cut(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn compact(v: &Value) -> String {
    let text = v.to_string();
    if text.len() > 60 {
        format!("{}…", &text[..text.char_indices().take_while(|(i, _)| *i < 60).last().map_or(0, |(i, c)| i + c.len_utf8())])
    } else {
        text
    }
}

/// JSON equality, with 1 and 1.0 the same number.
fn same(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn string(map: &Map<String, Value>, text: &str, at: &str, out: &mut Checked) {
    let n = text.chars().count() as u64;
    if let Some(min) = map.get("minLength").and_then(Value::as_u64) {
        if n < min {
            problem(out, at, format!("is {n} characters, shorter than the spec's minimum of {min}"));
        }
    }
    if let Some(max) = map.get("maxLength").and_then(Value::as_u64) {
        if n > max {
            problem(out, at, format!("is {n} characters, longer than the spec's maximum of {max}"));
        }
    }
    if let Some(pattern) = map.get("pattern").and_then(Value::as_str) {
        match regex_lite::Regex::new(pattern) {
            Ok(re) if !re.is_match(text) => problem(out, at, format!("is {}, which doesn't match the pattern {pattern}", compact(&Value::String(cut(text, 40))))),
            _ => {}
        }
    }
    if let Some(format) = map.get("format").and_then(Value::as_str) {
        let ok = match format {
            "date-time" => is_date_time(text),
            "date" => is_date(text),
            "uuid" => is_uuid(text),
            "email" => is_email(text),
            "uri" | "url" => text.contains("://") || text.starts_with("urn:") || text.starts_with("mailto:"),
            "ipv4" => text.split('.').count() == 4 && text.split('.').all(|p| p.parse::<u8>().is_ok()),
            _ => true,
        };
        if !ok {
            problem(out, at, format!("is {}, which isn't a {format}", compact(&Value::String(cut(text, 40)))));
        }
    }
}

fn number(map: &Map<String, Value>, n: f64, at: &str, out: &mut Checked) {
    let flag = |word: &str| map.get(word) == Some(&Value::Bool(true));
    if let Some(min) = map.get("minimum").and_then(Value::as_f64) {
        if n < min || (flag("exclusiveMinimum") && n == min) {
            problem(out, at, format!("is {n}, below the spec's minimum of {min}"));
        }
    }
    if let Some(min) = map.get("exclusiveMinimum").and_then(Value::as_f64) {
        if n <= min {
            problem(out, at, format!("is {n}, and the spec says more than {min}"));
        }
    }
    if let Some(max) = map.get("maximum").and_then(Value::as_f64) {
        if n > max || (flag("exclusiveMaximum") && n == max) {
            problem(out, at, format!("is {n}, above the spec's maximum of {max}"));
        }
    }
    if let Some(max) = map.get("exclusiveMaximum").and_then(Value::as_f64) {
        if n >= max {
            problem(out, at, format!("is {n}, and the spec says less than {max}"));
        }
    }
    if let Some(step) = map.get("multipleOf").and_then(Value::as_f64).filter(|s| *s > 0.0) {
        let q = n / step;
        if (q - q.round()).abs() > 1e-9 {
            problem(out, at, format!("is {n}, not a multiple of {step}"));
        }
    }
}

fn digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_date(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 3 && p[0].len() == 4 && p[1].len() == 2 && p[2].len() == 2 && p.iter().all(|x| digits(x))
        && (1..=12).contains(&p[1].parse::<u8>().unwrap_or(0))
        && (1..=31).contains(&p[2].parse::<u8>().unwrap_or(0))
}

fn is_date_time(s: &str) -> bool {
    let Some((date, time)) = s.split_once(['T', 't', ' ']) else { return false };
    if !is_date(date) || time.len() < 8 {
        return false;
    }
    let (hms, zone) = time.split_at(8);
    let hms_ok = hms.split(':').count() == 3 && hms.split(':').all(|x| x.len() == 2 && digits(x));
    let zone = zone.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    let zone_ok = zone.eq_ignore_ascii_case("z") || ((zone.starts_with('+') || zone.starts_with('-')) && zone.len() == 6 && &zone[3..4] == ":");
    hms_ok && zone_ok
}

fn is_uuid(s: &str) -> bool {
    let p: Vec<&str> = s.split('-').collect();
    p.len() == 5 && p.iter().zip([8, 4, 4, 4, 12]).all(|(x, n)| x.len() == n && x.bytes().all(|b| b.is_ascii_hexdigit()))
}

fn is_email(s: &str) -> bool {
    match s.split_once('@') {
        Some((user, host)) => !user.is_empty() && host.contains('.') && !host.starts_with('.') && !host.ends_with('.') && !s.contains(' '),
        None => false,
    }
}

fn sample_string(map: &Map<String, Value>) -> String {
    let format = map.get("format").and_then(Value::as_str).unwrap_or("");
    let s = match format {
        "date-time" => "2026-01-01T12:00:00Z".to_string(),
        "date" => "2026-01-01".to_string(),
        "uuid" => "3fa85f64-5717-4562-b3fc-2c963f66afa6".to_string(),
        "email" => "user@example.com".to_string(),
        "uri" | "url" => "https://example.com".to_string(),
        "ipv4" => "192.0.2.1".to_string(),
        "byte" => "dHJhY2Vy".to_string(),
        "password" => "secret".to_string(),
        _ => "string".to_string(),
    };
    match map.get("minLength").and_then(Value::as_u64) {
        Some(n) if (s.len() as u64) < n => format!("{s}{}", "x".repeat((n as usize) - s.len())),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "components": { "schemas": {
                "Pet": {
                    "type": "object",
                    "required": ["id", "name"],
                    "properties": {
                        "id": { "type": "integer", "format": "int64", "minimum": 1 },
                        "name": { "type": "string", "minLength": 1 },
                        "tag": { "type": "string", "nullable": true },
                        "status": { "type": "string", "enum": ["available", "sold"] },
                        "born": { "type": "string", "format": "date" },
                        "owner": { "$ref": "#/components/schemas/Owner" }
                    }
                },
                "Owner": { "type": "object", "properties": { "email": { "type": "string", "format": "email" } }, "additionalProperties": false },
                "Pets": { "type": "array", "items": { "$ref": "#/components/schemas/Pet" }, "maxItems": 3 },
                "Shape": { "oneOf": [
                    { "type": "object", "required": ["r"], "properties": { "r": { "type": "number" } }, "additionalProperties": false },
                    { "type": "object", "required": ["w"], "properties": { "w": { "type": "number" } }, "additionalProperties": false }
                ] },
                "Named": { "allOf": [ { "$ref": "#/components/schemas/Owner2" }, { "type": "object", "properties": { "nick": { "type": "string" } } } ] },
                "Owner2": { "type": "object", "properties": { "email": { "type": "string" } } },
                "Tree": { "type": "object", "properties": { "kids": { "type": "array", "items": { "$ref": "#/components/schemas/Tree" } } } }
            } }
        })
    }

    fn check(name: &str, v: Value) -> Checked {
        let d = doc();
        let s = Schemas { root: &d };
        let schema = json!({ "$ref": format!("#/components/schemas/{name}") });
        let c = s.check(&schema, &v);
        Checked { problems: c.problems, undocumented: c.undocumented }
    }

    fn whats(c: &Checked) -> Vec<String> {
        c.problems.iter().map(|p| format!("{} {}", p.at, p.what)).collect()
    }

    #[test]
    fn a_good_pet_passes() {
        let c = check("Pet", json!({ "id": 7, "name": "Rex", "tag": null, "status": "sold", "born": "2020-02-29", "owner": { "email": "a@b.co" } }));
        assert!(c.problems.is_empty(), "{:?}", whats(&c));
        assert!(c.undocumented.is_empty());
    }

    #[test]
    fn wrong_types_missing_and_enum_are_named() {
        let c = check("Pet", json!({ "id": "7", "status": "lost", "born": "2020-13-01", "owner": { "email": "nope", "x": 1 } }));
        let w = whats(&c);
        assert!(w.contains(&"$.id is a string (\"7\"), and the spec says integer".to_string()), "{w:?}");
        assert!(w.contains(&"$.name is missing, and the spec requires it".to_string()), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("$.status is \"lost\", not one of")), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("$.born is \"2020-13-01\", which isn't a date")), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("$.owner.email is \"nope\", which isn't a email")), "{w:?}");
        assert!(w.contains(&"$.owner.x is here, and the spec allows no fields beyond those it names".to_string()), "{w:?}");
    }

    #[test]
    fn undocumented_fields_are_drift_not_failure() {
        let c = check("Pets", json!([{ "id": 1, "name": "a", "etag": "x" }]));
        assert!(c.problems.is_empty(), "{:?}", whats(&c));
        assert_eq!(c.undocumented, vec!["$[0].etag".to_string()]);
        // Through allOf, a field any branch names is documented.
        let c = check("Named", json!({ "email": "e", "nick": "n", "other": 1 }));
        assert!(c.problems.is_empty());
        assert_eq!(c.undocumented, vec!["$.other".to_string()]);
    }

    #[test]
    fn arrays_and_one_of() {
        let c = check("Pets", json!([{ "id": 1, "name": "a" }, { "id": 2, "name": "b" }, { "id": 3, "name": "c" }, { "id": 0, "name": "" }]));
        let w = whats(&c);
        assert!(w.iter().any(|x| x == "$ has 4 items, more than the spec's maximum of 3"), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("$[3].id is 0, below")), "{w:?}");
        assert!(w.iter().any(|x| x.starts_with("$[3].name is 0 characters")), "{w:?}");
        assert!(check("Shape", json!({ "r": 1 })).problems.is_empty());
        assert!(whats(&check("Shape", json!({ "r": 1, "w": 2 }))).iter().any(|x| x.starts_with("$ matches none of oneOf")));
    }

    #[test]
    fn examples_are_made_from_schemas_and_pass_them() {
        let d = doc();
        let s = Schemas { root: &d };
        for name in ["Pet", "Pets", "Shape", "Named", "Tree"] {
            let schema = json!({ "$ref": format!("#/components/schemas/{name}") });
            let ex = s.example(&schema);
            let c = s.check(&schema, &ex);
            assert!(c.problems.is_empty(), "{name}: {ex} {:?}", whats(&c));
        }
        let pet = s.example(&json!({ "$ref": "#/components/schemas/Pet" }));
        assert_eq!(pet["status"], json!("available"));
        assert_eq!(pet["born"], json!("2026-01-01"));
    }

    #[test]
    fn formats() {
        assert!(is_date_time("2026-11-05T10:20:30Z"));
        assert!(is_date_time("2026-11-05T10:20:30.123+03:00"));
        assert!(!is_date_time("2026-11-05"));
        assert!(is_uuid("3fa85f64-5717-4562-b3fc-2c963f66afa6"));
        assert!(!is_uuid("3fa85f64"));
        assert_eq!(join("$", "odd key"), "$[\"odd key\"]");
    }
}
