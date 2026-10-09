//! iCalendar (RFC 5545) and vCard (RFC 6350 / 2426) objects as CalDAV and CardDAV need them:
//! content lines unfolded and parsed, components nested and checked, the UID found, and the
//! properties the REPORT filters read.
use anyhow::{bail, ensure, Context, Result};

pub const MAX_OBJECT_BYTES: usize = 512 * 1024;
const MAX_LINES: usize = 20_000;
const MAX_DEPTH: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub name: String,
    pub params: Vec<(String, String)>,
    pub value: String,
}

impl Property {
    pub fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Component {
    pub name: String,
    pub props: Vec<Property>,
    pub children: Vec<Component>,
}

impl Component {
    pub fn prop(&self, name: &str) -> Option<&Property> {
        self.props
            .iter()
            .find(|p| p.name.eq_ignore_ascii_case(name))
    }
    pub fn props_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Property> + 'a {
        self.props
            .iter()
            .filter(move |p| p.name.eq_ignore_ascii_case(name))
    }
}

/// Unfold (CRLF or LF followed by a space or tab continues the line) and split.
fn lines(text: &str) -> Result<Vec<String>> {
    ensure!(
        text.len() <= MAX_OBJECT_BYTES,
        "object over {} KiB",
        MAX_OBJECT_BYTES / 1024
    );
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let raw = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some(cont) = raw.strip_prefix([' ', '\t']) {
            let last = out
                .last_mut()
                .context("object starts with a continuation line")?;
            last.push_str(cont);
        } else if !raw.is_empty() {
            out.push(raw.to_owned());
        }
        ensure!(
            out.len() <= MAX_LINES,
            "object has more than {MAX_LINES} lines"
        );
    }
    Ok(out)
}

fn name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

/// `name *(";" param) ":" value`, with quoted parameter values.
fn parse_line(line: &str) -> Result<Property> {
    let mut i = 0;
    let b = line.as_bytes();
    while i < b.len() && b[i] != b';' && b[i] != b':' {
        i += 1;
    }
    let name = &line[..i];
    // vCard 3 allows a group prefix: "item1.EMAIL".
    let bare = name.rsplit('.').next().unwrap_or(name);
    ensure!(
        name_ok(bare) && name.split('.').all(name_ok),
        "invalid property name {name:?}"
    );
    let mut params = Vec::new();
    while i < b.len() && b[i] == b';' {
        i += 1;
        let start = i;
        while i < b.len() && b[i] != b'=' && b[i] != b';' && b[i] != b':' {
            i += 1;
        }
        let pname = line[start..i].to_owned();
        ensure!(name_ok(&pname), "invalid parameter name in {name}");
        let mut value = String::new();
        if i < b.len() && b[i] == b'=' {
            i += 1;
            loop {
                if i < b.len() && b[i] == b'"' {
                    let end = line[i + 1..]
                        .find('"')
                        .context("unterminated quoted parameter")?
                        + i
                        + 1;
                    value.push_str(&line[i + 1..end]);
                    i = end + 1;
                } else {
                    let start = i;
                    while i < b.len() && b[i] != b';' && b[i] != b':' && b[i] != b',' {
                        i += 1;
                    }
                    value.push_str(&line[start..i]);
                }
                if i < b.len() && b[i] == b',' {
                    value.push(',');
                    i += 1;
                } else {
                    break;
                }
            }
        }
        params.push((pname, value));
    }
    ensure!(i < b.len() && b[i] == b':', "property {name} has no value");
    Ok(Property {
        name: bare.to_ascii_uppercase(),
        params,
        value: line[i + 1..].to_owned(),
    })
}

/// Parse one top-level component (`BEGIN:<root>` ... `END:<root>`).
pub fn parse(text: &str, root: &str) -> Result<Component> {
    let lines = lines(text)?;
    let mut stack: Vec<Component> = Vec::new();
    let mut done: Option<Component> = None;
    for line in &lines {
        ensure!(done.is_none(), "content after END:{root}");
        let p = parse_line(line)?;
        match p.name.as_str() {
            "BEGIN" => {
                ensure!(stack.len() < MAX_DEPTH, "components nest too deeply");
                ensure!(name_ok(&p.value), "invalid component name");
                if stack.is_empty() {
                    ensure!(p.value.eq_ignore_ascii_case(root), "expected BEGIN:{root}");
                }
                stack.push(Component {
                    name: p.value.to_ascii_uppercase(),
                    props: vec![],
                    children: vec![],
                });
            }
            "END" => {
                let c = stack.pop().context("END without BEGIN")?;
                ensure!(
                    c.name.eq_ignore_ascii_case(&p.value),
                    "END:{} closes {}",
                    p.value,
                    c.name
                );
                match stack.last_mut() {
                    Some(parent) => parent.children.push(c),
                    None => done = Some(c),
                }
            }
            _ => stack
                .last_mut()
                .context("property outside a component")?
                .props
                .push(p),
        }
    }
    ensure!(stack.is_empty(), "unterminated component");
    done.with_context(|| format!("no {root} component"))
}

/// A calendar object resource (RFC 4791 §4.1): VCALENDAR with VERSION and PRODID, components of
/// one type (timezones aside) sharing one UID. Returns (component type, UID).
pub fn check_calendar(text: &str) -> Result<(String, String, Component)> {
    let cal = parse(text, "VCALENDAR")?;
    ensure!(
        cal.prop("VERSION").is_some_and(|v| v.value == "2.0"),
        "VCALENDAR needs VERSION:2.0"
    );
    ensure!(cal.prop("PRODID").is_some(), "VCALENDAR needs PRODID");
    let items: Vec<&Component> = cal
        .children
        .iter()
        .filter(|c| c.name != "VTIMEZONE")
        .collect();
    ensure!(!items.is_empty(), "no calendar component");
    let kind = items[0].name.clone();
    ensure!(
        matches!(kind.as_str(), "VEVENT" | "VTODO" | "VJOURNAL" | "VFREEBUSY"),
        "unsupported component {kind}"
    );
    ensure!(
        items.iter().all(|c| c.name == kind),
        "a calendar object holds one component type"
    );
    let uid = items[0]
        .prop("UID")
        .map(|u| u.value.clone())
        .context("component without UID")?;
    ensure!(
        !uid.is_empty() && uid.len() <= 255,
        "UID must be 1..255 characters"
    );
    ensure!(
        items
            .iter()
            .all(|c| c.prop("UID").is_some_and(|u| u.value == uid)),
        "all components share the UID"
    );
    Ok((kind, uid, cal))
}

/// A vCard address object (RFC 6352 §5.1): one VCARD with VERSION 3.0 or 4.0, FN and UID.
pub fn check_vcard(text: &str) -> Result<(String, Component)> {
    let card = parse(text, "VCARD")?;
    ensure!(
        card.prop("VERSION")
            .is_some_and(|v| v.value == "3.0" || v.value == "4.0"),
        "VCARD needs VERSION 3.0 or 4.0"
    );
    ensure!(card.prop("FN").is_some(), "VCARD needs FN");
    let uid = card
        .prop("UID")
        .map(|u| u.value.clone())
        .context("VCARD without UID")?;
    ensure!(
        !uid.is_empty() && uid.len() <= 255,
        "UID must be 1..255 characters"
    );
    Ok((uid, card))
}

/// iCalendar DATE or DATE-TIME as seconds since the epoch (floating and TZID times read as UTC,
/// which the CalDAV query documentation states).
pub fn timestamp(value: &str) -> Result<i64> {
    use chrono::{NaiveDate, NaiveDateTime, TimeZone, Utc};
    let v = value.trim_end_matches('Z');
    let t = if v.len() == 8 {
        NaiveDate::parse_from_str(v, "%Y%m%d")?
            .and_hms_opt(0, 0, 0)
            .context("date")?
    } else {
        NaiveDateTime::parse_from_str(v, "%Y%m%dT%H%M%S")?
    };
    Ok(Utc.from_utc_datetime(&t).timestamp())
}

/// RFC 5545 §3.3.6 duration in seconds (`P1D`, `PT1H30M`, `-P1W`).
pub fn duration(value: &str) -> Result<i64> {
    let (neg, v) = match value.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, value.strip_prefix('+').unwrap_or(value)),
    };
    let v = v.strip_prefix('P').context("duration starts with P")?;
    let mut total = 0i64;
    let mut num = String::new();
    let mut time = false;
    for c in v.chars() {
        match c {
            'T' => time = true,
            '0'..='9' => num.push(c),
            _ => {
                let n: i64 = num.parse().context("duration number")?;
                num.clear();
                total += n * match (c, time) {
                    ('W', _) => 604_800,
                    ('D', _) => 86_400,
                    ('H', true) => 3600,
                    ('M', true) => 60,
                    ('S', true) => 1,
                    _ => bail!("invalid duration unit {c}"),
                };
            }
        }
    }
    ensure!(num.is_empty(), "trailing number in duration");
    Ok(if neg { -total } else { total })
}

/// Whether a component overlaps `[start, end)` (RFC 4791 §9.9). A recurring component (RRULE
/// or RDATE) is treated as extending without end from its first start — recurrence sets are
/// not expanded, so a query may return a recurring object whose instances miss the range, never
/// the reverse.
pub fn overlaps(c: &Component, start: Option<i64>, end: Option<i64>) -> bool {
    let dt = |n: &str| c.prop(n).and_then(|p| timestamp(&p.value).ok());
    let begin = match c.name.as_str() {
        "VTODO" => dt("DTSTART").or_else(|| dt("DUE")),
        _ => dt("DTSTART"),
    };
    let Some(b) = begin else {
        return true;
    };
    let recurring = c.prop("RRULE").is_some() || c.prop("RDATE").is_some();
    let finish = if recurring {
        i64::MAX
    } else if let Some(e) = dt("DTEND").or_else(|| dt("DUE")) {
        e
    } else if let Some(d) = c.prop("DURATION").and_then(|p| duration(&p.value).ok()) {
        b + d
    } else if c.prop("DTSTART").is_some_and(|p| p.value.len() == 8) {
        b + 86_400
    } else {
        b
    };
    let after_start = start.is_none_or(|s| finish > s || (finish == b && b >= s));
    let before_end = end.is_none_or(|e| b < e);
    after_start && before_end
}

/// CalDAV/CardDAV text-match with the default `i;unicode-casemap` collation, approximated by
/// Unicode lower-casing; `match_type` is equals, contains, starts-with or ends-with.
pub fn text_match(value: &str, needle: &str, match_type: &str, negate: bool) -> bool {
    let (v, n) = (value.to_lowercase(), needle.to_lowercase());
    let hit = match match_type {
        "equals" => v == n,
        "starts-with" => v.starts_with(&n),
        "ends-with" => v.ends_with(&n),
        _ => v.contains(&n),
    };
    hit != negate
}
