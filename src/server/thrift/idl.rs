//! A Thrift IDL subset, parsed at startup: namespaces (ignored), typedefs, consts (skipped),
//! enums, structs, unions, exceptions and services (with `extends`, `oneway`, `throws`).
//! `include` is refused: the IDL must be self-contained.
use anyhow::{bail, ensure, Context, Result};
use std::collections::HashMap;

pub const MAX_IDL: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Bool,
    Byte,
    I16,
    I32,
    I64,
    Double,
    String,
    Binary,
    Uuid,
    List(Box<Type>),
    Set(Box<Type>),
    Map(Box<Type>, Box<Type>),
    /// A struct, union or exception.
    Struct(String),
    Enum(String),
}

#[derive(Debug, Clone)]
pub struct Field {
    pub id: i16,
    pub name: String,
    pub ty: Type,
    pub required: bool,
}

#[derive(Debug, Clone)]
pub struct Function {
    pub name: String,
    pub oneway: bool,
    /// None for void.
    pub returns: Option<Type>,
    pub args: Vec<Field>,
    pub throws: Vec<Field>,
}

#[derive(Debug, Clone, Default)]
pub struct Service {
    pub name: String,
    pub extends: Option<String>,
    pub functions: Vec<Function>,
}

#[derive(Debug, Default)]
pub struct Idl {
    pub structs: HashMap<String, Vec<Field>>,
    pub unions: Vec<String>,
    pub exceptions: Vec<String>,
    pub enums: HashMap<String, Vec<(String, i32)>>,
    pub services: Vec<Service>,
    typedefs: HashMap<String, Type>,
}

impl Idl {
    pub fn service(&self, name: Option<&str>) -> Result<&Service> {
        match name {
            Some(n) => self
                .services
                .iter()
                .find(|s| s.name == n)
                .with_context(|| format!("the IDL has no service {n}")),
            None => self.services.last().context("the IDL defines no service"),
        }
    }
    /// A function of `service` or of a service it extends.
    pub fn function<'a>(&'a self, service: &'a Service, name: &str) -> Option<&'a Function> {
        let mut s = Some(service);
        let mut hops = 0;
        while let (Some(svc), true) = (s, hops < 16) {
            if let Some(f) = svc.functions.iter().find(|f| f.name == name) {
                return Some(f);
            }
            s = svc.extends.as_deref().and_then(|e| {
                self.services
                    .iter()
                    .find(|x| x.name == e.rsplit('.').next().unwrap_or(e))
            });
            hops += 1;
        }
        None
    }
    pub fn enum_name(&self, name: &str, value: i32) -> Option<&str> {
        self.enums
            .get(name)?
            .iter()
            .find(|(_, v)| *v == value)
            .map(|(n, _)| n.as_str())
    }
    pub fn enum_value(&self, name: &str, label: &str) -> Option<i32> {
        self.enums
            .get(name)?
            .iter()
            .find(|(n, _)| n == label)
            .map(|(_, v)| *v)
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Int(i64),
    Str(String),
    Sym(char),
}

fn lex(src: &str) -> Result<Vec<Tok>> {
    let b: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '#' || (c == '/' && b.get(i + 1) == Some(&'/')) {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == '*' && b[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if c == '"' || c == '\'' {
            let q = c;
            let start = i + 1;
            i += 1;
            while i < b.len() && b[i] != q {
                i += 1;
            }
            ensure!(i < b.len(), "unterminated string literal");
            out.push(Tok::Str(b[start..i].iter().collect()));
            i += 1;
        } else if c.is_ascii_digit()
            || (c == '-' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == '.' || b[i] == 'x') {
                i += 1;
            }
            let text: String = b[start..i].iter().collect();
            let n = if let Some(hex) = text.strip_prefix("0x") {
                i64::from_str_radix(hex, 16)
            } else {
                text.parse::<i64>()
            };
            // Float constants only appear as defaults, which are skipped.
            out.push(Tok::Int(n.unwrap_or(0)));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '.') {
                i += 1;
            }
            out.push(Tok::Ident(b[start..i].iter().collect()));
        } else {
            out.push(Tok::Sym(c));
            i += 1;
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn next(&mut self) -> Result<Tok> {
        let t = self
            .toks
            .get(self.pos)
            .cloned()
            .context("unexpected end of IDL")?;
        self.pos += 1;
        Ok(t)
    }
    fn ident(&mut self) -> Result<String> {
        match self.next()? {
            Tok::Ident(s) => Ok(s),
            t => bail!("expected a name, found {t:?}"),
        }
    }
    fn sym(&mut self, c: char) -> Result<()> {
        match self.next()? {
            Tok::Sym(s) if s == c => Ok(()),
            t => bail!("expected '{c}', found {t:?}"),
        }
    }
    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(&Tok::Sym(c)) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn eat_word(&mut self, w: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Ident(s)) if s == w) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn separator(&mut self) {
        let _ = self.eat(',') || self.eat(';');
    }
    /// Skip a constant value (default values and const bodies), including lists and maps.
    fn skip_value(&mut self) -> Result<()> {
        match self.next()? {
            Tok::Sym('[') | Tok::Sym('{') => {
                let mut depth = 1;
                while depth > 0 {
                    match self.next()? {
                        Tok::Sym('[') | Tok::Sym('{') => depth += 1,
                        Tok::Sym(']') | Tok::Sym('}') => depth -= 1,
                        _ => {}
                    }
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    /// Skip a trailing annotation list `( key = "value", ... )`.
    fn skip_annotations(&mut self) -> Result<()> {
        if self.eat('(') {
            while !self.eat(')') {
                self.next()?;
            }
        }
        Ok(())
    }
    fn ty(&mut self, depth: usize) -> Result<Type> {
        ensure!(depth < 32, "types nest too deeply");
        let name = self.ident()?;
        let t = match name.as_str() {
            "bool" => Type::Bool,
            "byte" | "i8" => Type::Byte,
            "i16" => Type::I16,
            "i32" => Type::I32,
            "i64" => Type::I64,
            "double" => Type::Double,
            "string" => Type::String,
            "binary" => Type::Binary,
            "uuid" => Type::Uuid,
            "list" | "set" => {
                self.sym('<')?;
                let e = self.ty(depth + 1)?;
                self.sym('>')?;
                if name == "list" {
                    Type::List(Box::new(e))
                } else {
                    Type::Set(Box::new(e))
                }
            }
            "map" => {
                self.sym('<')?;
                let k = self.ty(depth + 1)?;
                self.sym(',')?;
                let v = self.ty(depth + 1)?;
                self.sym('>')?;
                Type::Map(Box::new(k), Box::new(v))
            }
            other => Type::Struct(other.rsplit('.').next().unwrap_or(other).to_owned()),
        };
        self.skip_annotations()?;
        Ok(t)
    }
    fn fields(&mut self, close: char) -> Result<Vec<Field>> {
        let mut out: Vec<Field> = Vec::new();
        while !self.eat(close) {
            let id = match self.next()? {
                Tok::Int(n) => i16::try_from(n).context("field id out of range")?,
                t => {
                    bail!("expected a field id, found {t:?} (implicit field ids are not supported)")
                }
            };
            self.sym(':')?;
            let required = self.eat_word("required");
            let _ = self.eat_word("optional");
            let ty = self.ty(0)?;
            let name = self.ident()?;
            if self.eat('=') {
                self.skip_value()?;
            }
            self.skip_annotations()?;
            self.separator();
            ensure!(
                !out.iter().any(|f| f.id == id),
                "field id {id} is used twice"
            );
            out.push(Field {
                id,
                name,
                ty,
                required,
            });
        }
        Ok(out)
    }
}

pub fn parse(src: &str) -> Result<Idl> {
    ensure!(
        src.len() <= MAX_IDL,
        "the IDL is over {} KiB",
        MAX_IDL / 1024
    );
    let mut p = Parser {
        toks: lex(src)?,
        pos: 0,
    };
    let mut idl = Idl::default();
    while let Some(t) = p.peek().cloned() {
        let Tok::Ident(word) = t else {
            bail!("unexpected {t:?} at the top level")
        };
        p.pos += 1;
        match word.as_str() {
            "namespace" => {
                p.next()?;
                p.next()?;
            }
            "include" | "cpp_include" => {
                bail!("include is not supported: give one self-contained IDL")
            }
            "typedef" => {
                let t = p.ty(0)?;
                let name = p.ident()?;
                p.skip_annotations()?;
                p.separator();
                idl.typedefs.insert(name, t);
            }
            "const" => {
                p.ty(0)?;
                p.ident()?;
                p.sym('=')?;
                p.skip_value()?;
                p.separator();
            }
            "enum" => {
                let name = p.ident()?;
                p.sym('{')?;
                let mut values = Vec::new();
                let mut next = 0i32;
                while !p.eat('}') {
                    let label = p.ident()?;
                    if p.eat('=') {
                        next = match p.next()? {
                            Tok::Int(n) => i32::try_from(n).context("enum value out of range")?,
                            t => bail!("expected an enum value, found {t:?}"),
                        };
                    }
                    p.skip_annotations()?;
                    values.push((label, next));
                    next += 1;
                    p.separator();
                }
                p.skip_annotations()?;
                idl.enums.insert(name, values);
            }
            "struct" | "union" | "exception" => {
                let name = p.ident()?;
                p.sym('{')?;
                let fields = p.fields('}')?;
                p.skip_annotations()?;
                if word == "union" {
                    idl.unions.push(name.clone());
                }
                if word == "exception" {
                    idl.exceptions.push(name.clone());
                }
                idl.structs.insert(name, fields);
            }
            "service" => {
                let name = p.ident()?;
                let extends = if p.eat_word("extends") {
                    Some(p.ident()?)
                } else {
                    None
                };
                p.sym('{')?;
                let mut functions = Vec::new();
                while !p.eat('}') {
                    let oneway = p.eat_word("oneway");
                    let returns = if p.eat_word("void") {
                        None
                    } else {
                        Some(p.ty(0)?)
                    };
                    let fname = p.ident()?;
                    p.sym('(')?;
                    let args = p.fields(')')?;
                    let throws = if p.eat_word("throws") {
                        p.sym('(')?;
                        p.fields(')')?
                    } else {
                        vec![]
                    };
                    p.skip_annotations()?;
                    p.separator();
                    ensure!(
                        !oneway || (returns.is_none() && throws.is_empty()),
                        "oneway {fname} must be void without throws"
                    );
                    functions.push(Function {
                        name: fname,
                        oneway,
                        returns,
                        args,
                        throws,
                    });
                }
                p.skip_annotations()?;
                idl.services.push(Service {
                    name,
                    extends,
                    functions,
                });
            }
            other => bail!("unsupported IDL construct {other:?}"),
        }
    }
    idl.resolve()?;
    Ok(idl)
}

impl Idl {
    /// Replace typedef names and tell enums from structs, everywhere.
    fn resolve(&mut self) -> Result<()> {
        let typedefs = self.typedefs.clone();
        let enums: Vec<String> = self.enums.keys().cloned().collect();
        let structs: Vec<String> = self.structs.keys().cloned().collect();
        let fix = |t: &mut Type| -> Result<()> { resolve_type(t, &typedefs, &enums, &structs, 0) };
        for fields in self.structs.values_mut() {
            for f in fields.iter_mut() {
                fix(&mut f.ty)?;
            }
        }
        for s in &mut self.services {
            for f in &mut s.functions {
                if let Some(r) = &mut f.returns {
                    fix(r)?;
                }
                for a in f.args.iter_mut().chain(f.throws.iter_mut()) {
                    fix(&mut a.ty)?;
                }
            }
        }
        Ok(())
    }
}

fn resolve_type(
    t: &mut Type,
    typedefs: &HashMap<String, Type>,
    enums: &[String],
    structs: &[String],
    depth: usize,
) -> Result<()> {
    ensure!(depth < 32, "typedefs nest too deeply");
    match t {
        Type::List(e) | Type::Set(e) => resolve_type(e, typedefs, enums, structs, depth + 1),
        Type::Map(k, v) => {
            resolve_type(k, typedefs, enums, structs, depth + 1)?;
            resolve_type(v, typedefs, enums, structs, depth + 1)
        }
        Type::Struct(name) => {
            if let Some(real) = typedefs.get(name) {
                *t = real.clone();
                resolve_type(t, typedefs, enums, structs, depth + 1)
            } else if enums.contains(name) {
                *t = Type::Enum(name.clone());
                Ok(())
            } else {
                ensure!(structs.contains(name), "unknown type {name}");
                Ok(())
            }
        }
        _ => Ok(()),
    }
}
