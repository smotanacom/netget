//! A small, bounded, namespace-aware XML tree for EPP frames, built without recursion; and
//! escaping for the documents NetGet writes. No DTD and no entities beyond the five predefined.
use anyhow::{bail, ensure, Context, Result};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;

pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 4096;

pub const EPP: &str = "urn:ietf:params:xml:ns:epp-1.0";
pub const DOMAIN: &str = "urn:ietf:params:xml:ns:domain-1.0";
pub const HOST: &str = "urn:ietf:params:xml:ns:host-1.0";
pub const CONTACT: &str = "urn:ietf:params:xml:ns:contact-1.0";

#[derive(Debug, Default, Clone)]
pub struct Node {
    pub ns: String,
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
    pub fn text(&self) -> String {
        self.text.trim().to_owned()
    }
    pub fn text_of(&self, name: &str) -> Option<String> {
        self.child(name).map(Node::text)
    }
    pub fn texts_of(&self, name: &str) -> Vec<String> {
        self.all(name).map(Node::text).collect()
    }
    /// The first element child, for the command or object element under a wrapper.
    pub fn first(&self) -> Option<&Node> {
        self.children.first()
    }
}

/// Parse one EPP document into its root element.
pub fn parse(bytes: &[u8]) -> Result<Node> {
    let mut reader = NsReader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buf = Vec::new();
    let mut stack: Vec<Node> = Vec::new();
    let mut root: Option<Node> = None;
    let mut nodes = 0usize;
    loop {
        let (ns, event) = reader
            .read_resolved_event_into(&mut buf)
            .context("the frame is not well-formed XML")?;
        let ns = match ns {
            ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
            ResolveResult::Unbound => String::new(),
            ResolveResult::Unknown(p) => {
                bail!("the prefix {} is not declared", String::from_utf8_lossy(&p))
            }
        };
        match event {
            Event::Start(e) | Event::Empty(e) if root.is_some() => {
                let _ = e;
                bail!("content after the root element")
            }
            Event::Start(ref e) | Event::Empty(ref e) => {
                nodes += 1;
                ensure!(nodes <= MAX_NODES, "more than {MAX_NODES} elements");
                ensure!(stack.len() < MAX_DEPTH, "nested deeper than {MAX_DEPTH}");
                let mut node = Node {
                    ns,
                    name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                    ..Default::default()
                };
                for a in e.attributes() {
                    let a = a.context("a malformed attribute")?;
                    let key = a.key.as_ref();
                    if key == b"xmlns" || key.starts_with(b"xmlns:") {
                        continue;
                    }
                    ensure!(node.attrs.len() < 32, "too many attributes");
                    let local = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
                    let value = a
                        .decode_and_unescape_value(reader.decoder())
                        .context("an attribute value is not valid")?
                        .into_owned();
                    node.attrs.push((local, value));
                }
                if matches!(event, Event::Empty(_)) {
                    match stack.last_mut() {
                        Some(parent) => parent.children.push(node),
                        None => root = Some(node),
                    }
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                let node = stack.pop().context("an unbalanced end tag")?;
                match stack.last_mut() {
                    Some(parent) => parent.children.push(node),
                    None => root = Some(node),
                }
            }
            Event::Text(t) => {
                let text = t.unescape().context("text is not valid")?;
                match stack.last_mut() {
                    Some(n) => {
                        ensure!(n.text.len() + text.len() <= 65536, "text too long");
                        n.text.push_str(&text);
                    }
                    None => ensure!(text.trim().is_empty(), "text outside the root element"),
                }
            }
            Event::CData(t) => {
                if let Some(n) = stack.last_mut() {
                    ensure!(n.text.len() + t.len() <= 65536, "text too long");
                    n.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::DocType(_) => bail!("a DOCTYPE is not allowed"),
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
        }
        buf.clear();
    }
    ensure!(stack.is_empty(), "the document ends inside an element");
    root.context("the frame holds no element")
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// `<p:tag>text</p:tag>`, escaped.
pub fn el(tag: &str, text: &str) -> String {
    format!("<{tag}>{}</{tag}>", escape(text))
}
