//! SCIM queries (RFC 7644 §3.4.2, §3.5.2, §3.9): attribute paths, the filter grammar with its
//! evaluation, PATCH paths, sorting and attribute projection. Rust applies these to the
//! resources the handler supplies, so the handler only has to own the data.
use super::schema::{self, ResourceType};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;

pub const MAX_FILTER_BYTES: usize = 4096;
const MAX_DEPTH: usize = 16;
const MAX_NODES: usize = 128;

/// `[URI ":"] ATTRNAME ["." subAttr]`.
#[derive(Debug, Clone, PartialEq)]
pub struct AttrPath {
    pub urn: Option<String>,
    pub attr: String,
    pub sub: Option<String>,
}

fn name_ok(s: &str) -> bool {
    let mut c = s.chars();
    c.next()
        .is_some_and(|f| f.is_ascii_alphabetic() || f == '$')
        && c.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '$')
        && s.len() <= 128
}

impl AttrPath {
    pub fn parse(s: &str) -> Result<Self> {
        ensure!(
            !s.is_empty() && s.len() <= 512,
            "empty or overlong attribute path"
        );
        // A path that is itself a schema URN names that whole extension (RFC 7644 §3.5.2).
        if s.contains(':') && schema::schema(s).is_some() {
            return Ok(Self {
                urn: Some(s.to_owned()),
                attr: String::new(),
                sub: None,
            });
        }
        let (urn, rest) = match s.rfind(':') {
            Some(i) => (Some(s[..i].to_owned()), &s[i + 1..]),
            None => (None, s),
        };
        if let Some(u) = &urn {
            ensure!(
                u.to_ascii_lowercase().starts_with("urn:"),
                "attribute path {s:?} has a prefix that is not a URN"
            );
        }
        let (attr, sub) = match rest.split_once('.') {
            Some((a, b)) => (a, Some(b)),
            None => (rest, None),
        };
        ensure!(name_ok(attr), "invalid attribute name in {s:?}");
        if let Some(b) = sub {
            ensure!(name_ok(b), "invalid sub-attribute name in {s:?}");
        }
        Ok(Self {
            urn,
            attr: attr.to_owned(),
            sub: sub.map(str::to_owned),
        })
    }

    pub fn to_json(&self) -> Value {
        let attr = (!self.attr.is_empty()).then_some(&self.attr);
        json!({"schema": self.urn, "attribute": attr, "sub_attribute": self.sub})
    }

    fn definition(&self, rt: &ResourceType) -> Option<&'static Value> {
        schema::attribute(rt, self.urn.as_deref(), &self.attr, self.sub.as_deref())
    }

    /// The object this path's attribute lives in: the resource, or its extension object.
    fn container<'a>(&self, rt: &ResourceType, resource: &'a Value) -> Option<&'a Value> {
        match &self.urn {
            Some(u) if !u.eq_ignore_ascii_case(rt.schema) => schema::member(resource, u),
            _ => Some(resource),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Compare {
        path: AttrPath,
        op: String,
        value: Value,
    },
    Present(AttrPath),
    And(Box<Filter>, Box<Filter>),
    Or(Box<Filter>, Box<Filter>),
    Not(Box<Filter>),
    /// `attr[filter]`: some element of the multi-valued `attr` matches `filter`, whose paths
    /// name sub-attributes.
    ValuePath {
        path: AttrPath,
        filter: Box<Filter>,
    },
}

const COMPARE_OPS: &[&str] = &["eq", "ne", "co", "sw", "ew", "gt", "ge", "lt", "le"];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Str(String),
    Open,
    Close,
    LBracket,
    RBracket,
}

fn tokenize(s: &str) -> Result<Vec<Tok>> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b' ' | b'\t' | b'\n' | b'\r' => i += 1,
            b'(' => {
                out.push(Tok::Open);
                i += 1
            }
            b')' => {
                out.push(Tok::Close);
                i += 1
            }
            b'[' => {
                out.push(Tok::LBracket);
                i += 1
            }
            b']' => {
                out.push(Tok::RBracket);
                i += 1
            }
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    if b[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                ensure!(i < b.len(), "unterminated string in filter");
                let lit: String = serde_json::from_str(&s[start..=i])
                    .context("invalid string literal in filter")?;
                out.push(Tok::Str(lit));
                i += 1;
            }
            _ => {
                let start = i;
                while i < b.len() && !b" \t\n\r()[]\"".contains(&b[i]) {
                    i += 1;
                }
                out.push(Tok::Word(s[start..i].to_owned()));
            }
        }
        ensure!(out.len() <= 4 * MAX_NODES, "filter has too many tokens");
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
    nodes: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn keyword(&self, k: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k))
    }
    fn node(&mut self) -> Result<()> {
        self.nodes += 1;
        ensure!(self.nodes <= MAX_NODES, "filter has too many expressions");
        Ok(())
    }
    fn or(&mut self, depth: usize, in_value_path: bool) -> Result<Filter> {
        ensure!(depth <= MAX_DEPTH, "filter nests too deeply");
        let mut left = self.and(depth, in_value_path)?;
        while self.keyword("or") {
            self.pos += 1;
            self.node()?;
            let right = self.and(depth, in_value_path)?;
            left = Filter::Or(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn and(&mut self, depth: usize, in_value_path: bool) -> Result<Filter> {
        let mut left = self.unary(depth, in_value_path)?;
        while self.keyword("and") {
            self.pos += 1;
            self.node()?;
            let right = self.unary(depth, in_value_path)?;
            left = Filter::And(Box::new(left), Box::new(right));
        }
        Ok(left)
    }
    fn unary(&mut self, depth: usize, in_value_path: bool) -> Result<Filter> {
        self.node()?;
        if self.keyword("not") {
            self.pos += 1;
            ensure!(self.peek() == Some(&Tok::Open), "not must be followed by (");
            self.pos += 1;
            let inner = self.or(depth + 1, in_value_path)?;
            ensure!(self.peek() == Some(&Tok::Close), "missing ) after not(");
            self.pos += 1;
            return Ok(Filter::Not(Box::new(inner)));
        }
        if self.peek() == Some(&Tok::Open) {
            self.pos += 1;
            let inner = self.or(depth + 1, in_value_path)?;
            ensure!(self.peek() == Some(&Tok::Close), "missing )");
            self.pos += 1;
            return Ok(inner);
        }
        let Some(Tok::Word(w)) = self.peek().cloned() else {
            bail!("expected an attribute path");
        };
        self.pos += 1;
        let path = AttrPath::parse(&w)?;
        if self.peek() == Some(&Tok::LBracket) {
            ensure!(!in_value_path, "a value filter cannot contain another");
            ensure!(
                path.sub.is_none(),
                "a value path names an attribute, not a sub-attribute"
            );
            self.pos += 1;
            let inner = self.or(depth + 1, true)?;
            ensure!(self.peek() == Some(&Tok::RBracket), "missing ]");
            self.pos += 1;
            return Ok(Filter::ValuePath {
                path,
                filter: Box::new(inner),
            });
        }
        let Some(Tok::Word(op)) = self.peek().cloned() else {
            bail!("expected an operator after {w}");
        };
        let op = op.to_ascii_lowercase();
        self.pos += 1;
        if op == "pr" {
            return Ok(Filter::Present(path));
        }
        ensure!(COMPARE_OPS.contains(&op.as_str()), "unknown operator {op}");
        let value = match self.peek().cloned() {
            Some(Tok::Str(s)) => Value::String(s),
            Some(Tok::Word(v)) => match v.as_str() {
                "true" => Value::Bool(true),
                "false" => Value::Bool(false),
                "null" => Value::Null,
                n => serde_json::from_str::<serde_json::Number>(n)
                    .map(Value::Number)
                    .map_err(|_| anyhow::anyhow!("{n} is not a valid comparison value"))?,
            },
            _ => bail!("expected a comparison value"),
        };
        self.pos += 1;
        Ok(Filter::Compare { path, op, value })
    }
}

pub fn parse_filter(s: &str) -> Result<Filter> {
    ensure!(
        s.len() <= MAX_FILTER_BYTES,
        "filter over {MAX_FILTER_BYTES} bytes"
    );
    let mut p = Parser {
        toks: tokenize(s)?,
        pos: 0,
        nodes: 0,
    };
    ensure!(!p.toks.is_empty(), "empty filter");
    let f = p.or(0, false)?;
    ensure!(p.pos == p.toks.len(), "unexpected text after the filter");
    Ok(f)
}

impl Filter {
    pub fn to_json(&self) -> Value {
        match self {
            Filter::Compare { path, op, value } => {
                json!({"op": op, "path": path.to_json(), "value": value})
            }
            Filter::Present(path) => json!({"op": "pr", "path": path.to_json()}),
            Filter::And(a, b) => json!({"and": [a.to_json(), b.to_json()]}),
            Filter::Or(a, b) => json!({"or": [a.to_json(), b.to_json()]}),
            Filter::Not(f) => json!({"not": f.to_json()}),
            Filter::ValuePath { path, filter } => {
                json!({"value_path": path.to_json(), "filter": filter.to_json()})
            }
        }
    }
}

/// Where paths resolve: a resource of a type, or an element of a multi-valued complex
/// attribute (inside `attr[...]`), whose sub-attribute definitions are `parent`.
#[derive(Clone, Copy)]
enum Scope<'a> {
    Resource(&'a ResourceType),
    Element(&'a ResourceType, &'a AttrPath),
}

fn values<'a>(scope: Scope, path: &AttrPath, obj: &'a Value) -> (Vec<&'a Value>, bool) {
    let (base, def, inner_sub) = match scope {
        Scope::Resource(rt) => (
            path.container(rt, obj),
            path.definition(rt),
            path.sub.as_deref(),
        ),
        Scope::Element(rt, parent) => (
            Some(obj),
            schema::attribute(rt, parent.urn.as_deref(), &parent.attr, Some(&path.attr)),
            None,
        ),
    };
    let exact = schema::case_exact(def);
    let Some(base) = base else {
        return (vec![], exact);
    };
    if path.attr.is_empty() {
        return (vec![base], exact);
    }
    let Some(v) = schema::member(base, &path.attr) else {
        return (vec![], exact);
    };
    let mut out = Vec::new();
    let mut push = |x: &'a Value| match (inner_sub, x) {
        (Some(s), Value::Object(_)) => {
            if let Some(y) = schema::member(x, s) {
                out.push(y)
            }
        }
        (Some(_), _) => {}
        // A multi-valued complex attribute compared directly means its "value".
        (None, Value::Object(_)) => {
            if let Some(y) = schema::member(x, "value") {
                out.push(y)
            }
        }
        (None, _) => out.push(x),
    };
    match v {
        Value::Array(items) => items.iter().for_each(&mut push),
        other => push(other),
    }
    (out, exact)
}

fn text(v: &Value, exact: bool) -> Option<String> {
    v.as_str().map(|s| {
        if exact {
            s.to_owned()
        } else {
            s.to_lowercase()
        }
    })
}

fn compare(op: &str, actual: &Value, expected: &Value, exact: bool) -> bool {
    if let (Some(a), Some(e)) = (text(actual, exact), text(expected, exact)) {
        return match op {
            "eq" => a == e,
            "ne" => a != e,
            "co" => a.contains(&e),
            "sw" => a.starts_with(&e),
            "ew" => a.ends_with(&e),
            "gt" => a > e,
            "ge" => a >= e,
            "lt" => a < e,
            "le" => a <= e,
            _ => false,
        };
    }
    if let (Some(a), Some(e)) = (actual.as_f64(), expected.as_f64()) {
        return match op {
            "eq" => a == e,
            "ne" => a != e,
            "gt" => a > e,
            "ge" => a >= e,
            "lt" => a < e,
            "le" => a <= e,
            _ => false,
        };
    }
    match op {
        "eq" => actual == expected,
        "ne" => actual != expected,
        _ => false,
    }
}

fn present(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        _ => true,
    }
}

fn eval_in(f: &Filter, scope: Scope, obj: &Value) -> bool {
    match f {
        Filter::And(a, b) => eval_in(a, scope, obj) && eval_in(b, scope, obj),
        Filter::Or(a, b) => eval_in(a, scope, obj) || eval_in(b, scope, obj),
        Filter::Not(x) => !eval_in(x, scope, obj),
        Filter::Present(path) => values(scope, path, obj).0.into_iter().any(present),
        Filter::Compare { path, op, value } => {
            let (vals, exact) = values(scope, path, obj);
            if vals.is_empty() {
                return op == "ne" && !value.is_null() || (op == "eq" && value.is_null());
            }
            vals.into_iter().any(|v| compare(op, v, value, exact))
        }
        Filter::ValuePath { path, filter } => {
            let Scope::Resource(rt) = scope else {
                return false;
            };
            let Some(attr) = path
                .container(rt, obj)
                .and_then(|c| schema::member(c, &path.attr))
            else {
                return false;
            };
            let elements: Vec<&Value> = match attr {
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            elements
                .into_iter()
                .any(|e| eval_in(filter, Scope::Element(rt, path), e))
        }
    }
}

pub fn matches(f: &Filter, rt: &ResourceType, resource: &Value) -> bool {
    eval_in(f, Scope::Resource(rt), resource)
}

/// A PATCH path (RFC 7644 §3.5.2): an attribute path, or `attr[filter]` optionally followed
/// by `.sub`.
#[derive(Debug, Clone, PartialEq)]
pub struct PatchPath {
    pub path: AttrPath,
    pub filter: Option<Filter>,
    pub sub_after_filter: Option<String>,
}

impl PatchPath {
    pub fn parse(s: &str) -> Result<Self> {
        ensure!(
            s.len() <= MAX_FILTER_BYTES,
            "path over {MAX_FILTER_BYTES} bytes"
        );
        match s.find('[') {
            None => Ok(Self {
                path: AttrPath::parse(s)?,
                filter: None,
                sub_after_filter: None,
            }),
            Some(i) => {
                let close = s.rfind(']').context("missing ] in path")?;
                ensure!(close > i, "malformed value path");
                let path = AttrPath::parse(&s[..i])?;
                ensure!(
                    path.sub.is_none(),
                    "a value path names an attribute, not a sub-attribute"
                );
                let mut p = Parser {
                    toks: tokenize(&s[i + 1..close])?,
                    pos: 0,
                    nodes: 0,
                };
                let filter = p.or(0, true)?;
                ensure!(p.pos == p.toks.len(), "unexpected text in the value filter");
                let rest = &s[close + 1..];
                let sub = match rest.strip_prefix('.') {
                    Some(sub) => {
                        ensure!(name_ok(sub), "invalid sub-attribute after the value filter");
                        Some(sub.to_owned())
                    }
                    None => {
                        ensure!(rest.is_empty(), "unexpected text after the value filter");
                        None
                    }
                };
                Ok(Self {
                    path,
                    filter: Some(filter),
                    sub_after_filter: sub,
                })
            }
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"path": self.path.to_json(), "filter": self.filter.as_ref().map(Filter::to_json), "sub_attribute_after_filter": self.sub_after_filter})
    }
}

/// Sort by `path` (first value of a multi-valued attribute; absent values last).
pub fn sort(resources: &mut [Value], rt: &ResourceType, path: &AttrPath, descending: bool) {
    let key = |r: &Value| -> Option<Value> {
        let (vals, exact) = values(Scope::Resource(rt), path, r);
        vals.into_iter().next().map(|v| match text(v, exact) {
            Some(t) => Value::String(t),
            None => v.clone(),
        })
    };
    resources.sort_by(|a, b| {
        let (ka, kb) = (key(a), key(b));
        let ord = match (&ka, &kb) {
            (None, None) => Ordering::Equal,
            (None, _) => return Ordering::Greater,
            (_, None) => return Ordering::Less,
            (Some(x), Some(y)) => match (x, y) {
                (Value::String(p), Value::String(q)) => p.cmp(q),
                _ => x
                    .as_f64()
                    .partial_cmp(&y.as_f64())
                    .unwrap_or(Ordering::Equal),
            },
        };
        if descending {
            ord.reverse()
        } else {
            ord
        }
    });
}

/// RFC 7644 §3.9: keep `always` attributes, drop `never` ones, then honour `attributes`
/// (only these plus the always set) or `excludedAttributes`.
pub fn project(
    resource: &Value,
    rt: &ResourceType,
    attributes: &[AttrPath],
    excluded: &[AttrPath],
) -> Value {
    let Value::Object(src) = resource else {
        return resource.clone();
    };
    let mut out = src.clone();
    for (urn, name) in schema::names_returned(rt, "never") {
        remove(
            &mut out,
            rt,
            &AttrPath {
                urn: urn.map(str::to_owned),
                attr: name,
                sub: None,
            },
        );
    }
    let always: Vec<AttrPath> = schema::names_returned(rt, "always")
        .into_iter()
        .map(|(urn, name)| AttrPath {
            urn: urn.map(str::to_owned),
            attr: name,
            sub: None,
        })
        .collect();
    if !attributes.is_empty() {
        let mut kept = Map::new();
        for p in always.iter().chain(attributes) {
            copy(&out, &mut kept, rt, p);
        }
        return Value::Object(kept);
    }
    for p in excluded {
        if !always.iter().any(|a| {
            a.attr.eq_ignore_ascii_case(&p.attr)
                && p.sub.is_none()
                && a.urn.is_none() == p.urn.is_none()
        }) {
            remove(&mut out, rt, p);
        }
    }
    Value::Object(out)
}

fn is_extension(rt: &ResourceType, p: &AttrPath) -> Option<String> {
    p.urn
        .as_ref()
        .filter(|u| !u.eq_ignore_ascii_case(rt.schema))
        .cloned()
}

fn copy(src: &Map<String, Value>, dst: &mut Map<String, Value>, rt: &ResourceType, p: &AttrPath) {
    let src_v = Value::Object(src.clone());
    if let Some(ext) = is_extension(rt, p) {
        let Some(key) = schema::member_key(&src_v, &ext) else {
            return;
        };
        if p.attr.is_empty() {
            dst.insert(key.clone(), src[&key].clone());
            return;
        }
        let inner_src = src[&key].as_object().cloned().unwrap_or_default();
        let entry = dst.entry(key).or_insert_with(|| json!({}));
        if let Value::Object(inner_dst) = entry {
            copy(
                &inner_src,
                inner_dst,
                rt,
                &AttrPath {
                    urn: None,
                    attr: p.attr.clone(),
                    sub: p.sub.clone(),
                },
            );
        }
        return;
    }
    let Some(key) = schema::member_key(&src_v, &p.attr) else {
        return;
    };
    let value = &src[&key];
    match &p.sub {
        None => {
            dst.insert(key, value.clone());
        }
        Some(sub) => {
            let pick = |o: &Value| -> Option<Value> {
                let k = schema::member_key(o, sub)?;
                let mut m = Map::new();
                m.insert(k.clone(), o[&k].clone());
                Some(Value::Object(m))
            };
            let picked = match value {
                Value::Array(items) => Value::Array(items.iter().filter_map(pick).collect()),
                Value::Object(_) => pick(value).unwrap_or_else(|| json!({})),
                _ => return,
            };
            match (dst.get_mut(&key), picked) {
                (Some(Value::Object(existing)), Value::Object(more)) => existing.extend(more),
                (_, picked) => {
                    dst.insert(key, picked);
                }
            }
        }
    }
}

fn remove(obj: &mut Map<String, Value>, rt: &ResourceType, p: &AttrPath) {
    if let Some(ext) = is_extension(rt, p) {
        let key = schema::member_key(&Value::Object(obj.clone()), &ext);
        if p.attr.is_empty() {
            if let Some(k) = key {
                obj.remove(&k);
            }
            return;
        }
        if let Some(Value::Object(inner)) = key.and_then(|k| obj.get_mut(&k)) {
            remove(
                inner,
                rt,
                &AttrPath {
                    urn: None,
                    attr: p.attr.clone(),
                    sub: p.sub.clone(),
                },
            );
        }
        return;
    }
    let Some(key) = schema::member_key(&Value::Object(obj.clone()), &p.attr) else {
        return;
    };
    match &p.sub {
        None => {
            obj.remove(&key);
        }
        Some(sub) => {
            let strip = |o: &mut Value| {
                if let Some(k) = schema::member_key(o, sub) {
                    o.as_object_mut().map(|m| m.remove(&k));
                }
            };
            match obj.get_mut(&key) {
                Some(Value::Array(items)) => items.iter_mut().for_each(strip),
                Some(v @ Value::Object(_)) => strip(v),
                _ => {}
            }
        }
    }
}

pub fn parse_list(s: Option<&str>) -> Result<Vec<AttrPath>> {
    match s.filter(|s| !s.trim().is_empty()) {
        None => Ok(vec![]),
        Some(s) => {
            let parts: Vec<&str> = s.split(',').map(str::trim).collect();
            ensure!(parts.len() <= 64, "at most 64 attribute names");
            parts.into_iter().map(AttrPath::parse).collect()
        }
    }
}
