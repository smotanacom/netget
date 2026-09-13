//! `send_first` was declared by seven servers that then dropped it.
//!
//! Each read the parameter, threaded it through `spawn`, and the far end took it as
//! `_send_first` — so the knob was advertised to the model, plumbed through three hops, and did
//! nothing. An advertised knob that does nothing is worse than an absent one: the model has no
//! way to discover that turning it does not work.
//!
//! Deleting the declaration was the other option and is worse. An undeclared key is *refused*
//! at startup, so any existing caller passing `send_first: false` — the value that matches what
//! the server actually does — would start failing. Declaring it and refusing only `true` keeps
//! the honest call working and makes the dishonest one loud.

#![allow(clippy::disallowed_names)]

use netget::llm::actions::protocol_trait::Protocol;
use netget::protocol::server_registry;

/// Every protocol that declares `send_first` must either honour it or refuse it. This is the
/// list that refuses; the description a model reads has to say so, because the model chooses
/// from the description alone.
const REFUSERS: &[&str] = &[
    "ipp",
    "jsonrpc",
    "mysql",
    "mssql",
    "postgresql",
    "redis",
    "zookeeper",
];

#[test]
fn a_refusing_protocol_says_so_in_the_parameter_description() {
    let registry = server_registry::registry();
    let mut checked = 0;

    // Walk the registry rather than looking names up: `get()` keys on the registered name,
    // whose exact spelling varies by protocol, and a lookup that silently misses would make
    // this test pass by inspecting nothing.
    for (name, proto) in registry.all_protocols() {
        let name = name.to_ascii_lowercase();
        if !REFUSERS.contains(&name.as_str()) {
            continue;
        }
        let Some(param) = proto
            .get_startup_parameters()
            .into_iter()
            .find(|p| p.name == "send_first")
        else {
            continue; // the declaration was removed; nothing left to mis-describe
        };
        checked += 1;

        let d = param.description.to_ascii_lowercase();
        assert!(
            d.contains("unsupported") || d.contains("refused") || d.contains("not supported"),
            "{name} declares send_first but its description does not say it is refused — the \
             model picks parameters from the description alone, so a knob documented as \
             working and refused at runtime is the same defect in a new place: {:?}",
            param.description
        );
        assert!(
            !d.contains("not typically needed"),
            "{name} still carries the old wording, which reads as 'you may set this and it \
             will work, you just usually would not': {:?}",
            param.description
        );
    }

    assert!(
        checked > 0,
        "no protocol from REFUSERS is compiled into this build, so this test inspected \
         nothing. Run it with at least one of: {REFUSERS:?}"
    );
}
