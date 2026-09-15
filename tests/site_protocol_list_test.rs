//! The landing page's protocol table is derived from `Cargo.toml`, and this keeps it that way.
//!
//! `site/index.html` lists every protocol feature in a hand-grouped table, with the ones
//! compiled into the browser demo highlighted. Both facts drift silently — a protocol lands
//! in `Cargo.toml` and nobody thinks of the website — so this walks the same three files a
//! visitor's claims rest on: every protocol feature is on the page, nothing on the page is a
//! feature that no longer exists, and the highlighted set is exactly
//! `crates/netget-web/Cargo.toml`'s. Whole-tree source scan, so it holds at any feature set.
//!
//! To update the page, edit the table by hand: put the new feature in the row it belongs to,
//! wrapped in `<b class="in-web">` if it is in the browser build. This test says what is
//! missing.

use std::collections::BTreeSet;
use std::fs;

/// Features that are not protocols: aggregates, build switches, the test-only feature, the
/// shared USB base, and the runtime facilities. Adding a protocol never touches this list;
/// adding a new *kind* of non-protocol feature does, deliberately.
const NOT_PROTOCOLS: &[&str] = &[
    "default",
    "terminal-snapshot",
    "portable-base",
    "dist",
    "dist-darwin",
    "dist-windows",
    "all-protocols",
    "gpu",
    "android-termux",
    "mcp-stdio",
    "mcp-http",
    "tun",
    "sha1",
    "usb-common",
    "sqlite",
    "embedded-llm",
];

fn feature_names(manifest: &str) -> BTreeSet<String> {
    let start = manifest.find("[features]").expect("[features] table");
    let end = manifest[start..]
        .find("\n[dependencies]")
        .map(|i| start + i)
        .expect("[dependencies] after [features]");
    manifest[start..end]
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once('=')?;
            let name = name.trim();
            if name.is_empty()
                || name.starts_with('#')
                || name.starts_with('[')
                || !rest.trim_start().starts_with('[')
            {
                return None;
            }
            if name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
            {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

fn protocol_features() -> BTreeSet<String> {
    let manifest = fs::read_to_string("Cargo.toml").expect("Cargo.toml");
    feature_names(&manifest)
        .into_iter()
        .filter(|n| !NOT_PROTOCOLS.contains(&n.as_str()))
        .collect()
}

fn browser_features() -> BTreeSet<String> {
    let manifest =
        fs::read_to_string("crates/netget-web/Cargo.toml").expect("crates/netget-web/Cargo.toml");
    manifest
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            let t = t.strip_prefix('"')?;
            let t = t.strip_suffix("\",")?;
            Some(t.to_string())
        })
        .collect()
}

/// (all tokens on the page, tokens marked as in the browser demo)
fn page_tokens() -> (BTreeSet<String>, BTreeSet<String>) {
    let page = fs::read_to_string("site/index.html").expect("site/index.html");
    let mut all = BTreeSet::new();
    let mut marked = BTreeSet::new();
    let mut rest = page.as_str();
    while let Some(i) = rest.find("<span class=\"proto-list\">") {
        let after = &rest[i + "<span class=\"proto-list\">".len()..];
        let end = after.find("</span>").expect("proto-list closes");
        let inner = &after[..end];
        // Marked tokens are `<b class="in-web" ...>name</b>`; the rest are bare words.
        let mut text = String::new();
        let mut cursor = inner;
        while let Some(b) = cursor.find("<b class=\"in-web\"") {
            text.push_str(&cursor[..b]);
            text.push(' ');
            let open_end = cursor[b..].find('>').expect("<b> closes") + b + 1;
            let close = cursor[open_end..].find("</b>").expect("</b>") + open_end;
            let name = cursor[open_end..close].trim().to_string();
            marked.insert(name.clone());
            text.push_str(&name);
            text.push(' ');
            cursor = &cursor[close + "</b>".len()..];
        }
        text.push_str(cursor);
        for tok in text.split_whitespace() {
            all.insert(tok.to_string());
        }
        rest = &after[end..];
    }
    (all, marked)
}

#[test]
fn every_protocol_feature_is_on_the_landing_page_and_nothing_else_is() {
    let features = protocol_features();
    let (on_page, _) = page_tokens();
    assert!(
        features.len() > 100,
        "feature scan found only {}",
        features.len()
    );
    let missing: Vec<_> = features.difference(&on_page).cloned().collect();
    let stale: Vec<_> = on_page.difference(&features).cloned().collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "site/index.html's protocol table drifted from Cargo.toml.\n  missing from the page: {missing:?}\n  on the page but not a feature: {stale:?}\n\
         Add each missing feature to the fitting row of the table (or to NOT_PROTOCOLS in this test if it is not a protocol), and remove stale ones."
    );
}

#[test]
fn the_highlighted_protocols_are_exactly_the_browser_build() {
    let web = browser_features();
    let (_, marked) = page_tokens();
    assert!(
        web.len() > 10,
        "browser feature scan found only {}",
        web.len()
    );
    let unmarked: Vec<_> = web.difference(&marked).cloned().collect();
    let over: Vec<_> = marked.difference(&web).cloned().collect();
    assert!(
        unmarked.is_empty() && over.is_empty(),
        "the page's browser-demo highlights drifted from crates/netget-web/Cargo.toml.\n  in the browser build but not highlighted: {unmarked:?}\n  highlighted but not in the browser build: {over:?}"
    );
}

#[test]
fn the_headline_count_is_not_an_overclaim() {
    let page = fs::read_to_string("site/index.html").expect("site/index.html");
    let n = protocol_features().len();
    // The hero says "150+"; it must stay at or below the real number.
    let claim = page
        .split("behind ")
        .filter_map(|s| {
            s.split_once('+')
                .map(|(num, _)| num.trim().parse::<usize>().ok())
                .flatten()
        })
        .next()
        .expect("the hero claims a count like \"150+\"");
    assert!(
        claim <= n,
        "the page claims {claim}+ protocols but Cargo.toml has {n} protocol features"
    );
    assert!(
        page.contains(&format!("{n} protocol features")),
        "the Protocols section should state the exact count ({n} protocol features)"
    );
}
