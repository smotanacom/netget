//! The landing page's protocol table is derived from `Cargo.toml`, and this keeps it that way.
//!
//! `site/index.html` lists every protocol feature in a hand-grouped table. It drifts
//! silently — a protocol lands in `Cargo.toml` and nobody thinks of the website — so this
//! walks both files a visitor's claims rest on: every protocol feature is on the page, and
//! nothing on the page is a feature that no longer exists. Whole-tree source scan, so it
//! holds at any feature set.
//!
//! To update the page, edit the table by hand: put the new feature in the row it belongs to.
//! This test says what is missing.
//!
//! The same number is the headline: every "<N> network protocols" / "<N> protocol features"
//! on the page and in the README must be exactly the count of protocol features, and a
//! rounded-down "150+" is a failure that names the line to fix.

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

/// Every token in the page's protocol table.
fn page_tokens() -> BTreeSet<String> {
    let page = fs::read_to_string("site/index.html").expect("site/index.html");
    let mut all = BTreeSet::new();
    let mut rest = page.as_str();
    while let Some(i) = rest.find("<span class=\"proto-list\">") {
        let after = &rest[i + "<span class=\"proto-list\">".len()..];
        let end = after.find("</span>").expect("proto-list closes");
        let inner = &after[..end];
        assert!(
            !inner.contains('<'),
            "the protocol table holds bare names only, found markup: {inner}"
        );
        for tok in inner.split_whitespace() {
            all.insert(tok.to_string());
        }
        rest = &after[end..];
    }
    all
}

#[test]
fn every_protocol_feature_is_on_the_landing_page_and_nothing_else_is() {
    let features = protocol_features();
    let on_page = page_tokens();
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

/// Every protocol-count claim in `text`: a number, with or without a `+`, followed by
/// "protocol…" or "network protocol…". Returns (1-based line, the claim as written, the
/// number, whether it carried a `+`).
fn count_claims(text: &str) -> Vec<(usize, String, usize, bool)> {
    let mut claims = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let words: Vec<&str> = line.split_whitespace().collect();
        for (j, word) in words.iter().enumerate() {
            // A tag or an attribute can sit right before the number (`>179`, `"179`).
            let word = word.rsplit(['>', '"', '(']).next().unwrap_or(word);
            let (digits, plus) = match word.strip_suffix('+') {
                Some(d) => (d, true),
                None => (word, false),
            };
            let Ok(n) = digits.parse::<usize>() else {
                continue;
            };
            let next = words.get(j + 1).copied().unwrap_or("");
            let after = words.get(j + 2).copied().unwrap_or("");
            let names_protocols = next.starts_with("protocol")
                || (next == "network" && after.starts_with("protocol"));
            if names_protocols {
                let claim = if next == "network" {
                    format!("{word} {next} {after}")
                } else {
                    format!("{word} {next}")
                };
                claims.push((i + 1, claim, n, plus));
            }
        }
    }
    claims
}

/// The headline count is stated in four places — the page's meta description, its
/// og:description, its hero and its Protocols section, and the README's opening sentence —
/// and each must be the exact number of protocol features, not a rounded-down "150+".
#[test]
fn every_headline_count_is_the_exact_number() {
    let n = protocol_features().len();
    let mut problems = Vec::new();
    let mut seen = 0;
    for file in ["site/index.html", "README.md"] {
        let text = fs::read_to_string(file).expect(file);
        let claims = count_claims(&text);
        assert!(
            !claims.is_empty(),
            "{file} states no protocol count at all; the scan in this test has gone blind"
        );
        for (line, claim, count, plus) in claims {
            seen += 1;
            if plus || count != n {
                problems.push(format!(
                    "  {file}:{line}: \"{claim}\" — write the exact count, {n} (the protocol \
                     features in Cargo.toml), with no \"+\""
                ));
            }
        }
    }
    assert!(
        seen >= 5,
        "expected the count in at least five places (meta, og, hero, Protocols section, README), found {seen}"
    );
    assert!(
        problems.is_empty(),
        "a protocol count on the site or in the README is not the exact number ({n}):\n{}",
        problems.join("\n")
    );
}
