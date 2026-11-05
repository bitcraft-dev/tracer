//! A GraphQL query checked against a server's schema, as the editor wants
//! it: each problem at its line and column, with what would fit there.
//!
//! ```text
//! query { pet(id: 1) { nam } }
//!                      ^^^ `nam` isn't a field of Pet. Did you mean name?
//! ```
//!
//! What's checked: the document parses; the operation's root exists; each
//! field is one its type has (`__typename` on any); a leaf takes no
//! selection and an object needs one; arguments are the field's, required
//! ones given, enum literals among the enum's values; fragments exist and
//! their types do; variables used are declared, and those declared are used.

use std::collections::{BTreeMap, BTreeSet};

use tracer_protocol::{GqlField, GqlProblem, GqlSchema, GqlType};

// ---- reading ----

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Punct(char),
    Spread,
    Name(String),
    Number,
    Str,
    End,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

fn lex(text: &str) -> Result<Vec<Token>, (usize, usize, String)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b' ' | b'\t' | b'\n' | b'\r' | b',' => i += 1,
            0xEF if b[i..].starts_with(&[0xEF, 0xBB, 0xBF]) => i += 3,
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'.' => {
                if b[i..].starts_with(b"...") {
                    out.push(Token { tok: Tok::Spread, start: i, end: i + 3 });
                    i += 3;
                } else {
                    return Err((i, i + 1, "a lone `.`: a spread is `...`".into()));
                }
            }
            b'!' | b'$' | b'&' | b'(' | b')' | b':' | b'=' | b'@' | b'[' | b']' | b'{' | b'|' | b'}' => {
                out.push(Token { tok: Tok::Punct(c as char), start: i, end: i + 1 });
                i += 1;
            }
            b'"' => {
                let start = i;
                if b[i..].starts_with(b"\"\"\"") {
                    i += 3;
                    loop {
                        if i >= b.len() {
                            return Err((start, b.len(), "a block string that never ends: close it with \"\"\"".into()));
                        }
                        if b[i..].starts_with(b"\\\"\"\"") {
                            i += 4;
                        } else if b[i..].starts_with(b"\"\"\"") {
                            i += 3;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    i += 1;
                    loop {
                        if i >= b.len() || b[i] == b'\n' {
                            return Err((start, i, "a string that never ends: close it with \" on the same line".into()));
                        }
                        if b[i] == b'\\' {
                            i += 2;
                        } else if b[i] == b'"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                }
                out.push(Token { tok: Tok::Str, start, end: i });
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_digit() || matches!(b[i], b'.' | b'e' | b'E' | b'+' | b'-')) {
                    i += 1;
                }
                out.push(Token { tok: Tok::Number, start, end: i });
            }
            c if c == b'_' || c.is_ascii_alphabetic() => {
                let start = i;
                while i < b.len() && (b[i] == b'_' || b[i].is_ascii_alphanumeric()) {
                    i += 1;
                }
                out.push(Token { tok: Tok::Name(text[start..i].to_string()), start, end: i });
            }
            _ => {
                let ch = text[i..].chars().next().unwrap_or('?');
                return Err((i, i + ch.len_utf8(), format!("`{ch}` isn't part of GraphQL")));
            }
        }
    }
    out.push(Token { tok: Tok::End, start: b.len(), end: b.len() });
    Ok(out)
}

/// Where something is: byte offsets in the query.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct At {
    start: usize,
    end: usize,
}

#[derive(Debug)]
enum Value {
    Var(String, At),
    Enum(String, At),
    List(Vec<Value>),
    Object(Vec<(String, Value)>),
    Other,
}

#[derive(Debug)]
struct Arg {
    name: String,
    at: At,
    value: Value,
}

#[derive(Debug)]
enum Sel {
    Field { name: String, at: At, args: Vec<Arg>, selection: Option<(Vec<Sel>, At)> },
    Spread { name: String, at: At },
    Inline { on: Option<(String, At)>, selection: Vec<Sel> },
}

#[derive(Debug)]
struct VarDef {
    name: String,
    at: At,
    ty: String,
    ty_at: At,
}

#[derive(Debug)]
enum Def {
    Op { kind: String, at: At, vars: Vec<VarDef>, selection: Vec<Sel> },
    Frag { name: String, at: At, on: (String, At), selection: Vec<Sel> },
}

struct Parser<'a> {
    text: &'a str,
    toks: Vec<Token>,
    i: usize,
}

type Fail = (usize, usize, String);

impl<'a> Parser<'a> {
    fn peek(&self) -> &Token {
        &self.toks[self.i]
    }

    fn next(&mut self) -> Token {
        let t = self.toks[self.i].clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn shown(&self, t: &Token) -> String {
        match t.tok {
            Tok::End => "the end".into(),
            _ => format!("`{}`", &self.text[t.start..t.end]),
        }
    }

    fn fail<T>(&self, wanted: &str) -> Result<T, Fail> {
        let t = self.peek();
        Err((t.start, t.end.max(t.start + 1).min(self.text.len().max(t.start)), format!("expected {wanted}, found {}", self.shown(t))))
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek().tok == Tok::Punct(c) {
            self.next();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> Result<Token, Fail> {
        if self.peek().tok == Tok::Punct(c) {
            Ok(self.next())
        } else {
            self.fail(&format!("`{c}`"))
        }
    }

    fn name(&mut self) -> Result<(String, At), Fail> {
        match self.peek().tok.clone() {
            Tok::Name(n) => {
                let t = self.next();
                Ok((n, At { start: t.start, end: t.end }))
            }
            _ => self.fail("a name"),
        }
    }

    fn document(&mut self) -> Result<Vec<Def>, Fail> {
        let mut defs = Vec::new();
        while self.peek().tok != Tok::End {
            defs.push(self.definition()?);
        }
        if defs.is_empty() {
            return Err((0, 0, "the query is empty".into()));
        }
        Ok(defs)
    }

    fn definition(&mut self) -> Result<Def, Fail> {
        let t = self.peek().clone();
        match &t.tok {
            Tok::Punct('{') => {
                let selection = self.selection_set()?.0;
                Ok(Def::Op { kind: "query".into(), at: At { start: t.start, end: t.end }, vars: vec![], selection })
            }
            Tok::Name(w) if w == "query" || w == "mutation" || w == "subscription" => {
                let kind = w.clone();
                self.next();
                if matches!(self.peek().tok, Tok::Name(_)) {
                    self.next();
                }
                let mut vars = Vec::new();
                if self.eat('(') {
                    while !self.eat(')') {
                        self.expect('$')?;
                        let (name, at) = self.name()?;
                        self.expect(':')?;
                        let ty_start = self.peek().start;
                        let ty = self.type_ref()?;
                        let ty_at = At { start: ty_start, end: self.toks[self.i - 1].end };
                        if self.eat('=') {
                            self.value()?;
                        }
                        self.directives()?;
                        vars.push(VarDef { name, at, ty, ty_at });
                    }
                }
                self.directives()?;
                let selection = self.selection_set()?.0;
                Ok(Def::Op { kind, at: At { start: t.start, end: t.end }, vars, selection })
            }
            Tok::Name(w) if w == "fragment" => {
                self.next();
                let (name, at) = self.name()?;
                match self.peek().tok.clone() {
                    Tok::Name(on) if on == "on" => {
                        self.next();
                    }
                    _ => return self.fail("`on` and a type"),
                }
                let on = self.name()?;
                self.directives()?;
                let selection = self.selection_set()?.0;
                Ok(Def::Frag { name, at, on, selection })
            }
            _ => self.fail("`query`, `mutation`, `subscription`, `fragment` or `{`"),
        }
    }

    fn type_ref(&mut self) -> Result<String, Fail> {
        let mut out = if self.eat('[') {
            let inner = self.type_ref()?;
            self.expect(']')?;
            format!("[{inner}]")
        } else {
            self.name()?.0
        };
        if self.eat('!') {
            out.push('!');
        }
        Ok(out)
    }

    fn directives(&mut self) -> Result<(), Fail> {
        while self.eat('@') {
            self.name()?;
            if self.peek().tok == Tok::Punct('(') {
                self.arguments()?;
            }
        }
        Ok(())
    }

    fn selection_set(&mut self) -> Result<(Vec<Sel>, At), Fail> {
        let open = self.expect('{')?;
        let mut out = Vec::new();
        loop {
            if self.peek().tok == Tok::Punct('}') {
                let close = self.next();
                if out.is_empty() {
                    return Err((open.start, close.end, "an empty selection: name at least one field inside { }".into()));
                }
                return Ok((out, At { start: open.start, end: close.end }));
            }
            out.push(self.selection()?);
        }
    }

    fn selection(&mut self) -> Result<Sel, Fail> {
        if self.peek().tok == Tok::Spread {
            self.next();
            match self.peek().tok.clone() {
                Tok::Name(w) if w == "on" => {
                    self.next();
                    let on = self.name()?;
                    self.directives()?;
                    let selection = self.selection_set()?.0;
                    Ok(Sel::Inline { on: Some(on), selection })
                }
                Tok::Name(_) => {
                    let (name, at) = self.name()?;
                    self.directives()?;
                    Ok(Sel::Spread { name, at })
                }
                Tok::Punct('{') | Tok::Punct('@') => {
                    self.directives()?;
                    let selection = self.selection_set()?.0;
                    Ok(Sel::Inline { on: None, selection })
                }
                _ => self.fail("a fragment's name, `on` or `{`"),
            }
        } else {
            let (mut name, mut at) = self.name()?;
            if self.eat(':') {
                (name, at) = self.name()?;
            }
            let args = if self.peek().tok == Tok::Punct('(') { self.arguments()? } else { vec![] };
            self.directives()?;
            let selection = if self.peek().tok == Tok::Punct('{') { Some(self.selection_set()?) } else { None };
            Ok(Sel::Field { name, at, args, selection })
        }
    }

    fn arguments(&mut self) -> Result<Vec<Arg>, Fail> {
        self.expect('(')?;
        let mut out = Vec::new();
        while !self.eat(')') {
            let (name, at) = self.name()?;
            self.expect(':')?;
            let value = self.value()?;
            out.push(Arg { name, at, value });
        }
        Ok(out)
    }

    fn value(&mut self) -> Result<Value, Fail> {
        let t = self.peek().clone();
        match &t.tok {
            Tok::Punct('$') => {
                self.next();
                let (name, at) = self.name()?;
                Ok(Value::Var(name, At { start: t.start, end: at.end }))
            }
            Tok::Number | Tok::Str => {
                self.next();
                Ok(Value::Other)
            }
            Tok::Name(n) if n == "true" || n == "false" || n == "null" => {
                self.next();
                Ok(Value::Other)
            }
            Tok::Name(n) => {
                let n = n.clone();
                self.next();
                Ok(Value::Enum(n, At { start: t.start, end: t.end }))
            }
            Tok::Punct('[') => {
                self.next();
                let mut items = Vec::new();
                while !self.eat(']') {
                    items.push(self.value()?);
                }
                Ok(Value::List(items))
            }
            Tok::Punct('{') => {
                self.next();
                let mut fields = Vec::new();
                while !self.eat('}') {
                    let (name, _) = self.name()?;
                    self.expect(':')?;
                    fields.push((name, self.value()?));
                }
                Ok(Value::Object(fields))
            }
            _ => self.fail("a value"),
        }
    }
}

// ---- checking ----

/// An argument as the schema has it: `id: ID!`, maybe with a default.
struct ArgDef {
    name: String,
    ty: String,
    default: bool,
}

/// `id: ID!, first: Int = 10` as its arguments. A default's own commas
/// (`[1, 2]`) are inside brackets and don't split it.
fn arg_defs(args: &str) -> Vec<ArgDef> {
    let mut parts = Vec::new();
    let (mut depth, mut quoted, mut start) = (0i32, false, 0);
    for (i, c) in args.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '[' | '{' | '(' if !quoted => depth += 1,
            ']' | '}' | ')' if !quoted => depth -= 1,
            ',' if !quoted && depth == 0 => {
                parts.push(&args[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&args[start..]);
    parts
        .into_iter()
        .filter_map(|p| {
            let (name, rest) = p.split_once(':')?;
            let (ty, default) = match rest.split_once('=') {
                Some((ty, _)) => (ty, true),
                None => (rest, false),
            };
            Some(ArgDef { name: name.trim().to_string(), ty: ty.trim().to_string(), default })
        })
        .collect()
}

/// `[User!]!` → `User`.
fn base(ty: &str) -> &str {
    ty.trim_matches(|c| c == '[' || c == ']' || c == '!')
}

struct Checker<'a> {
    text: &'a str,
    types: BTreeMap<&'a str, &'a GqlType>,
    frags: BTreeMap<String, &'a Def>,
    out: Vec<GqlProblem>,
}

impl<'a> Checker<'a> {
    fn problem(&mut self, at: At, severity: &str, message: String, suggestions: Vec<String>) {
        // Names offered for a name that isn't one replace it; anything else
        // is a hint.
        let replaces = message.contains("isn't a field of") || message.contains("has no argument") || message.contains("isn't a value of") || message.contains("no fragment `") || message.contains("has no type `");
        let (line, col) = line_col(self.text, at.start);
        if self.out.len() < 100 {
            self.out.push(GqlProblem { line, col, start: at.start as u64, end: at.end as u64, severity: severity.into(), message, suggestions, replaces });
        }
    }

    fn kind(&self, name: &str) -> &str {
        self.types.get(name).map_or("SCALAR", |t| t.kind.as_str())
    }

    fn is_leaf(&self, name: &str) -> bool {
        matches!(self.kind(name), "SCALAR" | "ENUM")
    }

    /// Check `selection` on type `parent`; the variables used, by name with
    /// where and the type expected, go in `used`; fragments spread, in
    /// `spread`.
    fn selection(&mut self, selection: &[Sel], parent: &str, used: &mut Vec<(String, At)>, spread: &mut BTreeSet<String>) {
        let Some(&ty) = self.types.get(parent) else { return };
        for sel in selection {
            match sel {
                Sel::Field { name, at, args, selection } => {
                    if name == "__typename" {
                        if let Some((_, sat)) = selection {
                            self.problem(*sat, "error", "`__typename` is a String: it takes no { … }".into(), vec![]);
                        }
                        continue;
                    }
                    if name.starts_with("__") {
                        continue;
                    }
                    let field: Option<&GqlField> = ty.fields.iter().find(|f| &f.name == name);
                    let Some(field) = field else {
                        if ty.kind == "UNION" {
                            let picks: Vec<String> = ty.values.iter().map(|v| format!("... on {v} {{ }}")).collect();
                            self.problem(*at, "error", format!("{parent} is a union of {}: pick fields inside `... on Type {{ }}`", ty.values.join(", ")), picks);
                        } else {
                            let names: Vec<&str> = ty.fields.iter().map(|f| f.name.as_str()).collect();
                            let (msg, sugg) = unknown(name, &names);
                            self.problem(*at, "error", format!("`{name}` isn't a field of {parent}.{msg}"), sugg);
                        }
                        for a in args {
                            collect_vars(&a.value, used);
                        }
                        continue;
                    };
                    let defs = arg_defs(&field.args);
                    for a in args {
                        collect_vars(&a.value, used);
                        match defs.iter().find(|d| d.name == a.name) {
                            None => {
                                let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
                                let (msg, sugg) = unknown(&a.name, &names);
                                let takes = if names.is_empty() { format!(" It takes none.") } else { msg };
                                self.problem(a.at, "error", format!("`{name}` has no argument `{}`.{takes}", a.name), sugg);
                            }
                            Some(d) => self.enum_value(&a.value, base(&d.ty)),
                        }
                    }
                    for d in defs.iter().filter(|d| d.ty.ends_with('!') && !d.default) {
                        if !args.iter().any(|a| a.name == d.name) {
                            self.problem(*at, "error", format!("`{name}` needs `{}: {}`", d.name, d.ty), vec![format!("{}: ", d.name)]);
                        }
                    }
                    let inner = base(&field.ty).to_string();
                    match (selection, self.is_leaf(&inner)) {
                        (Some((_, sat)), true) => {
                            self.problem(*sat, "error", format!("`{name}` is {}: it takes no {{ … }}", article(&field.ty)), vec![]);
                        }
                        (None, false) => {
                            let fields: Vec<String> = self.types.get(inner.as_str()).map(|t| t.fields.iter().take(12).map(|f| f.name.clone()).collect()).unwrap_or_default();
                            self.problem(*at, "error", format!("`{name}` is {}: pick its fields with {{ … }}", article(&field.ty)), fields);
                        }
                        (Some((sel, _)), false) => self.selection(sel, &inner, used, spread),
                        (None, true) => {}
                    }
                }
                Sel::Spread { name, at } => {
                    spread.insert(name.clone());
                    if !self.frags.contains_key(name) {
                        let names: Vec<&str> = self.frags.keys().map(String::as_str).collect();
                        let (msg, sugg) = unknown(name, &names);
                        self.problem(*at, "error", format!("there's no fragment `{name}`.{msg}"), sugg);
                    }
                }
                Sel::Inline { on, selection } => match on {
                    Some((t, at)) if !self.types.contains_key(t.as_str()) => {
                        let names: Vec<&str> = self.types.keys().copied().collect();
                        let (msg, sugg) = unknown(t, &names);
                        self.problem(*at, "error", format!("the schema has no type `{t}`.{msg}"), sugg);
                    }
                    Some((t, _)) => {
                        let t = t.clone();
                        self.selection(selection, &t, used, spread);
                    }
                    None => self.selection(selection, parent, used, spread),
                },
            }
        }
    }

    fn enum_value(&mut self, v: &Value, ty: &str) {
        let Some(&t) = self.types.get(ty) else { return };
        match v {
            Value::Enum(name, at) if t.kind == "ENUM" && !t.values.contains(name) => {
                let names: Vec<&str> = t.values.iter().map(String::as_str).collect();
                let (msg, sugg) = unknown(name, &names);
                self.problem(*at, "error", format!("`{name}` isn't a value of {ty}.{msg}"), sugg);
            }
            Value::Enum(name, at) if t.kind != "ENUM" && !matches!(t.kind.as_str(), "SCALAR") => {
                self.problem(*at, "error", format!("`{name}` isn't {}: write it as a value of that type", article(ty)), vec![]);
            }
            Value::List(items) => {
                for i in items {
                    self.enum_value(i, ty);
                }
            }
            Value::Object(fields) if t.kind == "INPUT_OBJECT" => {
                for (name, value) in fields {
                    if let Some(f) = t.fields.iter().find(|f| &f.name == name) {
                        let inner = base(&f.ty).to_string();
                        self.enum_value(value, &inner);
                    }
                }
            }
            _ => {}
        }
    }
}

fn collect_vars(v: &Value, used: &mut Vec<(String, At)>) {
    match v {
        Value::Var(n, at) => used.push((n.clone(), *at)),
        Value::List(items) => items.iter().for_each(|i| collect_vars(i, used)),
        Value::Object(fields) => fields.iter().for_each(|(_, v)| collect_vars(v, used)),
        _ => {}
    }
}

/// `an Int`, `a [Pet!]!`.
fn article(ty: &str) -> String {
    let first = base(ty).chars().next().unwrap_or('x').to_ascii_lowercase();
    if ty.starts_with('[') {
        format!("a list of {}", base(ty))
    } else if "aeiou".contains(first) {
        format!("an {}", base(ty))
    } else {
        format!("a {}", base(ty))
    }
}

/// " Did you mean x?" and what fits, nearest first.
fn unknown(name: &str, known: &[&str]) -> (String, Vec<String>) {
    let lower = name.to_lowercase();
    let mut ranked: Vec<(usize, &str)> = known.iter().map(|k| (distance(&lower, &k.to_lowercase()), *k)).collect();
    ranked.sort();
    let near = ranked.first().filter(|(d, _)| *d <= (name.len() / 3).max(1) || ranked.first().is_some_and(|(_, k)| k.to_lowercase().starts_with(&lower)));
    let msg = match near {
        Some((_, k)) => format!(" Did you mean {k}?"),
        None if known.is_empty() => String::new(),
        None => String::new(),
    };
    (msg, ranked.into_iter().take(12).map(|(_, k)| k.to_string()).collect())
}

fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut row = vec![i; b.len() + 1];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            row[j] = (prev[j] + 1).min(row[j - 1] + 1).min(prev[j - 1] + cost);
        }
        prev = row;
    }
    prev[b.len()]
}

/// 1-based line and column (in characters) of byte `at`.
fn line_col(text: &str, at: usize) -> (u32, u32) {
    let before = &text[..at.min(text.len())];
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().unwrap_or("").chars().count() as u32 + 1;
    (line, col)
}

/// `query`'s problems against `schema`, in the order they come in it.
pub fn check(query: &str, schema: &GqlSchema) -> Vec<GqlProblem> {
    let mut checker = Checker { text: query, types: schema.types.iter().map(|t| (t.name.as_str(), t)).collect(), frags: BTreeMap::new(), out: vec![] };
    let defs = match lex(query).and_then(|toks| Parser { text: query, toks, i: 0 }.document()) {
        Ok(d) => d,
        Err((start, end, message)) => {
            checker.problem(At { start, end }, "error", message, vec![]);
            return checker.out;
        }
    };
    for d in &defs {
        if let Def::Frag { name, .. } = d {
            checker.frags.insert(name.clone(), d);
        }
    }
    // Fragments: their type, and their own fields.
    let mut frag_use: BTreeMap<String, (Vec<(String, At)>, BTreeSet<String>)> = BTreeMap::new();
    for d in &defs {
        if let Def::Frag { name, on: (on, on_at), selection, .. } = d {
            let (mut used, mut spread) = (Vec::new(), BTreeSet::new());
            if checker.types.contains_key(on.as_str()) {
                checker.selection(selection, on, &mut used, &mut spread);
            } else {
                let names: Vec<&str> = checker.types.keys().copied().collect();
                let (msg, sugg) = unknown(on, &names);
                checker.problem(*on_at, "error", format!("the schema has no type `{on}`.{msg}"), sugg);
            }
            frag_use.insert(name.clone(), (used, spread));
        }
    }
    let mut spread_anywhere = BTreeSet::new();
    for d in &defs {
        let Def::Op { kind, at, vars, selection } = d else { continue };
        let root = match kind.as_str() {
            "mutation" => schema.mutation.as_str(),
            "subscription" => schema.subscription.as_str(),
            _ => schema.query.as_str(),
        };
        if root.is_empty() {
            checker.problem(*at, "error", format!("the server takes no {kind}s"), vec![]);
            continue;
        }
        for v in vars {
            let b = base(&v.ty);
            if !checker.types.contains_key(b) {
                let names: Vec<&str> = checker.types.iter().filter(|(_, t)| matches!(t.kind.as_str(), "SCALAR" | "ENUM" | "INPUT_OBJECT")).map(|(n, _)| *n).collect();
                let (msg, sugg) = unknown(b, &names);
                checker.problem(v.ty_at, "error", format!("the schema has no type `{b}`.{msg}"), sugg);
            } else if !matches!(checker.kind(b), "SCALAR" | "ENUM" | "INPUT_OBJECT") {
                checker.problem(v.ty_at, "error", format!("`{b}` is an output type: a variable takes a scalar, an enum or an input"), vec![]);
            }
        }
        let (mut used, mut spread) = (Vec::new(), BTreeSet::new());
        checker.selection(selection, root, &mut used, &mut spread);
        // Through fragments, and theirs.
        let mut seen = BTreeSet::new();
        let mut todo: Vec<String> = spread.iter().cloned().collect();
        while let Some(f) = todo.pop() {
            if !seen.insert(f.clone()) {
                continue;
            }
            if let Some((u, s)) = frag_use.get(&f) {
                used.extend(u.iter().cloned());
                todo.extend(s.iter().cloned());
            }
        }
        spread_anywhere.extend(seen);
        let declared: Vec<&str> = vars.iter().map(|v| v.name.as_str()).collect();
        for (name, at) in &used {
            if !declared.contains(&name.as_str()) {
                let decl = vec![format!("${name}: ")];
                checker.problem(*at, "error", format!("`${name}` isn't declared: add it to the operation's ( … )"), decl);
            }
        }
        for v in vars {
            if !used.iter().any(|(n, _)| n == &v.name) {
                checker.problem(v.at, "warning", format!("`${}` is declared and never used", v.name), vec![]);
            }
        }
    }
    let has_op = defs.iter().any(|d| matches!(d, Def::Op { .. }));
    for d in &defs {
        if let Def::Frag { name, at, .. } = d {
            if has_op && !spread_anywhere.contains(name) {
                checker.problem(*at, "warning", format!("fragment `{name}` is never spread"), vec![]);
            }
        }
    }
    checker.out.sort_by_key(|p| (p.start, p.end));
    checker.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, args: &str, ty: &str) -> GqlField {
        GqlField { name: name.into(), args: args.into(), ty: ty.into(), description: String::new() }
    }

    fn ty(name: &str, kind: &str, fields: Vec<GqlField>, values: &[&str]) -> GqlType {
        GqlType { name: name.into(), kind: kind.into(), description: String::new(), fields, values: values.iter().map(|v| v.to_string()).collect() }
    }

    fn schema() -> GqlSchema {
        GqlSchema {
            query: "Query".into(),
            mutation: String::new(),
            subscription: String::new(),
            types: vec![
                ty("Query", "OBJECT", vec![field("pet", "id: ID!", "Pet"), field("pets", "first: Int = 10, status: Status", "[Pet!]!"), field("search", "q: String!", "[Result]")], &[]),
                ty("Pet", "OBJECT", vec![field("id", "", "ID!"), field("name", "", "String"), field("owner", "", "Owner"), field("status", "", "Status")], &[]),
                ty("Owner", "OBJECT", vec![field("email", "", "String")], &[]),
                ty("Status", "ENUM", vec![], &["AVAILABLE", "SOLD"]),
                ty("Result", "UNION", vec![], &["Pet", "Owner"]),
                ty("ID", "SCALAR", vec![], &[]),
                ty("String", "SCALAR", vec![], &[]),
                ty("Int", "SCALAR", vec![], &[]),
            ],
        }
    }

    fn msgs(q: &str) -> Vec<String> {
        check(q, &schema()).into_iter().map(|p| format!("{}:{} {}", p.line, p.col, p.message)).collect()
    }

    #[test]
    fn a_good_query_has_no_problems() {
        let q = "query P($id: ID!) {\n  pet(id: $id) { id name owner { email } __typename }\n  pets(status: SOLD) { ...F }\n  search(q: \"x\") { ... on Pet { name } ... on Owner { email } }\n}\nfragment F on Pet { status }";
        assert_eq!(msgs(q), Vec::<String>::new());
    }

    #[test]
    fn unknown_fields_say_what_fits() {
        let p = check("{ pet(id: 1) { nam } }", &schema());
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].message, "`nam` isn't a field of Pet. Did you mean name?");
        assert_eq!((p[0].line, p[0].col, p[0].start, p[0].end), (1, 16, 15, 18));
        assert_eq!(p[0].suggestions[0], "name");
    }

    #[test]
    fn selections_arguments_and_enums() {
        let m = msgs("{\n  pet { id { x } owner }\n  pets(first: 1, colour: 2, status: LOST) { id }\n}");
        assert!(m.contains(&"2:3 `pet` needs `id: ID!`".to_string()), "{m:?}");
        assert!(m.contains(&"2:12 `id` is an ID: it takes no { … }".to_string()), "{m:?}");
        assert!(m.contains(&"2:18 `owner` is an Owner: pick its fields with { … }".to_string()), "{m:?}");
        assert!(m.iter().any(|x| x.starts_with("3:18 `pets` has no argument `colour`")), "{m:?}");
        assert!(m.iter().any(|x| x.starts_with("3:37 `LOST` isn't a value of Status")), "{m:?}");
    }

    #[test]
    fn variables_fragments_and_roots() {
        let m = msgs("query ($unused: Int, $bad: Pet) { pet(id: $missing) { ...Nope } }\nfragment Lone on Pett { id }");
        assert!(m.iter().any(|x| x.contains("`$unused` is declared and never used")), "{m:?}");
        assert!(m.iter().any(|x| x.contains("`Pet` is an output type")), "{m:?}");
        assert!(m.iter().any(|x| x.contains("`$missing` isn't declared")), "{m:?}");
        assert!(m.iter().any(|x| x.contains("there's no fragment `Nope`")), "{m:?}");
        assert!(m.iter().any(|x| x.contains("the schema has no type `Pett`. Did you mean Pet?")), "{m:?}");
        assert!(m.iter().any(|x| x.contains("fragment `Lone` is never spread")), "{m:?}");
        assert_eq!(msgs("mutation { x }"), vec!["1:1 the server takes no mutations".to_string()]);
        assert!(msgs("{ search(q: \"a\") { name } }")[0].contains("is a union of Pet, Owner"));
    }

    #[test]
    fn syntax_errors_point_at_the_token() {
        assert_eq!(msgs("{ pet(id: 1) { id }"), vec!["1:20 expected a name, found the end".to_string()]);
        assert_eq!(msgs("{ pet(id: \"x) { id } }"), vec!["1:11 a string that never ends: close it with \" on the same line".to_string()]);
        assert_eq!(msgs("{ }"), vec!["1:1 an empty selection: name at least one field inside { }".to_string()]);
        assert_eq!(msgs("# only a comment\n"), vec!["1:1 the query is empty".to_string()]);
        let d = arg_defs("ids: [ID!] = [1, 2], q: String!");
        assert_eq!((d.len(), d[0].default, d[1].ty.as_str()), (2, true, "String!"));
    }
}
