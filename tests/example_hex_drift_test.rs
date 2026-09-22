//! An example the model copies must be generated from the bytes, not transcribed from them.
//!
//! Four of the five BLE HID profiles shipped report descriptors that a real host rejects. In
//! **two of them the Rust const was correct and the example had drifted** — someone had pasted
//! the descriptor into the startup example, the const was later fixed, and the example was not.
//!
//! That asymmetry is what makes this worth a build gate rather than a review note. A wrong
//! const is a bug in NetGet, and NetGet's own tests can catch it. A wrong *example* is a bug in
//! what the model is told to do, and there is nothing downstream to catch it: the model copies
//! the example verbatim onto a real device, the host rejects the descriptor, and every layer
//! in between reports success. `hex::encode(HID_MOUSE_REPORT_DESCRIPTOR)` cannot drift; the
//! transcription always can.
//!
//! # The rule, in two limbs
//!
//! A "long hex literal" is a string literal of **more than 16 bytes** (over 32 hex characters)
//! that is entirely hex digits. Below that it is a MAC address, a short magic, an 8-byte
//! sequence number — things a person can read and check. Above it, nobody proofreads.
//!
//! **Limb A — no long hex literal in `get_startup_examples()`.** That body is the text the
//! model imitates when it starts a server, so a literal there is copied straight onto the wire.
//!
//! **Limb B — no long hex literal anywhere in an `actions.rs` whose protocol directory declares
//! a `const … : &[u8]`.** This is the HID case exactly: once the canonical bytes exist as a
//! const, any hex spelling of bytes in the same protocol is a second copy of something that
//! already has one home. The fix is `hex::encode(THE_CONST)`, which is what all five BLE
//! profiles now do.
//!
//! Limb B's baseline is **empty**, and that is the point of it: twelve protocols declare a
//! `&[u8]` const today (the five BLE HID profiles, `finger`, `git`, `rdp`, `tor_relay`,
//! `usb/smartcard`, `vnc`, and the `nfc` client) and not one of them has a long hex literal
//! beside it. The gate holds that line.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test example_hex_drift_test

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// More than this many bytes of hex in one literal and nobody is checking it by eye.
const MAX_INLINE_HEX_BYTES: usize = 16;

/// `limb:protocol:file:bytes` for every long hex literal in a model-facing example.
///
/// All five are *placeholders* rather than transcriptions — there is no const anywhere in their
/// protocol that they could have drifted from, which is why limb B does not see them and why
/// none is a live defect. They are recorded so that the sixth, which may well be a descriptor
/// somebody pasted, fails the build.
///
/// **`A:mercurial:actions.rs:20`** — `1234567890abcdef…`, a made-up 20-byte Mercurial changeset
/// id. Obviously synthetic, and harmless because Mercurial node ids are opaque.
///
/// **`A:torrent_dht:actions.rs:20`** (two sites) — `0123456789abcdef…`, a made-up 20-byte
/// BitTorrent node id / infohash. Same shape, same reasoning.
///
/// **`A:socks5:actions.rs:37` and `A:tor:actions.rs:37`** — the same 37 bytes in both, and the
/// one entry here that is arguably worth fixing: it is `GET / HTTP/1.1\r\nHost: example.com…`
/// hex-encoded, so the model is shown a request it cannot read, cannot adapt, and cannot check.
/// `CLAUDE.md`'s rule that raw bytes do not belong in action parameters applies to examples
/// too. Fix: express it as text with an `encoding: "utf8"` field, the way `tcp`'s
/// `send_tcp_data` settled the same question.
/// **Limb C's thirteen are UNREVIEWED, and that word is doing real work here.** They are not a
/// claim that each is fine; they are a claim that nobody has looked. Limb C was added on
/// 22 September 2026 and every one of these predates it — an `ActionDefinition`'s `example` was
/// outside the scan entirely, which is how a dozen went unnoticed while limbs A and B were
/// argued case by case.
///
/// They split into **three** kinds, and naming the third is most of what makes the review
/// tractable:
///
/// * **Opaque identifiers a caller supplies** — a 20-byte BitTorrent info hash or Mercurial
///   node id has nothing to drift *from*, and the reasoning already written above for
///   `A:torrent_dht` applies unchanged. `hls`, `nfc`, `mercurial`, `torrent_dht`,
///   `torrent_peer` and `torrent_tracker` look like this. Probably fine as they are.
/// * **A deliberate escape hatch, whose contract IS raw bytes** — `ntp`'s
///   `send_ntp_response` and `bitcoin`'s `send_bitcoin_message` both say so in their own
///   descriptions, and both sit *beside* a structured action (`send_ntp_time_response`,
///   `send_verack`) that the description tells the model to prefer. "Express it as structured
///   fields" is already done here; the hex action is the exit for what the structured one
///   cannot say. The remedy the failure message offers does not apply, and forcing it would
///   remove a capability.
/// * **Wire blobs with neither excuse** — `datalink`'s 42 bytes is an Ethernet frame,
///   `icmp`'s 28 an IP+ICMP packet, `ssh_agent`'s 22 and 23 agent messages. No const to build
///   from, no escape-hatch framing, and a model cannot proofread any of them. These are the
///   ones worth changing.
///
/// Shrinking this list means deciding which kind each is and acting on it. Removing a line
/// without doing that is how a baseline becomes a place to put things.
const LONG_HEX_EXAMPLE_BASELINE: &[&str] = &[
    "A:mercurial:actions.rs:20",
    "A:socks5:actions.rs:37",
    "A:tor:actions.rs:37",
    "A:torrent_dht:actions.rs:20",
    // Limb C, unreviewed — see above.
    "C:bitcoin:actions.rs:24",
    "C:bitcoin:actions.rs:32",
    "C:datalink:actions.rs:42",
    "C:hls:actions.rs:21",
    "C:icmp:actions.rs:28",
    "C:mercurial:actions.rs:20",
    "C:nfc:actions.rs:20",
    "C:ntp:actions.rs:56",
    "C:ssh_agent:actions.rs:22",
    "C:ssh_agent:actions.rs:23",
    "C:torrent_dht:actions.rs:20",
    "C:torrent_peer:actions.rs:20",
    "C:torrent_tracker:actions.rs:20",
];

/// Bodies whose contents the model reads as a template.
const MODEL_FACING_FNS: &[&str] = &[
    "get_startup_examples",
    "get_examples",
    "get_startup_parameters",
];

// ---------------------------------------------------------------------------
// Source scanning
// ---------------------------------------------------------------------------

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Remove `//` comments without cutting inside a string literal, preserving line count.
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

/// Every long hex string literal in `src`, as byte lengths.
fn long_hex_literals(src: &str) -> Vec<usize> {
    let b: Vec<char> = src.chars().collect();
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
        let lit: &[char] = &b[i + 1..j];
        if lit.len() > MAX_INLINE_HEX_BYTES * 2
            && lit.len() % 2 == 0
            && lit.iter().all(|c| c.is_ascii_hexdigit())
        {
            out.push(lit.len() / 2);
        }
        i = j + 1;
    }
    out
}

/// The braced body of every `fn <name>` in `src`.
///
/// Every occurrence, not just the first: a protocol with more than one `impl` block would
/// otherwise have all but one of its examples unscanned.
fn bodies_of(src: &str, name: &str) -> Vec<String> {
    let b: Vec<char> = src.chars().collect();
    let needle: Vec<char> = format!("fn {name}").chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + needle.len() < b.len() {
        if b[i..i + needle.len()] != needle[..]
            || (i > 0 && is_ident_char(b[i - 1]))
            || is_ident_char(b[i + needle.len()])
        {
            i += 1;
            continue;
        }
        let mut j = i + needle.len();
        while j < b.len() && b[j] != '{' && b[j] != ';' {
            j += 1;
        }
        if j >= b.len() || b[j] == ';' {
            i += needle.len();
            continue;
        }
        let mut depth = 0i32;
        let mut k = j;
        while k < b.len() {
            if b[k] == '{' {
                depth += 1;
            } else if b[k] == '}' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            k += 1;
        }
        out.push(b[j..k.min(b.len())].iter().collect::<String>());
        i = k;
    }
    out
}

/// Does `src` declare a `const NAME: &[u8] = …`?
fn declares_byte_const(src: &str) -> bool {
    for line in src.lines() {
        let t = line.trim_start();
        let t = t.strip_prefix("pub ").unwrap_or(t);
        let Some(rest) = t.strip_prefix("const ") else {
            continue;
        };
        let Some((name, ty)) = rest.split_once(':') else {
            continue;
        };
        if !name
            .trim()
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            || name.trim().is_empty()
        {
            continue;
        }
        let ty: String = ty.chars().filter(|c| !c.is_whitespace()).collect();
        if !ty.starts_with("&[u8]") {
            continue;
        }
        // An EMPTY byte const is not canonical bytes, and treating it as such is a false
        // positive with a real cost. `torrent_peer` gained `const CONNECTION_CAP_REFUSAL:
        // &[u8] = b""` — a deliberate "refuse by closing, send nothing" — from the connection-
        // cap sweep, and that alone made this limb flag the protocol's `info_hash` example.
        //
        // The premise of limb B is "the canonical bytes now exist twice and one copy will
        // drift". With an empty const there is no second copy, so the premise does not hold.
        // A 20-byte BitTorrent info hash is an identifier a caller supplies, not a descriptor
        // derived from anything, and there is nothing for it to drift from.
        let value = ty.split_once('=').map(|(_, v)| v.trim()).unwrap_or("");
        if value.starts_with("b\"\"") || value.starts_with("&[]") || value == "\"\"" {
            continue;
        }
        // **A printable-ASCII byte string is a protocol LINE, not canonical bytes**, and the
        // same false positive came back wearing a fuller shirt. The empty-const rule above was
        // added when `torrent_peer` gained `CONNECTION_CAP_REFUSAL: &[u8] = b""`; the
        // connection-bounds sweeps then gave `bitcoin`, `kubernetes`, `mercurial`, `s3` and
        // `sqs` the same const with a real HTTP 503 line in it, and every one of them started
        // arming this limb against hex that has nothing to do with it. `mercurial` is where it
        // fired: its example carries `1234567890abcdef…`, a 40-character CHANGESET NODE ID,
        // which is what a Mercurial example is supposed to contain and is not a second copy of
        // an HTTP status line.
        //
        // Limb B's premise is "the canonical bytes now exist twice and one copy will drift".
        // Nobody writes an HTTP status line as hex, so the premise cannot hold for a const
        // whose value is plain text. It still holds — and this still arms — for the shape the
        // limb was built for: a byte ARRAY (`&[0x05, 0x01, …]`, the BLE HID report
        // descriptors) or a `b"…"` carrying `\x` escapes, both of which are binary a model
        // might plausibly be asked to copy as hex.
        let is_plain_text_bytestring =
            value.starts_with("b\"") && !value.contains("\\x") && !value.contains("\\u");
        if is_plain_text_bytestring {
            continue;
        }
        return true;
    }
    false
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

fn directory_source(path: &Path) -> String {
    let Some(dir) = path.parent() else {
        return String::new();
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

fn survey() -> BTreeSet<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut found = BTreeSet::new();
    for dir in ["src/server", "src/client"] {
        for (protocol, file, path) in rust_files(&manifest.join(dir)) {
            if protocol.is_empty() {
                continue;
            }
            let leaf = protocol.rsplit('/').next().unwrap_or(&protocol).to_string();
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let src = strip_comments(&raw);

            // Limb A — model-facing example bodies.
            for fname in MODEL_FACING_FNS {
                for body in bodies_of(&src, fname) {
                    for bytes in long_hex_literals(&body) {
                        found.insert(format!("A:{leaf}:{file}:{bytes}"));
                    }
                }
            }

            // Limb C — an `ActionDefinition`'s own `example`, which is **more** model-facing
            // than anything limb A scans: `executable_examples_test` literally sends it, so it
            // is the text a model copies most directly. It was outside the scan entirely until
            // 22 September 2026, found while chasing a limb-B false positive in `mercurial` —
            // the protocol's hex turned out to live here, in a place nothing looked.
            for body in action_example_bodies(&src) {
                for bytes in long_hex_literals(&body) {
                    found.insert(format!("C:{leaf}:{file}:{bytes}"));
                }
            }

            // Limb B — a transcription living beside the const it should be generated from.
            if file == "actions.rs"
                && declares_byte_const(&strip_comments(&directory_source(&path)))
            {
                for bytes in long_hex_literals(&src) {
                    found.insert(format!("B:{leaf}:{file}:{bytes}"));
                }
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

#[test]
fn no_long_hex_literal_in_what_the_model_copies() {
    let found = survey();
    let found_refs: BTreeSet<&str> = found.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = LONG_HEX_EXAMPLE_BASELINE.iter().copied().collect();

    let new: Vec<_> = found_refs.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these put more than {MAX_INLINE_HEX_BYTES} bytes of hand-written hex where the model \
         will copy it: {new:?}\n\
         `A:` is a long hex literal inside a model-facing example body; `B:` is one in an \
         `actions.rs` whose protocol already declares the canonical bytes as a `const … : \
         &[u8]`, which means there are now two copies and one of them will drift. Four of five \
         BLE HID profiles shipped report descriptors a real host rejects, and in two of them \
         the const was right and the example was wrong. Build the example from the bytes — \
         `hex::encode(THE_CONST)` — or, if there is no const, express it as structured fields \
         rather than a blob: a model cannot proofread hex, and nothing downstream will catch \
         what it copies."
    );

    let fixed: Vec<_> = baseline.difference(&found_refs).copied().collect();
    assert!(
        fixed.is_empty(),
        "these are gone — remove them from LONG_HEX_EXAMPLE_BASELINE: {fixed:?}"
    );
}

/// A scan that matches nothing passes forever.
///
/// Limb B's baseline is empty, so without this the whole limb could stop resolving and nothing
/// would notice. What is asserted is that the *subjects* are still there: protocols that
/// declare byte constants, and example bodies to look inside.
#[test]
fn the_scan_is_reading_real_source() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut with_byte_const = 0usize;
    let mut with_examples = 0usize;
    for dir in ["src/server", "src/client"] {
        for (protocol, file, path) in rust_files(&manifest.join(dir)) {
            if protocol.is_empty() || file != "actions.rs" {
                continue;
            }
            if declares_byte_const(&strip_comments(&directory_source(&path))) {
                with_byte_const += 1;
            }
            let src = strip_comments(&std::fs::read_to_string(&path).unwrap_or_default());
            if !bodies_of(&src, "get_startup_examples").is_empty() {
                with_examples += 1;
            }
        }
    }
    assert!(
        with_byte_const >= 8,
        "only {with_byte_const} protocols declare a `const … : &[u8]`; there were 12 when this \
         was written, so limb B has stopped recognising the declaration it keys on"
    );
    assert!(
        with_examples >= 100,
        "only {with_examples} protocols have a `get_startup_examples` body; limb A is not \
         looking at the examples it is supposed to be looking at"
    );
}

/// What the rule does on inputs whose right answer is known.
#[test]
fn the_rule_flags_the_historical_defect_and_not_its_fix() {
    // The BLE HID report descriptor for a mouse, as a literal — the shape that drifted. It is
    // 50 bytes, which is why 16 is the threshold: no one proofreads this.
    const DESCRIPTOR_HEX: &str = "05010902a1010901a100050919012905150025019505750181029501750381030501093009311581257f950275088106c0c0";

    // 1. Limb A: a descriptor pasted into the startup example.
    let drifted = format!(
        "fn get_startup_examples() -> Vec<Value> {{ vec![json!({{ \"report_map\": \"{DESCRIPTOR_HEX}\" }})] }}"
    );
    assert_eq!(
        bodies_of(&drifted, "get_startup_examples")
            .iter()
            .flat_map(|b| long_hex_literals(b))
            .collect::<Vec<_>>(),
        vec![DESCRIPTOR_HEX.len() / 2],
        "a report descriptor transcribed into the startup example must be flagged"
    );

    // 2. The fix, which is what all five BLE profiles now do: generate it from the const.
    let fixed = "fn get_startup_examples() -> Vec<Value> {\n\
         let report_map = hex::encode(HID_MOUSE_REPORT_DESCRIPTOR);\n\
         vec![json!({ \"report_map\": report_map })] }";
    assert!(
        bodies_of(fixed, "get_startup_examples")
            .iter()
            .all(|b| long_hex_literals(b).is_empty()),
        "`hex::encode(CONST)` cannot drift and must not be reported"
    );

    // 3. Limb B keys on the const declaration, in every spelling the tree uses.
    assert!(declares_byte_const(
        "pub const HID_MOUSE_REPORT_DESCRIPTOR: &[u8] = &[0x05, 0x01];"
    ));
    assert!(declares_byte_const("const ATR: &[u8] = &[0x3b];"));

    // An empty byte const declares no canonical bytes, so it must not arm limb B. This is a
    // real case: `torrent_peer`'s `CONNECTION_CAP_REFUSAL` is `b""` because that protocol
    // refuses by closing rather than by answering, and without this the protocol's 20-byte
    // info_hash example was reported as drift from bytes that do not exist.
    assert!(!declares_byte_const(
        "const CONNECTION_CAP_REFUSAL: &[u8] = b\"\";"
    ));
    // A printable-ASCII byte string is a protocol LINE, not canonical bytes. The connection-
    // bounds sweeps gave `bitcoin`, `kubernetes`, `mercurial`, `s3` and `sqs` a
    // `CONNECTION_CAP_REFUSAL` holding a real HTTP 503, and every one of them started arming
    // limb B against hex that has nothing to do with it — `mercurial` reported its example's
    // 40-character changeset node id as drift from an HTTP status line. Nobody writes a status
    // line as hex, so limb B's premise cannot hold here.
    assert!(!declares_byte_const(
        "const CONNECTION_CAP_REFUSAL: &[u8] = b\"HTTP/1.1 503 Service Unavailable\\r\\n\";"
    ));
    assert!(!declares_byte_const(
        "const QUERY_TOO_LONG: &[u8] = b\"finger: query too long\\r\\n\";"
    ));
    // But a `b"…"` carrying `\x` escapes IS binary a model might be asked to copy as hex, so
    // it stays armed — the exclusion is about text, not about the `b"…"` spelling.
    assert!(declares_byte_const(
        "const ATR_BYTES: &[u8] = b\"\\x3b\\x00\";"
    ));

    // Whitespace tolerance in the TYPE is what this line is for. Its value used to be `&[]`,
    // which was incidental filler until empty values started meaning "no canonical bytes" —
    // so it now carries a real one, and still tests the spacing it was written for.
    assert!(declares_byte_const(
        "pub const X: & [ u8 ] = &[0x01, 0x02];"
    ));
    assert!(
        !declares_byte_const("const TIMEOUT: u64 = 30;"),
        "only a byte-array const establishes a canonical byte sequence"
    );
    assert!(
        !declares_byte_const("let x: &[u8] = &[1];"),
        "a local binding is not a declaration the example could be built from"
    );

    // 4. Short hex is fine and must stay fine — a MAC, an 8-byte magic, a session id. Flagging
    //    these would put dozens of entries in the baseline and teach people to edit it.
    for short in [
        "\"001122334455\"",                     // 6-byte MAC
        "\"f9beb4d9\"",                         // 4-byte magic
        "\"0123456789abcdef0123456789abcdef\"", // exactly 16 bytes — the boundary
    ] {
        assert!(
            long_hex_literals(short).is_empty(),
            "{short} is short enough to read by eye and must not be flagged"
        );
    }
    assert_eq!(
        long_hex_literals("\"0123456789abcdef0123456789abcdef01\""),
        vec![17],
        "17 bytes is over the line"
    );

    // 5. Not hex at all. Base64, a UUID and prose all contain hex digits.
    for not_hex in [
        "\"00002a4b-0000-1000-8000-00805f9b34fb\"", // GATT UUID: has dashes
        "\"SGVsbG8gd29ybGQgdGhpcyBpcyBiYXNlNjQ=\"", // base64
        "\"the quick brown fox jumped over it\"",
    ] {
        assert!(
            long_hex_literals(not_hex).is_empty(),
            "{not_hex} is not a hex literal"
        );
    }

    // 6. An odd number of hex characters is not a byte string.
    assert!(
        long_hex_literals("\"0123456789abcdef0123456789abcdef012\"").is_empty(),
        "an odd-length hex run cannot be bytes and is probably an identifier"
    );

    // 7. Every occurrence of the body, not just the first — a second `impl` block would
    //    otherwise hide its examples entirely.
    let two_impls = format!(
        "impl A {{ fn get_startup_examples() -> V {{ vec![] }} }}\n\
         impl B {{ fn get_startup_examples() -> V {{ json!(\"{DESCRIPTOR_HEX}\") }} }}"
    );
    assert_eq!(
        bodies_of(&two_impls, "get_startup_examples").len(),
        2,
        "both bodies must be scanned"
    );
}

/// Every `example: json!( … )` block — an `ActionDefinition`'s own example.
///
/// Separate from [`bodies_of`], which keys on a function name: this is a struct field, and the
/// text inside it is what `executable_examples_test` sends through the protocol's executor. It
/// is the most directly copied text in the tree.
fn action_example_bodies(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = src.as_bytes();
    let needle = "example:";
    let mut from = 0usize;
    while let Some(rel) = src[from..].find(needle) {
        let start = from + rel + needle.len();
        from = start;
        // Find the opening paren of `json!(`, then balance to its close.
        let Some(open_rel) = src[start..].find('(') else {
            break;
        };
        let open = start + open_rel;
        // Only accept `json!(` immediately before it; anything else is a different field.
        if !src[start..open].trim().ends_with("json!") {
            continue;
        }
        let mut depth = 0i32;
        let mut end = open;
        for (i, c) in src[open..].char_indices() {
            match bytes[open + i] as char {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = open + i;
                        break;
                    }
                }
                _ => {
                    let _ = c;
                }
            }
        }
        if end > open {
            out.push(src[open..=end].to_string());
            from = end;
        }
    }
    out
}
