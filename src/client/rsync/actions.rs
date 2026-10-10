//! What the model can do as an rsync client against a daemon (rsync://): list its modules,
//! list a path, and fetch files.
use crate::llm::actions::{
    client_trait::{Client, ClientActionResult},
    protocol_trait::Protocol,
    ActionDefinition, ParameterDefinition, StartupExamples,
};
use crate::protocol::{ConnectContext, EventType};
use crate::server::rsync::actions::{action, p};
use crate::state::app_state::AppState;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::LazyLock;

/// The most file data one fetch brings back into an event.
pub const DEFAULT_MAX_FETCH_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_FETCH_BYTES_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Default)]
pub struct RsyncClientProtocol;
impl RsyncClientProtocol {
    pub fn new() -> Self {
        Self
    }
}

fn path_param() -> crate::llm::actions::Parameter {
    p(
        "path",
        "string",
        "Module and path, e.g. pub/ (the module's contents), pub/docs/ or pub/hello.txt",
        true,
    )
}

fn list_modules_action() -> ActionDefinition {
    action(
        "rsync_list_modules",
        "Ask the daemon which modules it offers; the answer arrives as rsync_modules.",
        vec![],
        json!({"type":"rsync_list_modules"}),
    )
}

fn list_action() -> ActionDefinition {
    action(
        "rsync_list",
        "List a path (like rsync --list-only); the answer arrives as rsync_listing.",
        vec![
            path_param(),
            p(
                "recursive",
                "boolean",
                "List the whole subtree instead of one level",
                false,
            ),
        ],
        json!({"type":"rsync_list","path":"pub/","recursive":false}),
    )
}

fn fetch_action() -> ActionDefinition {
    action(
        "rsync_fetch",
        "Download a file, or a directory's files, and see their contents in rsync_fetched.",
        vec![
            path_param(),
            p(
                "recursive",
                "boolean",
                "Fetch a directory's whole subtree (default: just the named file or one level)",
                false,
            ),
        ],
        json!({"type":"rsync_fetch","path":"pub/hello.txt"}),
    )
}

fn disconnect_action() -> ActionDefinition {
    action(
        "disconnect",
        "Stop the client (no rsync connection stays open between operations).",
        vec![],
        json!({"type":"disconnect"}),
    )
}

pub fn actions() -> Vec<ActionDefinition> {
    vec![
        list_modules_action(),
        list_action(),
        fetch_action(),
        disconnect_action(),
    ]
}

fn ev(id: &str, description: &str, params: Vec<crate::llm::actions::Parameter>) -> EventType {
    EventType::new(id, description, list_action().example.clone())
        .with_parameters(params)
        .with_actions(actions())
}

fn entries_param() -> crate::llm::actions::Parameter {
    p(
        "entries",
        "array",
        "Each {path, type (file, dir, symlink), size, mode (octal), mtime, target (symlinks)}",
        true,
    )
}

pub static READY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    ev(
        "rsync_ready",
        "The client is ready; every operation is one connection to the daemon.",
        vec![p("daemon", "string", "The daemon's address and port", true)],
    )
});

pub static MODULES_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    ev(
        "rsync_modules",
        "The daemon's module list.",
        vec![
            p("modules", "array", "Each {name, comment}", true),
            p(
                "motd",
                "string",
                "The daemon's message of the day, if any",
                false,
            ),
        ],
    )
});

pub static LISTING_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    ev(
        "rsync_listing",
        "A path's listing, in rsync's order.",
        vec![
            p("path", "string", "The path that was listed", true),
            entries_param(),
        ],
    )
});

pub static FETCHED_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    ev(
        "rsync_fetched",
        "Files downloaded from the daemon, each checked against rsync's whole-file MD4.",
        vec![
            p("path", "string", "The path that was fetched", true),
            p(
                "files",
                "array",
                "Each {path, size, content, encoding (utf8 or hex)}",
                true,
            ),
            entries_param(),
            p(
                "skipped",
                "array",
                "Files not fetched because max_fetch_bytes was reached",
                false,
            ),
        ],
    )
});

pub static ERROR_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    ev(
        "rsync_error",
        "An operation failed: the daemon refused it, or a path did not exist.",
        vec![
            p(
                "operation",
                "string",
                "rsync_list_modules, rsync_list or rsync_fetch",
                true,
            ),
            p("path", "string", "The path it was about, if any", false),
            p(
                "message",
                "string",
                "What the daemon (or the client) said",
                true,
            ),
        ],
    )
});

/// Validate an action; returns the module and the path argument for list and fetch.
pub fn check(v: &Value) -> Result<Option<(String, String)>> {
    match v["type"].as_str().unwrap_or_default() {
        "rsync_list_modules" => Ok(None),
        "rsync_list" | "rsync_fetch" => {
            let path = v["path"]
                .as_str()
                .context("path is required")?
                .trim_start_matches('/');
            let module = path.split('/').next().unwrap_or_default();
            ensure!(
                !module.is_empty(),
                "path starts with a module name, e.g. pub/"
            );
            ensure!(
                !path.split('/').any(|c| c == ".."),
                "path must not contain .."
            );
            ensure!(
                !path.contains(['\n', '\r', '\0']) && path.len() <= 1024,
                "path is one line of at most 1024 bytes"
            );
            let arg = if path.contains('/') {
                path.to_string()
            } else {
                format!("{path}/")
            };
            Ok(Some((module.to_string(), arg)))
        }
        other => bail!("Unknown rsync client action {other:?}"),
    }
}

impl Protocol for RsyncClientProtocol {
    fn protocol_name(&self) -> &'static str {
        "rsync"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>rsync"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["rsync", "rsync client", "rsync://", "mirror"]
    }
    fn description(&self) -> &'static str {
        "rsync client for rsync:// daemons: lists modules and paths and downloads files (protocol 29)"
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        actions()
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![]
    }
    fn get_event_types(&self) -> Vec<EventType> {
        vec![
            READY_EVENT.clone(),
            MODULES_EVENT.clone(),
            LISTING_EVENT.clone(),
            FETCHED_EVENT.clone(),
            ERROR_EVENT.clone(),
        ]
    }
    fn get_startup_parameters(&self) -> Vec<ParameterDefinition> {
        vec![
            ParameterDefinition {
                name: "username".into(),
                type_hint: "string".into(),
                description:
                    "User for a module that asks for authentication (auth users in rsyncd.conf)"
                        .into(),
                required: false,
                example: json!("alice"),
                default: None,
            },
            ParameterDefinition {
                name: "password".into(),
                type_hint: "string".into(),
                description:
                    "Password for that user (the daemon's secrets file); never sent in clear".into(),
                required: false,
                example: json!("s3cret"),
                default: None,
            },
            ParameterDefinition {
                name: "max_fetch_bytes".into(),
                type_hint: "number".into(),
                description:
                    "Most file bytes one fetch brings back; files beyond it are listed as skipped"
                        .into(),
                required: false,
                example: json!(1048576),
                default: Some(json!(DEFAULT_MAX_FETCH_BYTES)),
            },
        ]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::*;
        ProtocolMetadataV2::builder()
            .state(DevelopmentState::Experimental)
            .privilege_requirement(PrivilegeRequirement::None)
            .implementation("The server's protocol-29 codec (src/server/rsync/wire.rs) as the receiver: @RSYNCD handshake with MOTD and MD4 challenge-response authentication, -ld/-lr requests, demultiplexed input, the file list sorted into rsync's order, whole-file requests written concurrently with the replies they produce, every file checked against MD4(seed ‖ data), and the end-of-run exchange")
            .llm_control("Which modules and paths to list and which files to fetch, and what to do with what came back")
            .e2e_testing("tests/client/rsync: a stock rsync 3.2.7 daemon (rsync --daemon) — module listing with MOTD, a recursive listing, a fetch whose contents the model acts on, a password-protected module, and an unknown module")
            .notes("Files come back into the event, bounded by max_fetch_bytes; nothing is written to disk. No uploads. Devices and special files in a listing are skipped.")
            .max_inbound_bytes(MAX_FETCH_BYTES_LIMIT as usize)
            .build()
    }
    fn example_prompt(&self) -> &'static str {
        "List the modules on rsync://mirror.example and fetch the README from the first one"
    }
    fn get_startup_examples(&self) -> StartupExamples {
        let llm = json!({"type":"open_client","protocol":"rsync","remote_addr":"127.0.0.1:873",
            "instruction":"List the modules, then fetch pub/hello.txt and summarise it"});
        let mut static_example = llm.clone();
        static_example["event_handlers"] = json!([
            {"event_pattern":"rsync_ready","handler":{"type":"static","actions":[list_modules_action().example]}},
            {"event_pattern":"*","handler":{"type":"static","actions":[]}}
        ]);
        let mut scripted = static_example.clone();
        scripted["event_handlers"][1] = json!({"event_pattern":"rsync_modules","handler":{"type":"script","language":"python",
            "code":"import json,sys\ne=json.load(sys.stdin)['event']\nprint(json.dumps({'actions':[{'type':'rsync_list','path':m['name']+'/'} for m in e['modules'][:1]]}))"}});
        StartupExamples::new(llm, scripted, static_example)
    }
    fn group_name(&self) -> &'static str {
        "File Transfer"
    }
}

impl Client for RsyncClientProtocol {
    fn connect(
        &self,
        ctx: ConnectContext,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<std::net::SocketAddr>> + Send>>
    {
        Box::pin(super::connect(ctx))
    }
    fn execute_action(&self, v: Value) -> Result<ClientActionResult> {
        if v["type"] == "disconnect" {
            return Ok(ClientActionResult::Disconnect);
        }
        check(&v)?;
        Ok(ClientActionResult::Custom {
            name: v["type"].as_str().unwrap_or_default().to_string(),
            data: v,
        })
    }
}
