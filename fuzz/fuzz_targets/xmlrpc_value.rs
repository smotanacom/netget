//! `netget::server::xmlrpc::parse_method_call` — the XML-RPC server's request decoder, and the
//! event builder that walks what it parsed.
//!
//! The parser is iterative over quick-xml events, so reading a deep document cannot overflow
//! the stack. What it builds is a recursive `XmlRpcValue`, and the request path walks it
//! recursively: `actions::create_method_call_event` converts every parameter with
//! `xmlrpc_value_to_json`, and dropping the value recurses too. A closed
//! `<value><array><data>…</data></array></value>` level is 42 bytes and the body cap is 4 MiB,
//! so without `MAX_VALUE_DEPTH` a single POST builds a value ~100,000 deep. The depth bound is
//! what protects those walkers, and this target drives them exactly as the server does: the
//! body is decoded with `from_utf8_lossy` (as `handle_xmlrpc_request` does), parsed, and
//! whatever parses becomes the model's event.
//!
//! The corpus carries `depth_bomb` — 20,000 levels of closed `<value><array><data>` inside a
//! real `<methodCall>`, kept under the 1 MiB at which libFuzzer truncates a seed — and
//! `at_depth_limit`, a value that nests as deep as the guard allows.

#![no_main]

use libfuzzer_sys::fuzz_target;
use netget::server::xmlrpc::actions::create_method_call_event;
use netget::server::xmlrpc::parse_method_call;

fuzz_target!(|data: &[u8]| {
    let body = String::from_utf8_lossy(data);
    if let Ok(call) = parse_method_call(&body) {
        let event = create_method_call_event(&call);
        // The event carries one JSON parameter per parsed parameter.
        let params = event.data["params"].as_array().map(|a| a.len());
        assert_eq!(params, Some(call.params.len()));
        drop(event);
        drop(call);
    }
});
