//! WebDAV XML (RFC 4918) as CalDAV and CardDAV use it: request bodies parsed into a small
//! namespace-resolved tree, PROPFIND and REPORT bodies interpreted, multistatus responses
//! written.
use anyhow::{bail, ensure, Context, Result};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::NsReader;

pub const DAV: &str = "DAV:";
pub const CALDAV: &str = "urn:ietf:params:xml:ns:caldav";
pub const CARDDAV: &str = "urn:ietf:params:xml:ns:carddav";
pub const CS: &str = "http://calendarserver.org/ns/";
pub const APPLE: &str = "http://apple.com/ns/ical/";
pub const MAX_BODY: usize = 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_ELEMENTS: usize = 20_000;

#[derive(Debug, Clone, Default)]
pub struct El {
    pub ns: String,
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<El>,
    pub text: String,
}

impl El {
    pub fn is(&self, ns: &str, name: &str) -> bool {
        self.ns == ns && self.name == name
    }
    pub fn child(&self, ns: &str, name: &str) -> Option<&El> {
        self.children.iter().find(|c| c.is(ns, name))
    }
    pub fn children_named<'a>(
        &'a self,
        ns: &'a str,
        name: &'a str,
    ) -> impl Iterator<Item = &'a El> + 'a {
        self.children.iter().filter(move |c| c.is(ns, name))
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// Parse a request body into a tree (namespaces resolved, depth and size bounded).
pub fn parse(body: &[u8]) -> Result<El> {
    ensure!(body.len() <= MAX_BODY, "XML body over 1 MiB");
    let text = std::str::from_utf8(body).context("XML body is not UTF-8")?;
    let mut reader = NsReader::from_str(text);
    let mut stack: Vec<El> = Vec::new();
    let mut count = 0usize;
    let mut root: Option<El> = None;
    let resolve = |r: ResolveResult| match r {
        ResolveResult::Bound(ns) => String::from_utf8_lossy(ns.as_ref()).into_owned(),
        _ => String::new(),
    };
    loop {
        let (ns, ev) = reader.read_resolved_event()?;
        let empty = matches!(ev, Event::Empty(_));
        match ev {
            Event::Start(e) | Event::Empty(e) => {
                ensure!(root.is_none(), "content after the root element");
                count += 1;
                ensure!(count <= MAX_ELEMENTS, "XML has too many elements");
                ensure!(stack.len() < MAX_DEPTH, "XML nests too deeply");
                let mut el = El {
                    ns: resolve(ns),
                    name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
                    ..Default::default()
                };
                for a in e.attributes().flatten() {
                    el.attrs.push((
                        String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned(),
                        a.decode_and_unescape_value(reader.decoder())
                            .map(|v| v.into_owned())
                            .unwrap_or_default(),
                    ));
                }
                if empty {
                    match stack.last_mut() {
                        Some(p) => p.children.push(el),
                        None => root = Some(el),
                    }
                } else {
                    stack.push(el);
                }
            }
            Event::End(_) => {
                let done = stack.pop().context("unbalanced XML")?;
                match stack.last_mut() {
                    Some(p) => p.children.push(done),
                    None => root = Some(done),
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    let s = t.unescape().map(|c| c.into_owned()).unwrap_or_default();
                    ensure!(top.text.len() + s.len() <= MAX_BODY, "XML text too large");
                    top.text.push_str(&s);
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            Event::DocType(_) => bail!("DOCTYPE is not accepted"),
            Event::Eof => break,
            _ => {}
        }
    }
    ensure!(stack.is_empty(), "unterminated XML");
    root.context("empty XML body")
}

pub type PropName = (String, String);

#[derive(Debug, Clone, PartialEq)]
pub enum PropRequest {
    AllProp,
    PropName,
    Props(Vec<PropName>),
}

fn prop_names(prop: &El) -> Vec<PropName> {
    prop.children
        .iter()
        .map(|c| (c.ns.clone(), c.name.clone()))
        .collect()
}

/// A PROPFIND body; an empty body means allprop (RFC 4918 §9.1).
pub fn propfind(body: &[u8]) -> Result<PropRequest> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(PropRequest::AllProp);
    }
    let root = parse(body)?;
    ensure!(
        root.is(DAV, "propfind"),
        "PROPFIND body must be DAV:propfind"
    );
    if root.child(DAV, "allprop").is_some() {
        return Ok(PropRequest::AllProp);
    }
    if root.child(DAV, "propname").is_some() {
        return Ok(PropRequest::PropName);
    }
    let prop = root
        .child(DAV, "prop")
        .context("propfind needs prop, allprop or propname")?;
    Ok(PropRequest::Props(prop_names(prop)))
}

/// A CalDAV time-range or CardDAV/CalDAV text/prop filter tree, kept as XML for the engine.
#[derive(Debug, Clone)]
pub enum Report {
    CalendarQuery {
        props: PropRequest,
        filter: El,
    },
    CalendarMultiget {
        props: PropRequest,
        hrefs: Vec<String>,
    },
    AddressbookQuery {
        props: PropRequest,
        filter: El,
        limit: Option<usize>,
    },
    AddressbookMultiget {
        props: PropRequest,
        hrefs: Vec<String>,
    },
}

fn report_props(root: &El) -> PropRequest {
    if root.child(DAV, "allprop").is_some() {
        PropRequest::AllProp
    } else if let Some(p) = root.child(DAV, "prop") {
        PropRequest::Props(prop_names(p))
    } else {
        PropRequest::AllProp
    }
}

pub fn report(body: &[u8]) -> Result<Report> {
    let root = parse(body)?;
    let hrefs = || -> Vec<String> {
        root.children_named(DAV, "href")
            .map(|h| h.text.trim().to_owned())
            .collect()
    };
    Ok(match (root.ns.as_str(), root.name.as_str()) {
        (CALDAV, "calendar-query") => Report::CalendarQuery {
            props: report_props(&root),
            filter: root
                .child(CALDAV, "filter")
                .cloned()
                .context("calendar-query needs a filter")?,
        },
        (CALDAV, "calendar-multiget") => Report::CalendarMultiget {
            props: report_props(&root),
            hrefs: hrefs(),
        },
        (CARDDAV, "addressbook-query") => Report::AddressbookQuery {
            props: report_props(&root),
            filter: root.child(CARDDAV, "filter").cloned().unwrap_or_default(),
            limit: root
                .child(CARDDAV, "limit")
                .and_then(|l| l.child(CARDDAV, "nresults"))
                .and_then(|n| n.text.trim().parse().ok()),
        },
        (CARDDAV, "addressbook-multiget") => Report::AddressbookMultiget {
            props: report_props(&root),
            hrefs: hrefs(),
        },
        (ns, name) => bail!("unsupported REPORT {{{ns}}}{name}"),
    })
}

pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

pub fn prefix(ns: &str) -> &'static str {
    match ns {
        DAV => "D",
        CALDAV => "C",
        CARDDAV => "CR",
        CS => "CS",
        APPLE => "A",
        _ => "X",
    }
}

/// An element in the multistatus output, `<P:name>inner</P:name>` (namespace `X` is declared
/// per element for unknown namespaces).
pub fn element(ns: &str, name: &str, inner: &str) -> String {
    let p = prefix(ns);
    let decl = if p == "X" {
        format!(" xmlns:X=\"{}\"", escape(ns))
    } else {
        String::new()
    };
    if inner.is_empty() {
        format!("<{p}:{name}{decl}/>")
    } else {
        format!("<{p}:{name}{decl}>{inner}</{p}:{name}>")
    }
}

/// One `<D:response>`: found properties with 200, missing ones with 404.
pub fn response(href: &str, found: &[(PropName, String)], missing: &[PropName]) -> String {
    let mut out = format!("<D:response><D:href>{}</D:href>", escape(href));
    if !found.is_empty() {
        out.push_str("<D:propstat><D:prop>");
        for ((ns, name), inner) in found {
            out.push_str(&element(ns, name, inner));
        }
        out.push_str("</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>");
    }
    if !missing.is_empty() {
        out.push_str("<D:propstat><D:prop>");
        for (ns, name) in missing {
            out.push_str(&element(ns, name, ""));
        }
        out.push_str("</D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>");
    }
    out.push_str("</D:response>");
    out
}

pub fn status_response(href: &str, status: &str) -> String {
    format!(
        "<D:response><D:href>{}</D:href><D:status>HTTP/1.1 {status}</D:status></D:response>",
        escape(href)
    )
}

pub fn multistatus(responses: &[String]) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:multistatus xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\" xmlns:CR=\"{CARDDAV}\" xmlns:CS=\"{CS}\" xmlns:A=\"{APPLE}\">{}</D:multistatus>",
        responses.concat()
    )
}

/// A DAV error body carrying one precondition element (RFC 4918 §16).
pub fn error_body(ns: &str, condition: &str) -> String {
    format!("<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<D:error xmlns:D=\"DAV:\" xmlns:C=\"{CALDAV}\" xmlns:CR=\"{CARDDAV}\">{}</D:error>", element(ns, condition, ""))
}
