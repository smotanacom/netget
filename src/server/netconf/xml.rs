//! Bounded XML represented by a flat, ordered stream of typed nodes.
//! No DOM recursion, DTD, external entities, raw XML action or configuration store.
use super::wire::MAX_MESSAGE_BYTES;
use anyhow::{bail, ensure, Context, Result};
use quick_xml::{events::Event, Reader};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_DEPTH: usize = 32;
pub const MAX_NODES: usize = 8192;
pub const MAX_TEXT_BYTES: usize = 65536;
pub const MAX_NAME_BYTES: usize = 256;
pub const MAX_URI_BYTES: usize = 4096;
pub const MAX_ATTRIBUTE_BYTES: usize = 4096;
pub const MAX_ATTRIBUTES: usize = 32;
pub const MAX_NAMESPACES: usize = 16;
pub const MAX_RETAINED_BYTES: usize = 8 * 1024 * 1024;
const XML: &str = "http://www.w3.org/XML/1998/namespace";
const XMLNS: &str = "http://www.w3.org/2000/xmlns/";

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    #[serde(default)]
    pub context: Vec<Binding>,
    pub nodes: Vec<Node>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub prefix: String,
    pub uri: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attribute {
    pub name: String,
    pub namespace: String,
    pub value: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    Start {
        name: String,
        namespace: String,
        #[serde(default)]
        bindings: Vec<Binding>,
        #[serde(default)]
        attributes: Vec<Attribute>,
    },
    End {
        name: String,
        namespace: String,
    },
    Text {
        text: String,
    },
}

fn xml_chars(text: &str) -> bool {
    text.chars().all(
        |c| matches!(c as u32, 9 | 10 | 13 | 0x20..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff),
    )
}
fn name_start(c: char) -> bool {
    matches!(c as u32, 0x41..=0x5a | 0x5f | 0x61..=0x7a | 0xc0..=0xd6 | 0xd8..=0xf6 | 0xf8..=0x2ff | 0x370..=0x37d | 0x37f..=0x1fff | 0x200c..=0x200d | 0x2070..=0x218f | 0x2c00..=0x2fef | 0x3001..=0xd7ff | 0xf900..=0xfdcf | 0xfdf0..=0xfffd | 0x10000..=0xeffff)
}
fn ncname(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(name_start)
        && chars.all(|c| name_start(c) || matches!(c as u32, 0x2d | 0x2e | 0x30..=0x39 | 0xb7 | 0x300..=0x36f | 0x203f..=0x2040))
}
pub fn qualified(name: &str) -> Result<(&str, &str)> {
    ensure!(
        !name.is_empty() && name.len() <= MAX_NAME_BYTES,
        "NETCONF XML name bound"
    );
    let (prefix, local) = match name.split_once(':') {
        Some((prefix, local)) => {
            ensure!(!prefix.is_empty(), "NETCONF empty XML name prefix");
            (prefix, local)
        }
        None => ("", name),
    };
    ensure!(
        (prefix.is_empty() || ncname(prefix)) && ncname(local),
        "NETCONF XML qualified name"
    );
    ensure!(
        prefix != "xmlns" && name != "xmlns",
        "NETCONF reserved namespace name"
    );
    Ok((prefix, local))
}
fn namespace(name: &str, element: bool, active: &BTreeMap<String, String>) -> Result<String> {
    let (prefix, _) = qualified(name)?;
    let value = match prefix {
        "xml" => XML,
        "" if !element => "",
        "" => active.get("").map(String::as_str).unwrap_or(""),
        _ => active
            .get(prefix)
            .map(String::as_str)
            .context("NETCONF undeclared XML namespace prefix")?,
    };
    ensure!(
        value.len() <= MAX_URI_BYTES && xml_chars(value),
        "NETCONF namespace URI bound"
    );
    Ok(value.to_owned())
}
fn bindings(
    values: &[Binding],
    inherited: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>> {
    ensure!(
        values.len() <= MAX_NAMESPACES,
        "NETCONF namespace binding count"
    );
    let mut active = inherited.clone();
    let mut seen = BTreeSet::new();
    for b in values {
        ensure!(
            b.prefix.len() <= MAX_NAME_BYTES && (b.prefix.is_empty() || ncname(&b.prefix)),
            "NETCONF namespace prefix"
        );
        ensure!(
            seen.insert(&b.prefix),
            "NETCONF duplicate namespace binding"
        );
        ensure!(
            b.uri.len() <= MAX_URI_BYTES && xml_chars(&b.uri),
            "NETCONF namespace URI bound"
        );
        ensure!(
            b.prefix != "xmlns" && b.uri != XMLNS,
            "NETCONF reserved namespace binding"
        );
        ensure!(
            (b.prefix == "xml") == (b.uri == XML),
            "NETCONF reserved xml prefix binding"
        );
        ensure!(
            b.prefix.is_empty() || !b.uri.is_empty(),
            "NETCONF empty prefixed namespace"
        );
        active.insert(b.prefix.clone(), b.uri.clone());
    }
    ensure!(
        active.len() <= MAX_NAMESPACES,
        "NETCONF active namespace count"
    );
    Ok(active)
}
fn retained(node: &Node) -> usize {
    match node {
        Node::Start {
            name,
            namespace,
            bindings,
            attributes,
        } => {
            256 + name.len()
                + namespace.len()
                + bindings
                    .iter()
                    .map(|b| 64 + b.prefix.len() + b.uri.len())
                    .sum::<usize>()
                + attributes
                    .iter()
                    .map(|a| 128 + a.name.len() + a.namespace.len() + a.value.len())
                    .sum::<usize>()
        }
        Node::End { name, namespace } => 128 + name.len() + namespace.len(),
        Node::Text { text } => 64 + text.len(),
    }
}
fn append(doc: &mut Document, node: Node, bytes: &mut usize) -> Result<()> {
    *bytes = bytes.saturating_add(retained(&node));
    ensure!(
        *bytes <= MAX_RETAINED_BYTES,
        "NETCONF XML retained-content bound"
    );
    if let Node::Text { text } = &node {
        ensure!(
            text.len() <= MAX_TEXT_BYTES && xml_chars(text),
            "NETCONF XML text bound/characters"
        );
        if let Some(Node::Text { text: previous }) = doc.nodes.last_mut() {
            ensure!(
                text.len() <= MAX_TEXT_BYTES.saturating_sub(previous.len()),
                "NETCONF contiguous XML text bound"
            );
            previous.push_str(text);
            return Ok(());
        }
    }
    ensure!(doc.nodes.len() < MAX_NODES, "NETCONF XML node count");
    doc.nodes.push(node);
    Ok(())
}

pub fn parse(message: &[u8]) -> Result<Document> {
    ensure!(
        !message.is_empty() && message.len() <= MAX_MESSAGE_BYTES,
        "NETCONF XML message byte bound"
    );
    let text = std::str::from_utf8(message)?;
    let mut reader = Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    reader.config_mut().check_comments = true;
    let mut doc = Document::default();
    let mut stack: Vec<(String, String, BTreeMap<String, String>)> = Vec::new();
    let mut bytes = 0usize;
    let mut roots = 0usize;
    let mut seen = false;
    loop {
        match reader.read_event()? {
            Event::Start(element) => {
                ensure!(stack.len() < MAX_DEPTH, "NETCONF XML depth bound");
                if stack.is_empty() {
                    roots += 1;
                    ensure!(roots == 1, "NETCONF multiple XML roots");
                }
                let name = std::str::from_utf8(element.name().as_ref())?.to_owned();
                qualified(&name)?;
                let mut local_bindings = Vec::new();
                let mut raw_attrs = Vec::new();
                for attribute in element.attributes() {
                    let attribute = attribute?;
                    ensure!(
                        attribute.value.len() <= MAX_ATTRIBUTE_BYTES,
                        "NETCONF raw XML attribute bound"
                    );
                    let key = std::str::from_utf8(attribute.key.as_ref())?;
                    // XML1.0 normalizes literal attribute whitespace before entity expansion.
                    let raw = std::str::from_utf8(&attribute.value)?
                        .replace("\r\n", " ")
                        .replace(['\r', '\n', '\t'], " ");
                    let value = quick_xml::escape::unescape(&raw)?.into_owned();
                    ensure!(
                        value.len() <= MAX_ATTRIBUTE_BYTES && xml_chars(&value),
                        "NETCONF XML attribute bound/characters"
                    );
                    if key == "xmlns" || key.starts_with("xmlns:") {
                        ensure!(
                            local_bindings.len() < MAX_NAMESPACES,
                            "NETCONF namespace binding count"
                        );
                        ensure!(
                            key == "xmlns" || !key.strip_prefix("xmlns:").unwrap_or("").is_empty(),
                            "NETCONF empty declared namespace prefix"
                        );
                        local_bindings.push(Binding {
                            prefix: key.strip_prefix("xmlns:").unwrap_or("").into(),
                            uri: value,
                        });
                    } else {
                        ensure!(
                            raw_attrs.len() < MAX_ATTRIBUTES,
                            "NETCONF XML attribute count"
                        );
                        qualified(key)?;
                        raw_attrs.push((key.to_owned(), value));
                    }
                }
                let active = bindings(
                    &local_bindings,
                    stack.last().map(|v| &v.2).unwrap_or(&BTreeMap::new()),
                )?;
                let ns = namespace(&name, true, &active)?;
                let mut attrs = Vec::new();
                let mut expanded = BTreeSet::new();
                for (name, value) in raw_attrs {
                    let attribute_ns = namespace(&name, false, &active)?;
                    ensure!(
                        expanded.insert((attribute_ns.clone(), qualified(&name)?.1.to_owned())),
                        "NETCONF duplicate expanded attribute name"
                    );
                    attrs.push(Attribute {
                        name,
                        namespace: attribute_ns,
                        value,
                    });
                }
                append(
                    &mut doc,
                    Node::Start {
                        name: name.clone(),
                        namespace: ns.clone(),
                        bindings: local_bindings,
                        attributes: attrs,
                    },
                    &mut bytes,
                )?;
                stack.push((name, ns, active));
                seen = true;
            }
            Event::End(element) => {
                let name = std::str::from_utf8(element.name().as_ref())?.to_owned();
                let ns = namespace(
                    &name,
                    true,
                    &stack.last().context("NETCONF unmatched XML end")?.2,
                )?;
                let (start, start_ns, _) = stack.pop().context("NETCONF unmatched XML end")?;
                ensure!(
                    name == start && ns == start_ns,
                    "NETCONF mismatched XML end"
                );
                append(
                    &mut doc,
                    Node::End {
                        name,
                        namespace: ns,
                    },
                    &mut bytes,
                )?;
            }
            Event::Text(value) => {
                ensure!(value.len() <= MAX_TEXT_BYTES, "NETCONF raw XML text bound");
                let raw = std::str::from_utf8(value.as_ref())?
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                let value = quick_xml::escape::unescape(&raw)?.into_owned();
                if stack.is_empty() {
                    ensure!(value.trim().is_empty(), "NETCONF text outside XML root");
                } else {
                    append(&mut doc, Node::Text { text: value }, &mut bytes)?;
                }
                seen = true;
            }
            Event::CData(value) => {
                ensure!(
                    !stack.is_empty() && value.len() <= MAX_TEXT_BYTES,
                    "NETCONF XML CDATA bound/position"
                );
                let value = std::str::from_utf8(value.as_ref())?
                    .replace("\r\n", "\n")
                    .replace('\r', "\n");
                append(&mut doc, Node::Text { text: value }, &mut bytes)?;
            }
            Event::Decl(decl) => {
                ensure!(
                    !seen && doc.nodes.is_empty(),
                    "NETCONF XML declaration position"
                );
                ensure!(
                    decl.version()?.as_ref() == b"1.0",
                    "NETCONF selected XML1.0 version"
                );
                if let Some(encoding) = decl.encoding() {
                    ensure!(
                        encoding?.as_ref().eq_ignore_ascii_case(b"utf-8"),
                        "NETCONF selected UTF8 encoding"
                    );
                }
                seen = true;
            }
            Event::DocType(_) => bail!("NETCONF XML DTD is refused"),
            Event::Comment(_) => seen = true,
            Event::PI(value) => {
                ensure!(
                    !value.target().eq_ignore_ascii_case(b"xml"),
                    "NETCONF reserved XML processing instruction"
                );
                seen = true;
            }
            Event::Empty(_) => unreachable!("empty XML elements expand into Start/End"),
            Event::Eof => break,
        }
    }
    ensure!(
        roots == 1 && stack.is_empty(),
        "NETCONF incomplete XML document"
    );
    Ok(doc)
}

fn escape(output: &mut String, text: &str, attribute: bool) -> Result<()> {
    ensure!(xml_chars(text), "NETCONF outgoing XML characters");
    for c in text.chars() {
        let encoded = match c {
            '&' => Some("&amp;"),
            '<' => Some("&lt;"),
            '>' => Some("&gt;"),
            '"' if attribute => Some("&quot;"),
            '\r' => Some("&#13;"),
            '\n' if attribute => Some("&#10;"),
            '\t' if attribute => Some("&#9;"),
            _ => None,
        };
        if let Some(encoded) = encoded {
            output.push_str(encoded);
        } else {
            output.push(c);
        }
        ensure!(
            output.len() <= MAX_MESSAGE_BYTES,
            "NETCONF outgoing XML byte bound"
        );
    }
    Ok(())
}
fn binding_text(output: &mut String, prefix: &str, uri: &str) -> Result<()> {
    output.push_str(" xmlns");
    if !prefix.is_empty() {
        output.push(':');
        output.push_str(prefix);
    }
    output.push_str("=\"");
    escape(output, uri, true)?;
    output.push('"');
    Ok(())
}

/// Render a typed fragment inside a namespaced wrapper. Prefix selection cannot
/// be redirected by configuration bindings or QName-valued attribute content.
pub fn embedded(doc: &Document, local: &str, uri: &str) -> Result<String> {
    ensure!(
        ncname(local) && uri.len() <= MAX_URI_BYTES,
        "NETCONF XML wrapper name/URI"
    );
    ensure!(
        doc.nodes.len() <= MAX_NODES,
        "NETCONF outgoing XML node count"
    );
    let context = bindings(&doc.context, &BTreeMap::new())?;
    let mut prefixes: BTreeSet<&str> = doc.context.iter().map(|b| b.prefix.as_str()).collect();
    for node in &doc.nodes {
        if let Node::Start { bindings, .. } = node {
            prefixes.extend(bindings.iter().map(|b| b.prefix.as_str()));
        }
    }
    let prefix = (0..=prefixes.len())
        .map(|n| format!("netget{n}"))
        .find(|p| !prefixes.contains(p.as_str()))
        .context("NETCONF XML wrapper prefix")?;
    let mut output = format!("<{prefix}:{local}");
    binding_text(&mut output, &prefix, uri)?;
    for b in &doc.context {
        binding_text(&mut output, &b.prefix, &b.uri)?;
    }
    output.push('>');
    let mut stack: Vec<(String, String, BTreeMap<String, String>)> = Vec::new();
    let mut bytes = 0usize;
    for node in &doc.nodes {
        bytes = bytes.saturating_add(retained(node));
        ensure!(
            bytes <= MAX_RETAINED_BYTES,
            "NETCONF outgoing XML retained-content bound"
        );
        match node {
            Node::Start {
                name,
                namespace,
                bindings: local_bindings,
                attributes,
            } => {
                ensure!(
                    stack.len() < MAX_DEPTH && attributes.len() <= MAX_ATTRIBUTES,
                    "NETCONF outgoing XML depth/attribute bound"
                );
                let (p, _) = qualified(name)?;
                ensure!(
                    namespace.len() <= MAX_URI_BYTES
                        && xml_chars(namespace)
                        && (p.is_empty() || !namespace.is_empty()),
                    "NETCONF outgoing element namespace"
                );
                let inherited = stack.last().map(|v| &v.2).unwrap_or(&context);
                let mut active = bindings(local_bindings, inherited)?;
                let mut declarations: BTreeMap<String, String> = local_bindings
                    .iter()
                    .map(|b| (b.prefix.clone(), b.uri.clone()))
                    .collect();
                let mut declare = |p: &str, ns: &str| -> Result<()> {
                    ensure!(
                        p != "xmlns" && ns != XMLNS && ((p == "xml") == (ns == XML)),
                        "NETCONF outgoing reserved namespace"
                    );
                    if let Some(explicit) = declarations.get(p) {
                        ensure!(explicit == ns, "NETCONF conflicting namespace declaration");
                    }
                    if active.get(p).map(String::as_str).unwrap_or("") != ns {
                        active.insert(p.to_owned(), ns.to_owned());
                        declarations.insert(p.to_owned(), ns.to_owned());
                    }
                    Ok(())
                };
                declare(p, namespace)?;
                let mut expanded = BTreeSet::new();
                for a in attributes {
                    let (p, local) = qualified(&a.name)?;
                    ensure!(
                        a.namespace.len() <= MAX_URI_BYTES
                            && a.value.len() <= MAX_ATTRIBUTE_BYTES
                            && xml_chars(&a.value),
                        "NETCONF outgoing attribute bound"
                    );
                    ensure!(
                        !p.is_empty() || a.namespace.is_empty(),
                        "NETCONF unprefixed attribute namespace"
                    );
                    if !p.is_empty() {
                        ensure!(
                            !a.namespace.is_empty(),
                            "NETCONF prefixed attribute namespace"
                        );
                        declare(p, &a.namespace)?;
                    }
                    ensure!(
                        expanded.insert((&a.namespace, local)),
                        "NETCONF duplicate outgoing expanded attribute"
                    );
                }
                ensure!(
                    active.len() <= MAX_NAMESPACES,
                    "NETCONF outgoing active namespace count"
                );
                output.push('<');
                output.push_str(name);
                for (p, ns) in declarations {
                    binding_text(&mut output, &p, &ns)?;
                }
                for a in attributes {
                    output.push(' ');
                    output.push_str(&a.name);
                    output.push_str("=\"");
                    escape(&mut output, &a.value, true)?;
                    output.push('"');
                }
                output.push('>');
                stack.push((name.clone(), namespace.clone(), active));
            }
            Node::End { name, namespace } => {
                let (start, ns, _) = stack.pop().context("NETCONF outgoing unmatched XML end")?;
                ensure!(
                    *name == start && *namespace == ns,
                    "NETCONF outgoing mismatched XML end"
                );
                output.push_str("</");
                output.push_str(name);
                output.push('>');
            }
            Node::Text { text } => {
                ensure!(
                    text.len() <= MAX_TEXT_BYTES,
                    "NETCONF outgoing XML text bound"
                );
                escape(&mut output, text, false)?;
            }
        }
        ensure!(
            output.len() <= MAX_MESSAGE_BYTES,
            "NETCONF outgoing XML byte bound"
        );
    }
    ensure!(stack.is_empty(), "NETCONF outgoing incomplete XML fragment");
    output.push_str(&format!("</{prefix}:{local}>"));
    ensure!(
        output.len() <= MAX_MESSAGE_BYTES,
        "NETCONF outgoing XML byte bound"
    );
    // The wrapper contributes one XML level and two nodes to the wire budget.
    parse(output.as_bytes())?;
    Ok(output)
}

pub fn element(doc: &Document, index: usize) -> Result<(&str, &str, &[Attribute])> {
    match doc.nodes.get(index) {
        Some(Node::Start {
            name,
            namespace,
            attributes,
            ..
        }) => Ok((qualified(name)?.1, namespace, attributes)),
        _ => bail!("NETCONF expected XML element"),
    }
}
pub fn is_element(doc: &Document, index: usize, local: &str, uri: &str) -> bool {
    element(doc, index).is_ok_and(|(name, namespace, _)| name == local && namespace == uri)
}
pub fn end_index(doc: &Document, start: usize) -> Result<usize> {
    element(doc, start)?;
    let mut depth = 0usize;
    for (index, node) in doc.nodes.iter().enumerate().skip(start) {
        match node {
            Node::Start { .. } => {
                ensure!(depth < MAX_DEPTH, "NETCONF XML subtree depth");
                depth += 1;
            }
            Node::End { .. } => {
                depth = depth.checked_sub(1).context("NETCONF XML subtree end")?;
                if depth == 0 {
                    return Ok(index);
                }
            }
            Node::Text { .. } => {}
        }
    }
    bail!("NETCONF incomplete XML subtree")
}
/// Immediate child elements of a protocol envelope; non-whitespace mixed text
/// is refused here, while configuration fragments preserve their mixed content.
pub fn children(doc: &Document, start: usize) -> Result<Vec<usize>> {
    let end = end_index(doc, start)?;
    let mut index = start + 1;
    let mut children = Vec::new();
    while index < end {
        match &doc.nodes[index] {
            Node::Start { .. } => {
                children.push(index);
                index = end_index(doc, index)? + 1;
            }
            Node::Text { text } => {
                ensure!(
                    text.trim().is_empty(),
                    "NETCONF protocol envelope mixed text"
                );
                index += 1;
            }
            Node::End { .. } => bail!("NETCONF unexpected XML child end"),
        }
    }
    Ok(children)
}
pub fn text(doc: &Document, start: usize) -> Result<String> {
    let end = end_index(doc, start)?;
    let mut output = String::new();
    for node in &doc.nodes[start + 1..end] {
        if let Node::Text { text } = node {
            ensure!(
                text.len() <= MAX_TEXT_BYTES.saturating_sub(output.len()),
                "NETCONF XML scalar text bound"
            );
            output.push_str(text);
        } else {
            bail!("NETCONF scalar XML field has child elements")
        }
    }
    Ok(output)
}
/// Detach configuration content while carrying all active namespace bindings,
/// including bindings used only by QName-valued text or attributes.
pub fn inner(doc: &Document, start: usize) -> Result<Document> {
    let end = end_index(doc, start)?;
    let mut scopes = vec![bindings(&doc.context, &BTreeMap::new())?];
    for (index, node) in doc.nodes.iter().enumerate().take(start + 1) {
        match node {
            Node::Start {
                bindings: local, ..
            } => {
                ensure!(
                    scopes.len() <= MAX_DEPTH,
                    "NETCONF XML namespace scope depth"
                );
                scopes.push(bindings(local, scopes.last().expect("context scope"))?);
            }
            Node::End { .. } => {
                ensure!(scopes.len() > 1, "NETCONF XML namespace scope underflow");
                scopes.pop();
            }
            Node::Text { .. } => {}
        }
        if index == start {
            break;
        }
    }
    Ok(Document {
        context: scopes
            .last()
            .expect("context scope")
            .iter()
            .map(|(prefix, uri)| Binding {
                prefix: prefix.clone(),
                uri: uri.clone(),
            })
            .collect(),
        nodes: doc.nodes[start + 1..end].to_vec(),
    })
}

const FRAGMENT_ROOT: &str = "netget-fragment";
const FRAGMENT_URI: &str = "urn:netget:netconf:fragment";

/// Parse handler-supplied XML content (zero or more sibling elements and text) with the
/// same bounds as a wire message. The text is markup the handler wrote; it is parsed, never
/// spliced, so a fragment cannot close the envelope it is placed in or declare a DTD.
pub fn parse_fragment(text: &str) -> Result<Document> {
    parse_fragment_in(text, None)
}

/// [`parse_fragment`] where unprefixed elements without their own `xmlns` belong to
/// `default_namespace` — for content whose schema is the envelope's own, such as the RFC 6241
/// `<error-info>` elements (`session-id`, `bad-element`, …).
pub fn parse_fragment_in(text: &str, default_namespace: Option<&str>) -> Result<Document> {
    ensure!(
        text.len() <= MAX_MESSAGE_BYTES - 128,
        "NETCONF XML fragment byte bound"
    );
    ensure!(
        !text.trim_start().starts_with("<?xml"),
        "NETCONF XML fragment must not carry a declaration"
    );
    let wrapper = default_namespace.unwrap_or(FRAGMENT_URI);
    let wrapped = format!("<{FRAGMENT_ROOT} xmlns=\"{wrapper}\">{text}</{FRAGMENT_ROOT}>");
    let doc = parse(wrapped.as_bytes())?;
    let mut inner = inner(&doc, 0)?;
    // The wrapper's default namespace is an artefact of wrapping: unprefixed elements in the
    // fragment that did not declare their own namespace must stay in no namespace.
    inner
        .context
        .retain(|b| !(b.prefix.is_empty() && b.uri == wrapper));
    for node in &mut inner.nodes {
        if default_namespace.is_some() {
            break;
        }
        if let Node::Start { namespace, .. } | Node::End { namespace, .. } = node {
            if namespace == FRAGMENT_URI {
                namespace.clear();
            }
        }
    }
    Ok(inner)
}

/// Render a detached fragment back to XML text. Each top-level element declares the
/// bindings that were in scope where the fragment was taken, so the text stands alone.
pub fn render_fragment(doc: &Document) -> Result<String> {
    let mut output = String::new();
    let mut depth = 0usize;
    for node in &doc.nodes {
        match node {
            Node::Start {
                name,
                namespace,
                bindings: local,
                attributes,
            } => {
                ensure!(depth < MAX_DEPTH, "NETCONF outgoing XML depth bound");
                let (element_prefix, _) = qualified(name)?;
                output.push('<');
                output.push_str(name);
                let mut declared: BTreeMap<&str, &str> = BTreeMap::new();
                if depth == 0 {
                    for b in &doc.context {
                        declared.insert(&b.prefix, &b.uri);
                    }
                }
                for b in local {
                    declared.insert(&b.prefix, &b.uri);
                }
                // An unprefixed top-level element states its own namespace, so it cannot
                // inherit the default namespace of whatever envelope it is placed in.
                if depth == 0
                    && element_prefix.is_empty()
                    && declared.get("") != Some(&namespace.as_str())
                {
                    declared.insert("", namespace);
                }
                for (prefix, uri) in declared {
                    if prefix == "xml" {
                        continue;
                    }
                    binding_text(&mut output, prefix, uri)?;
                }
                for a in attributes {
                    qualified(&a.name)?;
                    output.push(' ');
                    output.push_str(&a.name);
                    output.push_str("=\"");
                    escape(&mut output, &a.value, true)?;
                    output.push('"');
                }
                output.push('>');
                depth += 1;
            }
            Node::End { name, .. } => {
                depth = depth
                    .checked_sub(1)
                    .context("NETCONF outgoing unmatched XML end")?;
                output.push_str("</");
                output.push_str(name);
                output.push('>');
            }
            Node::Text { text } => escape(&mut output, text, false)?,
        }
        ensure!(
            output.len() <= MAX_MESSAGE_BYTES,
            "NETCONF outgoing XML byte bound"
        );
    }
    ensure!(depth == 0, "NETCONF outgoing incomplete XML fragment");
    Ok(output)
}
