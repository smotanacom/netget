//! If a decoder can call itself, it needs a counter.
//!
//! A Rust stack overflow is a `SIGSEGV` against the guard page, not a panic. There is nothing
//! for `catch_unwind` to see, no task for `tokio::spawn` to lose in isolation, and no log line:
//! the **whole NetGet process dies**. That makes it the one defect class in this tree where a
//! single unauthenticated peer takes down every other server in the process.
//!
//! Six protocols have had it, and the striking thing is how cheap each was on the wire:
//!
//! * **AMQP** field tables are recursive and cost the peer **five bytes per level**, so one
//!   128 KiB frame bought ~26 000 levels. Now `MAX_FIELD_TABLE_DEPTH = 32`; real tables are
//!   depth 2, and the bound was verified by removing it and watching the test binary abort.
//! * **bencode** was cheapest of all: one byte (`l` or `d`) opens a level, ~1 KB killed a
//!   2 MiB tokio worker, and the DHT server took those bytes from an unauthenticated **UDP**
//!   socket. A typed decode was no safer — serde's derive skips unknown fields through
//!   `IgnoredAny`, which lands straight back in `deserialize_any`.
//! * **`xmlrpc` 0.15** could be overflowed by the reply to the *first* call, at ~20 bytes per
//!   `<value><array><data>` level.
//!
//! Every one of them was found by a person reasoning about a parser, one at a time. This test
//! is the part of that which can be mechanised.
//!
//! # The rule
//!
//! Over every `.rs` in `src/server`, `src/client`, `src/utils`, `src/protocol` and
//! `src/scripting`, a function is flagged when **all** of these hold:
//!
//! 1. it is part of a **call cycle within its file** — directly self-recursive, or mutually
//!    recursive with a sibling. Mutual recursion is not an edge case here: AMQP's defect is
//!    `field_table_at` → `field_value_at` → `field_table_at`, and a direct-self-call rule
//!    would have missed the worst instance in the tree.
//! 2. its **parameter list** has decoder shape — `&[u8]`, a `&mut` reader/cursor/decoder, a
//!    parameter whose type ends in `Value` (`serde_json::Value`, `XmlRpcValue`, `ProtoValue`,
//!    `serde_bencode::value::Value`), or `&self`/`&mut self` on an `impl` whose type is a
//!    Decoder/Encoder/Parser/Reader/Codec/Scanner/Writer. The *return* type is deliberately
//!    not consulted: keying on it pulled in every helper that merely produces JSON.
//! 3. nothing in its protocol directory bounds the depth — no `depth`/`level`/`nest`/
//!    `remaining`/`budget`/`recursion` parameter, and no `MAX_*_DEPTH` / `*_DEPTH_LIMIT`
//!    constant. The bound is looked for across the **directory**, not the file, because
//!    `xmlrpc`'s `MAX_VALUE_DEPTH` lives in `mod.rs` and guards a walker in `actions.rs`.
//!
//! # Refining this rule *was* the task
//!
//! `PROTOCOL_QUALITY.md` records that the crude version — "does the function's own name appear
//! in its body" — reports **973 functions**, and `CLAUDE.md` is explicit about what a noisy
//! build-failing check does: people edit the baseline instead of the code. Getting from 973 to
//! 26 took four discriminators, and each one was added because of a specific family of false
//! positives:
//!
//! * **A call, not a mention.** `self.adapter_name` is not recursion into `adapter_name`. Every
//!   `&str`-returning accessor in the BLE and protocol modules came in this way.
//! * **Not a method of the same name on something else.** `ddb.put_item()` inside `fn
//!   put_item` is the AWS SDK, not recursion; the same shape accounts for all six DynamoDB and
//!   all seven S3 client wrappers. Hence a call counts only via `self`, `Self::`, a bare free
//!   call, or a receiver this body itself constructed as the same type — which is what keeps
//!   AMQP's `inner.field_value_at(depth + 1)` visible while excluding `ddb.put_item()`.
//! * **A heap frame is not a stack frame.** `async fn`s and functions returning `BoxFuture` /
//!   `Pin<Box<dyn Future>>` are excluded from the graph entirely. A recursive `async fn` *must*
//!   be boxed or it does not compile, and boxing is precisely the sanctioned fix for the
//!   action → event → action cycle (`MAX_FOLLOWUP_DEPTH`). Including them flagged twenty
//!   LLM-follow-up chains that are bounded by construction.
//! * **Parameters, not return types.** See rule 2 above.
//!
//! What survives is 26 functions, of which 11 are correctly recognised as already bounded —
//! `amqp` (5), `vnc` (2), `xmlrpc` server (2) and client (2). That the known-good bounds are
//! *seen* is what makes the ratchet meaningful: remove
//! `MAX_FIELD_TABLE_DEPTH` and AMQP moves from the exempt set into the failure.
//!
//! **The baseline may only shrink**, and the fix is always the same three lines: take a `depth`
//! parameter, refuse past a constant, pass `depth + 1`.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test recursive_decoder_depth_test

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// `area:protocol:file:function` for every recursive decoder with no depth bound in scope.
///
/// No line numbers: these are stable identities and other agents edit these files constantly.
///
/// **`grpc` (8 entries, four in each of the server and the client).** `proto_value_to_json`
/// walks a `prost_reflect::Value` the peer sent; `json_to_proto_value` / `json_to_field_value`
/// / `json_to_dynamic_message` walk JSON the model sent. Both are bounded *incidentally* —
/// prost's decoder caps recursion at 100 and `serde_json` caps `from_str` at 128 — so neither
/// is exploitable today. They are listed because an incidental bound owned by a dependency is
/// exactly what `CLAUDE.md` says not to rely on: a prost major version or a switch to a
/// streaming JSON reader removes it silently. Fix: a `depth` parameter, bound at 32, which is
/// far above any real protobuf schema.
///
/// Their cycle partner `dynamic_message_to_json` is deliberately *not* here: it takes
/// `&DynamicMessage`, which is not a decoder-shaped parameter, so the rule sees the cycle at
/// the other node only. One node per cycle is enough to fail the build, and widening the shape
/// vocabulary to catch the second was what pulled in the `ActionResult` walkers.
///
/// **`ipp:actions.rs:check`** recursively validates the model's attribute object. Bounded by
/// serde_json's 128 today; same argument as grpc.
///
/// **`torrent_dht:mod.rs:bencode_to_json`** is bounded in practice and the interesting case:
/// `handle_datagram` runs `crate::utils::bencode::check_bencode_structure` (iterative,
/// `MAX_BENCODE_DEPTH = 32`) before anything is decoded, so the tree this walks can never be
/// deeper than 32. The constant lives in `src/utils/`, not in the protocol directory, so the
/// scan cannot see it. Fix: name the bound at the walker too, or re-export it.
///
/// **`usb/fido2:mod.rs:run_command` and `:park`** are genuinely mutually recursive over a
/// `&[u8]` CTAPHID payload, and terminate for a semantic reason rather than a counted one:
/// `park` re-enters `run_command` only with `UserPresence::Denied`, and that arm cannot park
/// again. That is a two-level bound held by an enum variant, which is real but invisible —
/// a future arm that parks under a different presence value reopens it. Fix: a depth parameter,
/// or an explicit `debug_assert` naming the invariant.
///
/// **`scripting/event_handler.rs` (3).** `interpolate_value`, `contains_event_reference` and
/// `validate_event_references` recurse over a handler's configuration JSON, which arrives over
/// MCP and from the dashboard. Bounded by serde_json's parse limit rather than by anything
/// here.
const UNBOUNDED_RECURSIVE_DECODER_BASELINE: &[&str] = &[
    "client:grpc:mod.rs:json_to_dynamic_message",
    "client:grpc:mod.rs:json_to_field_value",
    "client:grpc:mod.rs:json_to_proto_value",
    "client:grpc:mod.rs:proto_value_to_json",
    "scripting::contains_event_reference",
    "scripting::interpolate_value",
    "scripting::validate_event_references",
    "server:grpc:mod.rs:json_to_dynamic_message",
    "server:grpc:mod.rs:json_to_field_value",
    "server:grpc:mod.rs:json_to_proto_value",
    "server:grpc:mod.rs:proto_value_to_json",
    "server:ipp:actions.rs:check",
    "server:torrent_dht:mod.rs:bencode_to_json",
    "server:usb/fido2:mod.rs:park",
    "server:usb/fido2:mod.rs:run_command",
];

/// Parameter-type shapes that mean "this function consumes something a peer or the model sent,
/// and its structure decides how deep the recursion goes".
const DECODER_PARAM_SHAPES: &[&str] = &[
    "&[u8]",
    "&mut [u8]",
    "&mut Reader",
    "&mut Cursor",
    "&mut Buf",
    "&mut BytesMut",
    "&mut Decoder",
    "&mut Parser",
    "&mut Scanner",
    "&mut Writer",
];

/// `impl` type names for which a bare `&self` decoder is still a decoder.
const DECODER_IMPL_TYPES: &[&str] = &[
    "Decoder", "Encoder", "Parser", "Reader", "Codec", "Scanner", "Writer", "Cursor",
];

/// Parameter names that count as an explicit depth bound.
const DEPTH_PARAM_PREFIXES: &[&str] =
    &["depth", "level", "nest", "remaining", "budget", "recursion"];

/// Return-type markers for a future whose frame lives on the heap.
const HEAP_FRAME_RETURNS: &[&str] = &[
    "BoxFuture",
    "Pin<Box",
    "Pin < Box",
    "impl Future",
    "dyn Future",
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

/// Blank out the contents of string literals, so a name inside a message is not read as a call.
fn blank_strings(src: &str) -> String {
    let b: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] != '"' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        out.push('"');
        i += 1;
        while i < b.len() && b[i] != '"' {
            if b[i] == '\\' && i + 1 < b.len() {
                out.push(' ');
                i += 1;
            }
            out.push(if b[i] == '\n' { '\n' } else { ' ' });
            i += 1;
        }
        if i < b.len() {
            out.push('"');
            i += 1;
        }
    }
    out
}

/// Match `bracket` forward from `from` (which must sit on the opener), returning the closer.
fn match_bracket(b: &[char], from: usize, open: char, close: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut i = from;
    while i < b.len() {
        if b[i] == open {
            depth += 1;
        } else if b[i] == close {
            depth -= 1;
            if depth == 0 {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

struct FnDef {
    name: String,
    line: usize,
    params: String,
    ret: String,
    body: String,
    impl_ty: Option<String>,
    is_async: bool,
}

/// Every `impl <Type>` header in the file, as `(offset, type name)`.
fn impl_headers(b: &[char]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 < b.len() {
        let at_line_start = i == 0
            || (b[..i]
                .iter()
                .rev()
                .take_while(|c| **c == ' ' || **c == '\t')
                .count()
                + 1
                <= i
                && b[i
                    - 1
                    - b[..i]
                        .iter()
                        .rev()
                        .take_while(|c| **c == ' ' || **c == '\t')
                        .count()]
                    == '\n');
        if at_line_start
            && b[i] == 'i'
            && b[i + 1] == 'm'
            && b[i + 2] == 'p'
            && b[i + 3] == 'l'
            && !is_ident_char(b[i + 4])
        {
            // skip an optional `<'a, T>` generic list, then take the first identifier
            let mut j = i + 4;
            while j < b.len() && b[j].is_whitespace() {
                j += 1;
            }
            if j < b.len() && b[j] == '<' {
                match match_bracket(b, j, '<', '>') {
                    Some(k) => j = k + 1,
                    None => {
                        i += 1;
                        continue;
                    }
                }
            }
            while j < b.len() && b[j].is_whitespace() {
                j += 1;
            }
            let start = j;
            while j < b.len() && is_ident_char(b[j]) {
                j += 1;
            }
            if j > start {
                out.push((i, b[start..j].iter().collect::<String>()));
            }
        }
        i += 1;
    }
    out
}

fn parse_fns(src: &str) -> Vec<FnDef> {
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

    let impls = impl_headers(&b);
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 3 < b.len() {
        // a bare `fn ` token
        if !(b[i] == 'f'
            && b[i + 1] == 'n'
            && b[i + 2].is_whitespace()
            && (i == 0 || !is_ident_char(b[i - 1])))
        {
            i += 1;
            continue;
        }
        let fn_at = i;
        let mut j = i + 2;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        let name_start = j;
        while j < b.len() && is_ident_char(b[j]) {
            j += 1;
        }
        if j == name_start {
            i += 1;
            continue;
        }
        let name: String = b[name_start..j].iter().collect();

        // optional generic list
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == '<' {
            match match_bracket(&b, j, '<', '>') {
                Some(k) => j = k + 1,
                None => {
                    i = name_start;
                    continue;
                }
            }
        }
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j >= b.len() || b[j] != '(' {
            i = name_start;
            continue;
        }
        let Some(pclose) = match_bracket(&b, j, '(', ')') else {
            i = name_start;
            continue;
        };
        let params: String = b[j..=pclose].iter().collect();

        // return type / where clause, up to the body brace or a `;` (trait signature)
        let mut k = pclose + 1;
        while k < b.len() && b[k] != '{' && b[k] != ';' {
            k += 1;
        }
        if k >= b.len() || b[k] == ';' {
            i = name_start;
            continue;
        }
        let ret: String = b[pclose + 1..k].iter().collect();
        let Some(bclose) = match_bracket(&b, k, '{', '}') else {
            i = name_start;
            continue;
        };
        let body: String = b[k..=bclose].iter().collect();

        // `async` immediately before `fn`
        let lead: String = b[..fn_at].iter().rev().take(16).collect::<String>();
        let lead: String = lead.chars().rev().collect();
        let is_async = lead.trim_end().ends_with("async");

        let impl_ty = impls
            .iter()
            .rev()
            .find(|(off, _)| *off < fn_at)
            .map(|(_, t)| t.clone());

        out.push(FnDef {
            name,
            line: line_of[fn_at],
            params,
            ret,
            body,
            impl_ty,
            is_async,
        });
        i = k;
    }
    out
}

/// Names bound in this body by a `let` whose right-hand side constructs `Self` or `impl_ty`.
///
/// This is what makes AMQP's `let mut inner = Decoder::new(body); … inner.field_value_at(..)`
/// visible as recursion while `let ddb = …; ddb.put_item()` stays invisible.
fn sibling_bindings(body: &str, impl_ty: Option<&str>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let Some(ty) = impl_ty else { return out };
    let b: Vec<char> = body.chars().collect();
    let mut i = 0usize;
    while i + 3 < b.len() {
        if !(b[i] == 'l'
            && b[i + 1] == 'e'
            && b[i + 2] == 't'
            && b[i + 3].is_whitespace()
            && (i == 0 || !is_ident_char(b[i - 1])))
        {
            i += 1;
            continue;
        }
        let mut j = i + 3;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        // optional `mut`
        if b[j..].starts_with(&['m', 'u', 't']) && j + 3 < b.len() && b[j + 3].is_whitespace() {
            j += 3;
            while j < b.len() && b[j].is_whitespace() {
                j += 1;
            }
        }
        let start = j;
        while j < b.len() && is_ident_char(b[j]) {
            j += 1;
        }
        if j == start {
            i += 1;
            continue;
        }
        let ident: String = b[start..j].iter().collect();
        // right-hand side up to the statement's `;`
        let mut k = j;
        while k < b.len() && b[k] != '=' && b[k] != ';' {
            k += 1;
        }
        if k >= b.len() || b[k] == ';' {
            i = j;
            continue;
        }
        let mut e = k;
        while e < b.len() && b[e] != ';' {
            e += 1;
        }
        let rhs: String = b[k..e.min(b.len())].iter().collect();
        let constructs = rhs.contains(&format!("{ty}::"))
            || rhs.contains(&format!("{ty} {{"))
            || rhs.contains("Self::")
            || rhs.contains("Self {");
        if constructs {
            out.insert(ident);
        }
        i = j;
    }
    out
}

/// Names this body calls **on itself**.
fn self_calls(body: &str, impl_ty: Option<&str>) -> BTreeSet<String> {
    let src = blank_strings(body);
    let siblings = sibling_bindings(&src, impl_ty);
    let b: Vec<char> = src.chars().collect();
    let mut out = BTreeSet::new();

    let mut i = 0usize;
    while i < b.len() {
        if !is_ident_char(b[i]) || (i > 0 && is_ident_char(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_ident_char(b[i]) {
            i += 1;
        }
        let name: String = b[start..i].iter().collect();

        // must be a call: optional turbofish, then `(`
        let mut j = i;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j + 1 < b.len() && b[j] == ':' && b[j + 1] == ':' {
            let mut t = j + 2;
            while t < b.len() && b[t].is_whitespace() {
                t += 1;
            }
            if t < b.len() && b[t] == '<' {
                match match_bracket(&b, t, '<', '>') {
                    Some(k) => {
                        j = k + 1;
                        while j < b.len() && b[j].is_whitespace() {
                            j += 1;
                        }
                    }
                    None => continue,
                }
            } else {
                continue;
            }
        }
        if j >= b.len() || b[j] != '(' {
            continue;
        }

        // What precedes the name decides whether it is a call on *us*.
        let mut p = start;
        while p > 0 && (b[p - 1] == ' ' || b[p - 1] == '\n' || b[p - 1] == '\t') {
            p -= 1;
        }
        let ok = if p >= 2 && b[p - 1] == ':' && b[p - 2] == ':' {
            // `Recv::name(` — only `Self::` or the impl type count.
            let mut q = p - 2;
            while q > 0 && is_ident_char(b[q - 1]) {
                q -= 1;
            }
            let recv: String = b[q..p - 2].iter().collect();
            recv == "Self" || impl_ty.is_some_and(|t| recv == t)
        } else if p >= 1 && b[p - 1] == '.' {
            // `recv.name(` — `self`, or a local this body built as the same type.
            let mut q = p - 1;
            while q > 0 && (b[q - 1] == ' ' || b[q - 1] == '\n' || b[q - 1] == '\t') {
                q -= 1;
            }
            let end = q;
            while q > 0 && is_ident_char(b[q - 1]) {
                q -= 1;
            }
            let recv: String = b[q..end].iter().collect();
            let field_path = q > 0 && b[q - 1] == '.';
            recv == "self" || (!field_path && siblings.contains(&recv))
        } else {
            // a bare free call
            true
        };
        if ok {
            out.insert(name);
        }
    }
    out
}

fn has_decoder_shape(params: &str, impl_ty: Option<&str>) -> bool {
    let squashed: String = params.split_whitespace().collect::<Vec<_>>().join(" ");
    let tight = squashed.replace(" ", "");
    if DECODER_PARAM_SHAPES
        .iter()
        .any(|s| tight.contains(&s.replace(' ', "")))
    {
        return true;
    }
    // a parameter whose type ends in `Value`
    let b: Vec<char> = tight.chars().collect();
    for i in 0..b.len() {
        if b[i] != ':' || (i + 1 < b.len() && b[i + 1] == ':') || (i > 0 && b[i - 1] == ':') {
            continue;
        }
        let rest: String = b[i + 1..].iter().take(64).collect();
        let ty: String = rest
            .chars()
            .take_while(|c| is_ident_char(*c) || *c == ':' || *c == '&' || *c == '\'')
            .collect();
        let leaf = ty.rsplit("::").next().unwrap_or(&ty);
        let leaf = leaf.trim_start_matches('&');
        if leaf.ends_with("Value") && !leaf.is_empty() {
            return true;
        }
    }
    // `&self` / `&mut self` on a decoder-ish type
    if tight.contains("&self") || tight.contains("&mutself") {
        if let Some(t) = impl_ty {
            if DECODER_IMPL_TYPES.iter().any(|d| t.contains(d)) {
                return true;
            }
        }
    }
    false
}

fn has_depth_param(params: &str) -> bool {
    let b: Vec<char> = params.chars().collect();
    let mut i = 0usize;
    while i < b.len() {
        if !is_ident_char(b[i]) || (i > 0 && is_ident_char(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_ident_char(b[i]) {
            i += 1;
        }
        let ident: String = b[start..i].iter().collect::<String>().to_ascii_lowercase();
        let mut j = i;
        while j < b.len() && b[j].is_whitespace() {
            j += 1;
        }
        if j < b.len() && b[j] == ':' && DEPTH_PARAM_PREFIXES.iter().any(|p| ident.starts_with(p)) {
            return true;
        }
    }
    false
}

/// A `MAX_*_DEPTH` / `MAX_*_NEST…` / `*_DEPTH_LIMIT` / `*_RECURSION_LIMIT` identifier anywhere.
fn has_depth_constant(src: &str) -> bool {
    let b: Vec<char> = src.chars().collect();
    let mut i = 0usize;
    while i < b.len() {
        if !is_ident_char(b[i]) || (i > 0 && is_ident_char(b[i - 1])) {
            i += 1;
            continue;
        }
        let start = i;
        while i < b.len() && is_ident_char(b[i]) {
            i += 1;
        }
        let ident: String = b[start..i].iter().collect();
        if ident
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            && ((ident.starts_with("MAX_")
                && (ident.contains("DEPTH")
                    || ident.contains("NEST")
                    || ident.contains("RECURSION")
                    || ident.contains("LEVEL")))
                || ident.ends_with("DEPTH_LIMIT")
                || ident.ends_with("RECURSION_LIMIT"))
        {
            return true;
        }
    }
    false
}

fn returns_heap_frame(ret: &str) -> bool {
    HEAP_FRAME_RETURNS.iter().any(|m| ret.contains(m))
}

/// Every function in `src` that is part of a call cycle and has decoder shape, with whether a
/// depth bound is in scope (`dir_src` is the whole protocol directory concatenated).
fn scan(src_raw: &str, dir_src: &str) -> Vec<(String, usize, bool)> {
    let src = strip_comments(src_raw);
    let fns = parse_fns(&src);

    let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    for (idx, f) in fns.iter().enumerate() {
        if f.is_async || returns_heap_frame(&f.ret) {
            continue; // a heap frame cannot overflow the stack by recursing
        }
        by_name.entry(f.name.clone()).or_default().push(idx);
    }

    let mut edges: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (name, idxs) in &by_name {
        let mut calls = BTreeSet::new();
        for &idx in idxs {
            let f = &fns[idx];
            for c in self_calls(&f.body, f.impl_ty.as_deref()) {
                if by_name.contains_key(&c) {
                    calls.insert(c);
                }
            }
        }
        edges.insert(name.clone(), calls);
    }

    fn reaches(
        from: &str,
        target: &str,
        edges: &BTreeMap<String, BTreeSet<String>>,
        seen: &mut BTreeSet<String>,
    ) -> bool {
        if !seen.insert(from.to_string()) {
            return false;
        }
        for n in edges.get(from).into_iter().flatten() {
            if n == target || reaches(n, target, edges, seen) {
                return true;
            }
        }
        false
    }

    let bounded_by_constant = has_depth_constant(dir_src);
    let mut out = Vec::new();
    for (name, idxs) in &by_name {
        let mut seen = BTreeSet::new();
        if !reaches(name, name, &edges, &mut seen) {
            continue;
        }
        for &idx in idxs {
            let f = &fns[idx];
            if !has_decoder_shape(&f.params, f.impl_ty.as_deref()) {
                continue;
            }
            let bounded = bounded_by_constant || has_depth_param(&f.params);
            out.push((f.name.clone(), f.line, bounded));
        }
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

/// Every `.rs` in the same directory, concatenated — the scope a depth constant is looked for
/// in, because `xmlrpc`'s `MAX_VALUE_DEPTH` guards a walker in a sibling file.
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

/// `(all recursive decoders, the unbounded subset)`.
fn survey() -> (BTreeSet<String>, BTreeSet<String>) {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = BTreeSet::new();
    let mut unbounded = BTreeSet::new();

    for (area, dir) in [
        ("server", "src/server"),
        ("client", "src/client"),
        ("utils", "src/utils"),
        ("protocol", "src/protocol"),
        ("scripting", "src/scripting"),
    ] {
        for (protocol, file, path) in rust_files(&manifest.join(dir)) {
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let dir_src = directory_source(&path);
            for (name, _line, bounded) in scan(&raw, &dir_src) {
                let id = if protocol.is_empty() {
                    format!("{area}::{name}")
                } else {
                    format!("{area}:{protocol}:{file}:{name}")
                };
                if !bounded {
                    unbounded.insert(id.clone());
                }
                all.insert(id);
            }
        }
    }
    (all, unbounded)
}

// ---------------------------------------------------------------------------
// The ratchet
// ---------------------------------------------------------------------------

#[test]
fn no_recursive_decoder_is_unbounded() {
    let (_, unbounded) = survey();
    let found: BTreeSet<&str> = unbounded.iter().map(String::as_str).collect();
    let baseline: BTreeSet<&str> = UNBOUNDED_RECURSIVE_DECODER_BASELINE
        .iter()
        .copied()
        .collect();

    let new: Vec<_> = found.difference(&baseline).copied().collect();
    assert!(
        new.is_empty(),
        "these recurse over something a peer or the model sent, with no depth counter and no \
         MAX_*_DEPTH in their protocol directory: {new:?}\n\
         A Rust stack overflow is a SIGSEGV against the guard page, not a panic: `catch_unwind` \
         cannot see it, `tokio::spawn` cannot contain it, and the whole NetGet process dies — \
         taking every other server in it. AMQP's field tables cost the peer five bytes a level; \
         bencode costs one. Take a `depth: usize`, refuse past a constant (32 is far above any \
         real message), and pass `depth + 1`. Verify the bound the way AMQP's was: remove it \
         and watch the test binary abort with `stack overflow`."
    );

    let fixed: Vec<_> = baseline.difference(&found).copied().collect();
    assert!(
        fixed.is_empty(),
        "these are bounded now, or moved — update UNBOUNDED_RECURSIVE_DECODER_BASELINE: {fixed:?}"
    );
}

/// The known-good bounds must be *visible* to the scan, or the ratchet checks nothing.
///
/// This is the load-bearing assertion of the file. If `scan` stopped resolving AMQP's mutual
/// recursion through `inner.field_value_at(..)`, the test above would go green while a removed
/// `MAX_FIELD_TABLE_DEPTH` sailed past.
#[test]
fn the_scan_sees_the_decoders_that_are_already_bounded() {
    let (all, unbounded) = survey();
    let bounded: Vec<&String> = all.difference(&unbounded).collect();

    for expected in [
        "server:amqp:codec.rs:field_table_at",
        "server:amqp:codec.rs:field_value_at",
        "server:xmlrpc:actions.rs:xmlrpc_value_to_json",
        "client:xmlrpc:mod.rs:xmlrpc_value_to_json_at",
        "server:vnc:actions.rs:parse_display_command",
    ] {
        assert!(
            bounded.iter().any(|b| b.as_str() == expected),
            "{expected} is a recursive decoder with a depth bound and the scan must see both \
             halves; it currently reports: {bounded:?}"
        );
    }

    assert!(
        all.len() >= 20,
        "only {} recursive decoders found in the whole tree; there were 26 when this was \
         written, so a number this low means the call-cycle detection broke rather than that \
         the code got better",
        all.len()
    );
}

/// What the rule does on inputs whose right answer is known.
#[test]
fn the_rule_flags_the_historical_defects_and_not_their_fixes() {
    fn scan_one(src: &str) -> Vec<(String, bool)> {
        scan(src, src).into_iter().map(|(n, _, b)| (n, b)).collect()
    }

    // 1. The bencode defect: a free function walking a wire-supplied value tree.
    assert_eq!(
        scan_one(
            "fn bencode_to_json(value: &serde_bencode::value::Value) -> serde_json::Value {\n\
             match value { Value::List(l) => l.iter().map(|v| bencode_to_json(v)).collect(), _ => json!(null) }\n\
             }"
        ),
        vec![("bencode_to_json".to_string(), false)],
        "a self-recursive walker over a wire-supplied Value must be flagged"
    );

    // 2. The fix: a depth parameter.
    assert_eq!(
        scan_one(
            "fn bencode_to_json(value: &serde_bencode::value::Value, depth: usize) -> Value {\n\
             if depth > 32 { return Value::Null; }\n\
             bencode_to_json(value, depth + 1)\n}"
        ),
        vec![("bencode_to_json".to_string(), true)],
        "a `depth` parameter is the fix and must be recognised"
    );

    // 3. AMQP's shape: MUTUAL recursion, reached through a sibling the body constructed. A
    //    direct-self-call rule misses this entirely, and it is the worst instance in the tree.
    let amqp = "impl<'a> Decoder<'a> {\n\
        fn field_table_at(&mut self, depth: usize) -> Result<Value> {\n\
        let mut inner = Decoder::new(b);\n\
        inner.field_value_at(depth + 1)\n}\n\
        fn field_value_at(&mut self, depth: usize) -> Result<Value> {\n\
        self.field_table_at(depth + 1)\n}\n}";
    let got = scan_one(amqp);
    assert!(
        got.iter().any(|(n, _)| n == "field_table_at")
            && got.iter().any(|(n, _)| n == "field_value_at"),
        "mutual recursion through a locally-constructed sibling must be seen: {got:?}"
    );
    assert!(
        got.iter().all(|(_, bounded)| *bounded),
        "both halves take a depth parameter and must be exempt: {got:?}"
    );

    // 4. The same pair with the bound removed — this is the regression the ratchet exists for.
    let amqp_broken = amqp.replace(", depth: usize", "").replace("depth + 1", "0");
    let broken = scan_one(&amqp_broken);
    assert!(
        broken.iter().any(|(n, b)| n == "field_value_at" && !*b),
        "removing MAX_FIELD_TABLE_DEPTH and the depth parameter must make it fail: {broken:?}"
    );

    // 5. The SDK-wrapper family: `ddb.put_item()` inside `fn put_item` is not recursion. This
    //    one shape accounted for thirteen false positives across the DynamoDB and S3 clients.
    assert!(
        scan_one(
            "fn put_item(ddb: &Client, data: &serde_json::Value) -> Result<Value> {\n\
             ddb.put_item().send()\n}"
        )
        .is_empty(),
        "a same-named method on a foreign receiver is not a self-call"
    );

    // 6. An accessor that merely mentions its own name as a field.
    assert!(
        scan_one("impl A { fn adapter_name(&self) -> &str { &self.adapter_name } }").is_empty(),
        "`self.name` is a field read, not a call"
    );

    // 7. A boxed/async follow-up chain. This is the sanctioned fix for action -> event ->
    //  action, bounded by MAX_FOLLOWUP_DEPTH, and including it flagged twenty of them.
    assert!(
        scan_one(
            "fn execute<'a>(action: serde_json::Value) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {\n\
             Box::pin(async move { execute(action).await })\n}"
        )
        .is_empty(),
        "a heap-allocated frame cannot overflow the stack by recursing"
    );

    // 8. A depth constant in the directory counts even when it is in another file — this is
    //    `xmlrpc`, whose MAX_VALUE_DEPTH lives in mod.rs and guards a walker in actions.rs.
    let walker = "fn to_json(v: &XmlRpcValue) -> serde_json::Value { to_json(v) }";
    assert_eq!(
        scan(walker, "const MAX_VALUE_DEPTH: usize = 64;")
            .into_iter()
            .map(|(n, _, b)| (n, b))
            .collect::<Vec<_>>(),
        vec![("to_json".to_string(), true)],
        "a MAX_*_DEPTH elsewhere in the protocol directory is a bound in scope"
    );

    // 9. No decoder shape: recursion over things nobody sent us is not this class.
    assert!(
        scan_one("fn fib(n: u64) -> u64 { if n < 2 { n } else { fib(n - 1) + fib(n - 2) } }")
            .is_empty(),
        "the parameter list is the anchor; arithmetic recursion is not a decoder"
    );
}
