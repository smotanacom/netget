//! A declared startup parameter that nothing reads is a promise the protocol does not keep.
//!
//! `get_startup_parameters()` is the model's *and* the operator's menu: the dashboard builds a
//! form field for every entry, the LLM is shown every entry as something it may pass, and
//! `StartupParams` validates against exactly this list. A parameter that is declared and never
//! read is therefore not dead code in the harmless sense — it is an advertised knob that
//! silently does nothing when turned.
//!
//! `imap`'s `use_tls` is the case that shows why this is worth a build failure rather than a
//! lint. It is declared, and `src/client/imap/CLAUDE.md` documents it twice ("Upgrade to TLS if
//! port 993 or `use_tls=true`", "`use_tls` (optional) - Enable TLS"). Nothing in
//! `src/client/imap/` reads it. Someone asking for TLS gets plaintext, and both the parameter
//! list and the documentation tell them otherwise.
//!
//! # The rule is deliberately conservative
//!
//! A parameter counts as dead only when its name **appears nowhere in the protocol's own
//! directory outside the `get_startup_parameters()` body that declares it**.
//!
//! That is much weaker than "is passed to a `StartupParams` accessor", and intentionally so. A
//! stricter first version flagged 57 client parameters, and spot-checking found real false
//! positives immediately: `s3` reads `access_key_id` through `get_protocol_field`, not through
//! `params.get_optional_string`, and several protocols hand the whole params object to a
//! helper. For a check that fails the build, a false positive is worse than a miss — it trains
//! people to edit the baseline instead of the code. If the name appears literally nowhere else,
//! there is no accessor, no helper and no indirection that could be reading it.
//!
//! **The baseline may only shrink.** Wire the parameter up, or delete it and the documentation
//! that describes it, then remove the line here.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test startup_param_drift_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// `role:protocol:parameter` for every declared parameter nothing reads.
///
/// **Empty, and it may only stay that way.** It once held twenty entries. The three shapes the
/// fixes took are worth knowing, because a future entry will be one of them:
///
/// * **Wired up** — the parameter now does what it says. `default_headers` (`http`, `http2`,
///   `http3`, `jsonrpc`, `webdav`) is merged into every request *underneath* the headers the
///   model sets; credentials (`elasticsearch:username`/`password`, `webdav:auth`,
///   `http_proxy:proxy_auth`) become the `Authorization` / `Proxy-Authorization` header they
///   describe; `elasticsearch:default_index`, `http_proxy:default_target`,
///   `kubernetes:kubeconfig`, `mcp:client_name`/`client_version`, `xmlrpc:timeout_secs`,
///   `dc:hub_topic` and `openidconnect:flow` are each read at the one place that can honour
///   them.
/// * **Deleted** — `http3:enable_0rtt`. The client builds a fresh QUIC endpoint per request
///   and keeps no session-ticket cache, so there is never a session to resume; 0-RTT could
///   not have happened whatever the flag said. Deleting the declaration and the docs that
///   described it is the honest fix, not leaving it declared with a TODO.
/// * **Refused** — `client:pop3:use_tls`. This client has no TLS at all and a POP3 session's
///   next move is `USER`/`PASS`, so `use_tls: true` returns an `Err` naming the reason rather
///   than producing a cleartext session that reports itself as encrypted. `imap` set that
///   precedent; the note that pop3 "cannot be fixed locally because `connect` does not
///   receive startup parameters" was wrong — the parameter list is per-protocol, so adding
///   `ctx.startup_params` to the call was the whole fix. `server:nntp:send_first` was the last
///   entry and took this exit: NNTP's greeting is mandatory (RFC 3977 5.1), so the parameter
///   could never have been honoured in the `false` direction and declaring it was the bug.
/// # These five did not appear because the code got worse — the check got stronger
///
/// The scan above used to treat a name appearing in `get_startup_examples()` as proof that
/// something read it. It is the opposite: the examples exist to teach the model to *pass* the
/// parameter, so they guarantee the name appears in the file whether or not anything consumes
/// it. Excluding the examples body — September 2026 — immediately surfaced five parameters
/// that had been dead the whole time and invisible for that exact reason.
///
/// **So this baseline is not new debt. It is old debt becoming visible**, and it may only
/// shrink, like every other entry above did.
const DEAD_PARAM_BASELINE: &[&str] = &[
    // bitcoind's RPC is auth-mandatory and this client sets no `Authorization` header
    // anywhere, so authenticated Bitcoin Core RPC cannot work at all.
    "client:bitcoin:rpc_password",
    "client:bitcoin:rpc_user",
    // The printer path cannot be set; the client uses its own constant.
    "client:ipp:printer_path",
    // Passes `ctx.remote_addr` as the interface, so the declared knob is a dashboard field
    // that does nothing. (`arp` looks identical but genuinely reads its own.)
    "client:isis:interface",
    "server:bluetooth_ble:auto_advertise",
];

fn strip_comments(src: &str) -> String {
    src.lines()
        .map(|l| match l.find("//") {
            Some(i) => &l[..i],
            None => l,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Index just past the `}` matching the `{` at `start`.
fn balanced(src: &str, start: usize) -> Option<usize> {
    let b = src.as_bytes();
    let mut depth = 0i32;
    for (i, byte) in b.iter().enumerate().skip(start) {
        if *byte == b'{' {
            depth += 1;
        } else if *byte == b'}' {
            depth -= 1;
            if depth == 0 {
                return Some(i + 1);
            }
        }
    }
    None
}

fn actions_files(root: &Path) -> Vec<(String, PathBuf)> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, PathBuf)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.file_name().is_some_and(|f| f == "actions.rs") {
                let parent = p.parent().unwrap();
                out.push((
                    parent
                        .strip_prefix(root)
                        .unwrap_or(parent)
                        .to_string_lossy()
                        .to_string(),
                    p.clone(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

fn read_rs_recursively(dir: &Path, skip: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            read_rs_recursively(&p, skip, out);
        } else if p.extension().is_some_and(|e| e == "rs") && p != skip {
            out.push_str(&strip_comments(
                &std::fs::read_to_string(&p).unwrap_or_default(),
            ));
            out.push('\n');
        }
    }
}

/// Declared parameters that appear nowhere else in the protocol's directory.
fn dead_params(root: &Path, role: &str) -> BTreeSet<String> {
    let mut dead = BTreeSet::new();
    for (protocol, actions_path) in actions_files(root) {
        let src = strip_comments(&std::fs::read_to_string(&actions_path).unwrap_or_default());
        let Some(decl) = src.find("fn get_startup_parameters") else {
            continue;
        };
        let Some(open) = src[decl..].find('{').map(|i| decl + i) else {
            continue;
        };
        let Some(close) = balanced(&src, open) else {
            continue;
        };
        let body = &src[open..close];

        // Everything the protocol contains *except* the declaration body itself: the rest of
        // actions.rs on both sides, plus every other .rs file in the directory.
        let mut elsewhere = String::new();
        elsewhere.push_str(&src[..open]);
        elsewhere.push_str(&src[close..]);
        read_rs_recursively(
            actions_path.parent().unwrap(),
            &actions_path,
            &mut elsewhere,
        );

        // ...and *except* `get_startup_examples()`, which was the hole in this check.
        //
        // The conservative rule above exempts a parameter whose name appears anywhere else in
        // the directory, so that a name read through a helper is not flagged. But
        // `get_startup_examples()` puts every parameter name into a JSON example, in this same
        // file. So the examples whose whole purpose is teaching the model to pass a parameter
        // were what hid the fact that nothing reads it — the check was being defeated by the
        // documentation of the thing it checks.
        //
        // `client/bitcoin` is the proof: it declares `rpc_user` and `rpc_password`, has no
        // `Authorization` handling anywhere, and bitcoind's RPC is auth-mandatory, so
        // authenticated Bitcoin Core RPC cannot work — and this test was silent about it
        // because both names appear in a usage example.
        if let Some(ex) = src.find("fn get_startup_examples") {
            if let Some(ex_open) = src[ex..].find('{').map(|i| ex + i) {
                if let Some(ex_close) = balanced(&src, ex_open) {
                    let examples = &src[ex_open..ex_close];
                    elsewhere = elsewhere.replace(examples, "");
                }
            }
        }

        let mut from = 0usize;
        while let Some(rel) = body[from..].find("name:") {
            let at = from + rel + "name:".len();
            from = at;
            let tail = body[at..].trim_start();
            let Some(rest) = tail.strip_prefix('"') else {
                continue; // a constant or an expression, not a literal — skip it
            };
            let Some(end) = rest.find('"') else { continue };
            let param = &rest[..end];
            if param.is_empty() {
                continue;
            }
            if !elsewhere.contains(&format!("\"{param}\"")) {
                dead.insert(format!("{role}:{protocol}:{param}"));
            }
        }
    }
    dead
}

#[test]
fn no_new_declared_startup_parameter_goes_unread() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = dead_params(&manifest.join("src/server"), "server");
    found.extend(dead_params(&manifest.join("src/client"), "client"));

    let found: BTreeSet<&str> = found.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = DEAD_PARAM_BASELINE.iter().copied().collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these startup parameters are declared and their name appears nowhere else in the \
         protocol, so nothing can be reading them: {new:?}\n\
         `get_startup_parameters()` is the menu shown to the model and rendered as a form field \
         for the operator, so an unread entry is an advertised knob that does nothing when \
         turned. Read it, or delete it and the documentation that describes it."
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these parameters are no longer dead — remove them from DEAD_PARAM_BASELINE so the \
         ratchet keeps its grip: {fixed:?}"
    );
}

/// The scan must be reading real declarations, not silently finding none.
#[test]
fn the_scan_finds_startup_parameters_to_check() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    for (root, min_files) in [("src/server", 100usize), ("src/client", 80)] {
        let files = actions_files(&manifest.join(root));
        assert!(
            files.len() >= min_files,
            "{root}: found {} actions.rs files, expected at least {min_files}",
            files.len()
        );
        let declaring = files
            .iter()
            .filter(|(_, p)| {
                std::fs::read_to_string(p)
                    .unwrap_or_default()
                    .contains("fn get_startup_parameters")
            })
            .count();
        assert!(
            declaring >= 50,
            "{root}: only {declaring} protocols declare startup parameters — the drift scan may \
             be looking at the wrong thing"
        );
    }
}
