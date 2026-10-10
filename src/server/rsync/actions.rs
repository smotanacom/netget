//! What the model decides for an rsync daemon: which modules exist, and what a module holds
//! at the path a client asked for. The daemon is read-only; the model supplies every byte,
//! nothing is read from disk.
use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter, ParameterDefinition, StartupExamples,
};
use crate::protocol::{log_template::LogTemplate, EventType, SpawnContext};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

pub const MODULES: &str = "rsync_modules";
pub const ENTRIES: &str = "rsync_send_entries";
pub const REFUSE: &str = "rsync_refuse";
/// Modules one listing may name.
pub const MAX_MODULES: usize = 256;

#[derive(Default, Clone)]
pub struct RsyncProtocol;

impl RsyncProtocol {
    pub fn new() -> Self {
        Self
    }
}

pub fn p(name: &str, type_hint: &str, description: &str, required: bool) -> Parameter {
    Parameter {
        name: name.into(),
        type_hint: type_hint.into(),
        description: description.into(),
        required,
    }
}

pub fn action(
    name: &str,
    description: &str,
    parameters: Vec<Parameter>,
    example: Value,
) -> ActionDefinition {
    let info = match name {
        MODULES => "-> rsync module list".to_string(),
        ENTRIES => "-> rsync file list".to_string(),
        REFUSE => "-> rsync refusal: {message}".to_string(),
        other => format!("-> rsync {}", other.trim_start_matches("rsync_")),
    };
    ActionDefinition {
        name: name.into(),
        description: description.into(),
        parameters,
        example,
        log_template: Some(LogTemplate::new().with_info(info)),
    }
}

pub fn modules_action() -> ActionDefinition {
    action(
        MODULES,
        "Answer the module listing (what `rsync rsync://host/` prints): each module's name and comment.",
        vec![p("modules", "array", "Modules, each {name, comment}; name is one word", true)],
        json!({"type":MODULES,"modules":[{"name":"pub","comment":"public files"}]}),
    )
}

pub fn entries_action() -> ActionDefinition {
    action(
        ENTRIES,
        "Answer with what the module holds at and under the requested path. The daemon sends the right part of it, in rsync's order.",
        vec![p(
            "entries",
            "array",
            "Each {path (relative to the module root, e.g. docs/a.txt), type (file, dir or symlink), content (files), encoding (utf8 or hex), target (symlinks), mode (octal string, e.g. \"644\"), mtime (unix seconds)}",
            true,
        )],
        json!({"type":ENTRIES,"entries":[
            {"path":"hello.txt","type":"file","content":"hello\n"},
            {"path":"docs","type":"dir"},
            {"path":"docs/readme.md","type":"file","content":"# Readme\n","mode":"644"}]}),
    )
}

pub fn refuse_action() -> ActionDefinition {
    action(
        REFUSE,
        "Refuse the request; the client prints the message and exits with an error.",
        vec![p(
            "message",
            "string",
            "Why, e.g. Unknown module 'x' or permission denied",
            true,
        )],
        json!({"type":REFUSE,"message":"Unknown module 'secret'"}),
    )
}

pub static LIST_MODULES_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "rsync_list_modules",
        "A client asked which modules this daemon offers (rsync rsync://host/). Answer rsync_modules.",
        modules_action().example.clone(),
    )
    .with_parameters(vec![p("client", "string", "The client's address and port", true)])
    .with_actions(vec![modules_action()])
});

pub static REQUEST_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "rsync_request",
        "A client asked for a path in a module, to list it or download it. Answer rsync_send_entries with what is there, or rsync_refuse.",
        entries_action().example.clone(),
    )
    .with_parameters(vec![
        p("module", "string", "The module named in the request", true),
        p("path", "string", "The path inside the module (empty for its root; a trailing / means the directory's contents)", true),
        p("recursive", "boolean", "Whether the whole subtree is wanted (-r)", true),
        p("list_only", "boolean", "Whether the client said --list-only (it also lists when it names no destination)", true),
        p("client", "string", "The client's address and port", true),
    ])
    .with_actions(vec![entries_action(), refuse_action()])
});

/// Check an action's shape (the daemon applies it to the request it answers).
pub fn check(v: &Value) -> Result<()> {
    match v["type"].as_str().unwrap_or_default() {
        MODULES => {
            let list = v["modules"].as_array().context("modules is an array")?;
            ensure!(list.len() <= MAX_MODULES, "more than {MAX_MODULES} modules");
            for m in list {
                let name = m["name"].as_str().context("each module needs a name")?;
                ensure!(
                    !name.is_empty()
                        && name.len() <= 64
                        && !name.contains(['/', ' ', '\t', '\n', '\r']),
                    "module name {name:?} must be one word without /"
                );
            }
        }
        ENTRIES => {
            let list = v["entries"].as_array().context("entries is an array")?;
            ensure!(list.len() <= super::wire::MAX_ENTRIES, "too many entries");
            for e in list {
                super::wire::entry_from_json(e, 0)?;
            }
        }
        REFUSE => {
            v["message"]
                .as_str()
                .context("rsync_refuse needs a message")?;
        }
        other => bail!("Unknown rsync action {other:?}"),
    }
    Ok(())
}

impl Protocol for RsyncProtocol {
    fn protocol_name(&self) -> &'static str {
        "rsync"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>rsync"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec![
            "rsync",
            "rsyncd",
            "rsync daemon",
            "rsync://",
            "873",
            "file sync",
        ]
    }
    fn description(&self) -> &'static str {
        "rsync daemon (rsync://, protocol 29): read-only modules whose listings and file contents the model supplies, for stock rsync clients to list and download"
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        Vec::new()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![modules_action(), entries_action(), refuse_action()]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![LIST_MODULES_EVENT.clone(), REQUEST_EVENT.clone()]
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "motd".into(),
                type_hint: "string".into(),
                description: "Message of the day, sent after the greeting (rsync prints it)".into(),
                required: false,
                example: json!("Welcome to the NetGet mirror"),
                default: None,
            },
            ParameterDefinition {
                name: "idle_timeout_secs".into(),
                type_hint: "number".into(),
                description: "Seconds to wait for the client's next line or request".into(),
                required: false,
                example: json!(60),
                default: Some(json!(super::IDLE_TIMEOUT.as_secs())),
            },
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::*;
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(873))
            .well_known_port(873)
            .answers_on_failure()
            .request_only("rsync carries one request per connection on a strictly sequenced binary stream; a message injected into it would desynchronise the client")
            .implementation("Hand-rolled rsync daemon protocol pinned at 29 (src/server/rsync/wire.rs): @RSYNCD handshake and MOTD, module listing, newline-terminated arguments, the raw checksum seed, multiplexed output, the protocol-29 file list in rsync's f_name_cmp order, whole-file transfers as literal tokens with MD4(seed ‖ data), dry runs, -c list checksums, and the three-NDX_DONE end of run with stats")
            .llm_control("Which modules exist, and the files, directories and symlinks a module holds (with their contents) at each requested path; refusals")
            .e2e_testing("tests/server/rsync: the stock rsync 3.2.7 client — module listing, --list-only, -a and -r downloads compared byte for byte, a dry run, a symlink, a refusal, and an upload refused as read-only")
            .notes("Read-only: an upload is refused ('module is read only'). -z, -R, -U, -N, -s and --files-from are refused rather than half-supported. No authentication (every module is open). Block matching against an existing destination file is not done: every file is sent whole, which is correct, just not minimal.")
            .max_inbound_bytes(wire::MAX_FILTER_BYTES)
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "Run an rsync daemon with a module 'pub' holding a README and a docs directory"
    }
    fn get_startup_examples(&self) -> StartupExamples {
        let base = json!({"type":"open_server","base_stack":"rsync","port":0,
            "instruction":"Offer one module, pub, holding hello.txt (hello) and docs/readme.md"});
        let mut static_example = base.clone();
        static_example["event_handlers"] = json!([
            {"event_pattern":"rsync_list_modules","handler":{"type":"static","actions":[modules_action().example]}},
            {"event_pattern":"rsync_request","handler":{"type":"static","actions":[entries_action().example]}}
        ]);
        let mut scripted = base.clone();
        scripted["event_handlers"] = json!([{"event_pattern":"*","handler":{"type":"script","language":"python",
            "code":"import json,sys\ni=json.load(sys.stdin); t=i['event_type_id']; e=i['event']\nif t=='rsync_list_modules': a=[{'type':'rsync_modules','modules':[{'name':'pub','comment':'public'}]}]\nelif e['module']!='pub': a=[{'type':'rsync_refuse','message':\"Unknown module '%s'\" % e['module']}]\nelse: a=[{'type':'rsync_send_entries','entries':[{'path':'hello.txt','type':'file','content':'hello\\n'}]}]\nprint(json.dumps({'actions':a}))"}}]);
        StartupExamples::new(base, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "File Transfer"
    }
}

use super::wire;

impl Server for RsyncProtocol {
    fn spawn(
        &self,
        ctx: SpawnContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::spawn(ctx))
    }
    fn execute_action(&self, action: Value) -> Result<ActionResult> {
        check(&action)?;
        // The connection applies it to the request it answers.
        Ok(ActionResult::Custom {
            name: action["type"].as_str().unwrap_or_default().to_string(),
            data: action,
        })
    }
}
