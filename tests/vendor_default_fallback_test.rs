//! A client that loses its target must fail, never fall back to the real service.
//!
//! The DynamoDB client took its address as `_remote_addr` and dropped it. With no explicit
//! `endpoint_url` the AWS SDK's own resolver then produced `https://dynamodb.<region>
//! .amazonaws.com` and signed the request with whatever ambient credentials the machine had —
//! so **a client the operator pointed at localhost issued real reads and writes against real
//! AWS**, and that was the shape of the protocol's own startup examples.
//!
//! It happened three times, with three different SDKs, which is what makes it a class rather
//! than a bug:
//!
//! * **`dynamodb`** — `_remote_addr`, as above.
//! * **`openai`** — the config was left at `async-openai`'s default unless `remote_addr` was
//!   both non-empty *and* not literally `https://api.openai.com/v1`, so an unrecorded address
//!   meant the vendor endpoint signed with the operator's real key.
//! * **`openapi`** — the base URL came from the **model-supplied spec**'s `servers[0]`, which
//!   won over the address the operator typed.
//!
//! The common cause is not carelessness: it is that **a vendor SDK's defaults point at
//! production**, and a target that merely fails to arrive is indistinguishable, inside the SDK,
//! from one the caller deliberately omitted. Nothing errors. Nothing logs. The traffic just
//! goes somewhere else, correctly signed.
//!
//! # The rule, in two limbs
//!
//! **Limb A — a vendor SDK must be told where to go.** A `src/client/<p>/mod.rs` that mentions
//! any of [`VENDOR_SDKS`] is flagged if its `connect` takes the target as `_remote_addr`, or if
//! `remote_addr` never appears outside the signature. Those are the two spellings of "dropped
//! on the floor"; the SDK then resolves its own endpoint. Limb A deliberately does **not**
//! include `reqwest::Client`: a plain HTTP client resolves nothing on its own, the URL is built
//! by hand, and including it would put 13 clients in front of a rule that has nothing to say
//! about them.
//!
//! **Limb B — no vendor hostname may be a fallback target.** A string literal containing a host
//! from [`VENDOR_HOSTS`] is flagged when it stands in a *target-producing* position: `return
//! "…"`, `= "…"`, `unwrap_or("…")`, `or_else(|| "…")`, a match arm. A vendor host inside an
//! `anyhow!` message, a parameter `description`, a `json!` example or a log line is not
//! flagged, and that distinction is the whole rule — `openai/mod.rs` names `api.openai.com`
//! four times, every one of them in the error that *refuses* to fall back to it, which is
//! precisely the fix.
//!
//! # A trap this rule walked into, worth not repeating
//!
//! Limb B found nothing at first, because the scanner stripped `//` comments line-wise and
//! every vendor default is an `https://` URL — so the line was truncated at the `//` inside its
//! own literal and the hit was erased. [`strip_comments`] here is string-aware for that reason.
//! A comment stripper that does not know about string literals silently blinds any scan whose
//! subject is a URL.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test vendor_default_fallback_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `limb:protocol:detail` for every client that can reach a vendor instead of its target.
///
/// **`A:sqs:remote_addr dropped as _remote_addr` — this is a LIVE DEFECT, not accepted debt.**
/// It is the DynamoDB bug unfixed, in the next AWS client along: `SqsClient::
/// connect_with_llm_actions` takes `_remote_addr: String` and reads it nowhere, so unless the
/// caller happens to know to pass the undocumented-by-`remote_addr` `endpoint_url` startup
/// parameter, `aws_sdk_sqs` resolves `https://sqs.<region>.amazonaws.com`. Its credential
/// handling compounds it: the comment above `config_loader.load().await` says explicit
/// credentials are used "when the caller supplied them", but when they were not, that same line
/// runs the SDK's default chain — environment, `~/.aws/credentials`, then IMDS. An operator who
/// starts an SQS client against `127.0.0.1:4566` gets real AWS, signed as themselves. The fix is
/// the one `dynamodb` already carries: un-underscore the parameter and add
/// `let endpoint_url = endpoint_url.or_else(|| endpoint_url_from_remote_addr(&remote_addr));`
/// before the builder. It is entered here so the ratchet can hold the line while that lands,
/// and it must be the first entry removed.
///
/// **`B:npm` and `B:pypi` — deliberate, documented, and still the class.** Each returns the
/// public registry (`https://registry.npmjs.org`, `https://pypi.org`) when `remote_addr` is
/// empty, logging an INFO as it does so. They are much milder than the AWS case — no
/// credentials are signed and an address the operator *did* type always wins, which is itself a
/// fix that landed earlier — but an empty target still reaches the real service. `openai` faced
/// the identical choice and took the other exit, refusing with a message naming both the
/// vendor URL and a localhost example; that is the shape to copy if these are ever changed.
// `A:sqs:target dropped` was fixed on 15 Sep 2026 rather than re-baselined. It was the
// DynamoDB defect verbatim, in the next AWS client along: `_remote_addr` was read nowhere, so
// without an explicit `endpoint_url` the SDK resolved `https://sqs.<region>.amazonaws.com` and
// signed with whatever ambient credentials the machine had - a client the operator pointed at
// localhost issuing real queue operations against real AWS. This ratchet is the reason there
// will not be a third.
const VENDOR_FALLBACK_BASELINE: &[&str] = &["B:npm:registry.npmjs.org", "B:pypi:pypi.org"];

/// SDKs that resolve an endpoint of their own when none is configured.
///
/// Membership is decided by one question: *if I build this client and set no endpoint, does it
/// still know where to send a request?* For the AWS SDK, `async-openai`, the Azure and Google
/// clients and `octocrab` the answer is yes, and the place it sends is production. For
/// `reqwest` it is no — there is no request until someone supplies a URL — which is why it is
/// absent even though 13 clients use it.
const VENDOR_SDKS: &[&str] = &[
    "aws_config",
    "aws_sdk_",
    "aws_types",
    "aws_credential_types",
    "async_openai",
    "OpenAIConfig",
    "azure_core",
    "azure_identity",
    "google_cloud",
    "octocrab",
];

/// Hostnames that are somebody's production service.
const VENDOR_HOSTS: &[&str] = &[
    "api.openai.com",
    "amazonaws.com",
    "googleapis.com",
    "azure.com",
    "core.windows.net",
    "api.github.com",
    "registry.npmjs.org",
    "pypi.org",
    "hub.docker.com",
    "registry-1.docker.io",
    "api.anthropic.com",
    "slack.com",
    "api.twilio.com",
    "cloudflare.com",
    "quay.io",
    "gcr.io",
    "repo.maven.apache.org",
    "crates.io",
];

/// Text immediately before a literal that makes the literal *the value of an expression*.
///
/// This is what separates a fallback target from the four mentions of `api.openai.com` in
/// `openai/mod.rs`, all of which sit inside `anyhow!(…)` and say the client is refusing.
const TARGET_PRODUCING_LEAD: &[&str] = &[
    "return ",
    "=> ",
    "unwrap_or(",
    "unwrap_or_else(|| ",
    "or_else(|| ",
    "= ",
];

/// Text immediately after it, so a literal that is merely the first argument of a call
/// (`.info("…", x)`, `format!("…", y)`) is not mistaken for a returned value.
const TARGET_PRODUCING_TRAIL: &[&str] =
    &[";", ",", ")", ".to_string()", ".to_owned()", ".into()", "}"];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

/// Remove `//` comments **without** cutting inside a string literal.
///
/// The line-wise `split("//")[0]` version every other ratchet here uses is wrong for this one:
/// every vendor default is an `https://` URL, so it truncated each hit at the `//` in its own
/// literal and limb B reported nothing at all.
fn strip_comments(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_string = false;
    while i < b.len() {
        let c = b[i];
        if in_string {
            if c == '\\' && i + 1 < b.len() {
                // Blank both halves, but keep a newline: Rust's line continuation (a `\`
                // at end of line inside a literal) is an escape whose second character IS
                // the newline, and swallowing it silently shifted every line number below
                // it — nine of them in `ipp/actions.rs` alone.
                out.push(' ');
                out.push(if b[i + 1] == '\n' { '\n' } else { ' ' });
                i += 2;
                continue;
            }
            if c == '"' {
                in_string = false;
            }
            out.push(c);
            i += 1;
            continue;
        }
        // A char literal may *be* a quote (`'\"'`), and reading it as the start of a
        // string inverts the quote parity for the rest of the file. Lifetimes (`'a`) are
        // left alone, which is why the closing `'` has to be where a char literal puts it.
        if c == '\'' {
            let simple = i + 2 < b.len() && b[i + 2] == '\'';
            let escaped = i + 3 < b.len() && b[i + 1] == '\\' && b[i + 3] == '\'';
            if simple || escaped {
                let n = if simple { 3 } else { 4 };
                for k in 0..n {
                    out.push(if b[i + k] == '\n' { '\n' } else { ' ' });
                }
                i += n;
                continue;
            }
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < b.len() && b[i + 1] == '/' {
            while i < b.len() && b[i] != '\n' {
                out.push(' ');
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn contains_word(hay: &str, needle: &str) -> bool {
    let h: Vec<char> = hay.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || n.len() > h.len() {
        return false;
    }
    (0..=h.len() - n.len()).any(|i| {
        h[i..i + n.len()] == n[..]
            && (i == 0 || !is_ident_char(h[i - 1]))
            && (i + n.len() >= h.len() || !is_ident_char(h[i + n.len()]))
    })
}

fn count_word(hay: &str, needle: &str) -> usize {
    let h: Vec<char> = hay.chars().collect();
    let n: Vec<char> = needle.chars().collect();
    if n.is_empty() || n.len() > h.len() {
        return 0;
    }
    (0..=h.len() - n.len())
        .filter(|&i| {
            h[i..i + n.len()] == n[..]
                && (i == 0 || !is_ident_char(h[i - 1]))
                && (i + n.len() >= h.len() || !is_ident_char(h[i + n.len()]))
        })
        .count()
}

/// Limb A: does this client wrap a vendor SDK, and does it read its target?
fn limb_a_drops_target(src: &str) -> bool {
    if !VENDOR_SDKS.iter().any(|s| src.contains(s)) {
        return false;
    }
    // `_remote_addr` is the explicit "I am ignoring this" spelling, and it is the one the tree
    // actually uses, because rustc's unused-variable warning pushes you there. The count check
    // catches the other way in: `<= 1` means the name occurs only in the signature, so nothing
    // reads it. It cannot be `== 0` — that can only ever match the `_`-prefixed spelling, and
    // a named-but-unused parameter would slip through.
    contains_word(src, "_remote_addr") || count_word(src, "remote_addr") <= 1
}

/// Limb B: vendor hostnames sitting where the target is produced.
fn limb_b_vendor_targets(src: &str) -> Vec<(usize, String)> {
    let b: Vec<char> = src.chars().collect();
    let mut line_of = Vec::with_capacity(b.len() + 1);
    let mut line = 1usize;
    for &c in &b {
        line_of.push(line);
        if c == '\n' {
            line += 1;
        }
    }
    line_of.push(line);

    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != '"' {
            i += 1;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && b[j] != '"' && b[j] != '\n' {
            j += 1;
        }
        if j >= b.len() || b[j] == '\n' {
            i += 1;
            continue;
        }
        let literal: String = b[i + 1..j].iter().collect();
        if let Some(host) = VENDOR_HOSTS.iter().find(|h| literal.contains(**h)) {
            let before: String = b[..i].iter().rev().take(24).collect::<String>();
            let before: String = before.chars().rev().collect();
            let after: String = b[j + 1..].iter().take(16).collect();
            let after = after.trim_start();
            let leads = TARGET_PRODUCING_LEAD.iter().any(|l| before.ends_with(l));
            let trails = TARGET_PRODUCING_TRAIL.iter().any(|t| after.starts_with(t));
            if leads && trails {
                out.push((line_of[i], (*host).to_string()));
            }
        }
        i = j + 1;
    }
    out
}

fn rust_files(root: &Path) -> Vec<(String, String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let parent = p.parent().unwrap();
                out.push((
                    parent
                        .strip_prefix(root)
                        .unwrap_or(parent)
                        .to_string_lossy()
                        .to_string(),
                    p.file_name().unwrap().to_string_lossy().to_string(),
                    p,
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn survey() -> BTreeSet<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeSet::new();

    // Limb A is about clients: a server never has a "target" to lose.
    for (protocol, file, path) in rust_files(&manifest.join("src/client")) {
        if file != "mod.rs" || protocol.is_empty() {
            continue;
        }
        let src = strip_comments(&std::fs::read_to_string(&path).unwrap_or_default());
        if limb_a_drops_target(&src) {
            found.insert(format!("A:{protocol}:target dropped"));
        }
    }

    // Limb B applies to both trees — a server hardcoding a vendor as its upstream would be the
    // same mistake pointing the other way.
    for dir in ["src/server", "src/client"] {
        for (protocol, _file, path) in rust_files(&manifest.join(dir)) {
            if protocol.is_empty() {
                continue;
            }
            let src = strip_comments(&std::fs::read_to_string(&path).unwrap_or_default());
            for (_line, host) in limb_b_vendor_targets(&src) {
                let leaf = protocol.rsplit('/').next().unwrap_or(&protocol);
                found.insert(format!("B:{leaf}:{host}"));
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

#[test]
fn no_client_can_reach_a_vendor_instead_of_its_target() {
    let found = survey();
    let found_refs: BTreeSet<&str> = found.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = VENDOR_FALLBACK_BASELINE.iter().copied().collect();

    let new: Vec<_> = found_refs.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these can send NetGet's traffic to somebody's production service instead of the \
         address the operator named: {new:?}\n\
         `A:` means a client wrapping a vendor SDK that never reads `remote_addr` — the SDK \
         then resolves its own endpoint and signs with whatever ambient credentials exist, \
         which is how the DynamoDB client issued real reads and writes against real AWS from a \
         localhost instruction. `B:` means a vendor hostname standing where the target is \
         produced. Read the target; if it is missing, refuse to connect. Do not let a library's \
         fallback decide where the traffic goes, and put the vendor hostname only in the error \
         that declines to use it — `openai/mod.rs::api_base_for` is the worked example."
    );

    let fixed: Vec<_> = baseline.difference(&found_refs).copied().collect();
    assert!(
        fixed.is_empty(),
        "these no longer reach a vendor — remove them from VENDOR_FALLBACK_BASELINE: {fixed:?}"
    );
}

/// A scan that matches nothing passes forever.
#[test]
fn the_scan_is_reading_real_source() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let clients = rust_files(&manifest.join("src/client"));
    assert!(
        clients.iter().filter(|(_, f, _)| f == "mod.rs").count() >= 80,
        "expected at least 80 client modules, found {}",
        clients.iter().filter(|(_, f, _)| f == "mod.rs").count()
    );

    // Limb A has to actually find the SDK-wrapping clients, or it is checking an empty set.
    let wrapping = clients
        .iter()
        .filter(|(p, f, path)| {
            f == "mod.rs" && !p.is_empty() && {
                let src = strip_comments(&std::fs::read_to_string(path).unwrap_or_default());
                VENDOR_SDKS.iter().any(|s| src.contains(s))
            }
        })
        .count();
    assert!(
        wrapping >= 3,
        "only {wrapping} clients wrap a vendor SDK; there were four (dynamodb, openai, s3, \
         sqs) when this was written, so a lower number means VENDOR_SDKS stopped matching"
    );
}

/// What the rule does on inputs whose right answer is known.
///
/// Cases 1–3 are the three historical defects. Cases 4–7 are the fixes and the near misses,
/// and every one of them is a shape this scan reported or missed while it was being written.
#[test]
fn the_rule_flags_the_historical_defects_and_not_their_fixes() {
    // 1. `dynamodb`, verbatim: the target taken and dropped.
    assert!(
        limb_a_drops_target(
            "use aws_config::BehaviorVersion;\n\
             pub async fn connect(_remote_addr: String) -> Result<()> { Ok(()) }"
        ),
        "an SDK client whose target is `_remote_addr` must be flagged"
    );

    // 2. The same thing by accident: the parameter is named, and never read.
    assert!(
        limb_a_drops_target(
            "use aws_sdk_sqs::Client;\n\
             pub async fn connect(remote_addr: String) -> Result<()> { Ok(()) }"
        ),
        "a target that is never read is dropped just as thoroughly as one that is `_`-prefixed"
    );

    // 3. `openai` / `openapi`: the vendor host as a fallback value.
    assert_eq!(
        limb_b_vendor_targets(
            "fn base() -> String { return \"https://api.openai.com/v1\".to_string(); }"
        )
        .len(),
        1,
        "a vendor host returned when the target is missing must be flagged"
    );
    assert_eq!(
        limb_b_vendor_targets(
            "fn base(a: Option<&str>) -> &str { a.unwrap_or(\"https://pypi.org\") }"
        )
        .len(),
        1,
        "`unwrap_or(\"<vendor>\")` is the same fallback wearing a combinator"
    );

    // 4. The fix: the target is read, and the endpoint derives from it.
    assert!(
        !limb_a_drops_target(
            "use aws_config::BehaviorVersion;\n\
             pub async fn connect(remote_addr: String) -> Result<()> {\n\
             let endpoint = endpoint_url.or_else(|| endpoint_from(&remote_addr));\n\
             Ok(()) }"
        ),
        "reading the target is the fix and must not be reported"
    );

    // 5. The fix for limb B: the vendor host appears only in the refusal. This is exactly
    //    `openai/mod.rs::api_base_for`, which names `api.openai.com` four times.
    assert!(
        limb_b_vendor_targets(
            "fn base(a: &str) -> Result<String> {\n\
             if a.is_empty() { anyhow::bail!(\"needs an endpoint (e.g. https://api.openai.com/v1 \
             or http://127.0.0.1:8080/v1); refusing to fall back to api.openai.com\"); }\n\
             Ok(a.to_string()) }"
        )
        .is_empty(),
        "a vendor host inside the error that declines to use it is the fix, not the defect"
    );

    // 6. Descriptions, examples and log lines mention vendor hosts constantly — `npm` and
    //    `pypi`'s `actions.rs` alone have eleven between them.
    for body in [
        "fn d() { let p = Param { description: \"NPM registry URL (default: https://registry.npmjs.org)\".to_string() }; }",
        "fn e() { json!({ \"remote_addr\": \"pypi.org\" }); }",
        "fn l() { log.info(\"defaulting to https://pypi.org\"); }",
    ] {
        assert!(
            limb_b_vendor_targets(body).is_empty(),
            "a vendor host that is documentation, not a target: {body}"
        );
    }

    // 7. `reqwest` is not a vendor SDK. Thirteen clients build one and every URL they use is
    //    assembled by hand from the target; including it would drown limb A.
    assert!(
        !limb_a_drops_target(
            "use reqwest::Client;\n pub async fn connect(_remote_addr: String) {}"
        ),
        "a plain HTTP client resolves no endpoint of its own and is not this class"
    );

    // 8. The comment stripper must not cut a line at the `//` inside its own URL — limb B
    //    reported zero hits across the whole tree until this was fixed.
    assert!(
        strip_comments("let u = \"https://pypi.org\"; // a comment").contains("https://pypi.org"),
        "stripping comments must not truncate a string literal containing `//`"
    );
    assert!(
        !strip_comments("let u = \"ok\"; // https://pypi.org").contains("pypi.org"),
        "a vendor host inside an actual comment must still be removed"
    );
}
