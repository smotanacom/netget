//! Git client implementation
pub mod actions;

pub use actions::GitClientProtocol;

use anyhow::{Context, Result};
use git2::{
    BranchType, Cred, FetchOptions, ObjectType, RemoteCallbacks, Repository, StatusOptions,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::client::git::actions::{
    GIT_CLIENT_CONNECTED_EVENT, GIT_OPERATION_COMPLETED_EVENT, GIT_OPERATION_ERROR_EVENT,
};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// Everything one Git client session carries between actions.
///
/// This used to be three `&mut Option<...>` locals owned by the connect task, which is
/// why nothing outside that task could run a Git action: the working repository was
/// unreachable. Behind an `Arc<Mutex<_>>` the LLM path and the injected-command loop
/// share one session, so `[ send ]` operates on the repository the LLM just cloned.
#[derive(Default)]
struct GitSession {
    repo_path: Option<PathBuf>,
    /// The `local_path` startup parameter, used as the clone destination when a
    /// `git_clone` action does not name one.
    local_path: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

/// Read one string startup parameter off the client's stored protocol data.
async fn read_field(app_state: &AppState, client_id: ClientId, key: &str) -> Option<String> {
    app_state
        .with_client_mut(client_id, |client| {
            client
                .get_protocol_field(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .await
        .flatten()
}

/// What one executed action did. Shared vocabulary between the connected-event handler
/// and the injected-command loop.
enum Applied {
    /// The action ran. `detail` is the one-line summary the dashboard shows; `output` is
    /// what the operation produced (log text, diff, branch list) for the model to read in
    /// the `git_operation_completed` event.
    Ran {
        detail: String,
        output: Option<String>,
    },
    /// The action asked to end the session.
    Disconnect,
}

impl Applied {
    /// A result with nothing for the model to read beyond the summary.
    fn ran(detail: impl Into<String>) -> Self {
        Applied::Ran {
            detail: detail.into(),
            output: None,
        }
    }
}

/// How many times an operation's result may be answered by another operation before this
/// client stops reporting. A model may reasonably chain clone -> log -> checkout; without a
/// bound, one that answers `git_status` with `git_status` would loop on the LLM forever.
const MAX_FOLLOWUP_DEPTH: u8 = 6;

/// The largest operation output handed to the model, in bytes. A diff or a log can be
/// arbitrarily long and the whole thing would crowd out the rest of the prompt.
const MAX_OUTPUT_BYTES_FOR_MODEL: usize = 8000;

/// Git client that performs Git operations
pub struct GitClient;

impl GitClient {
    /// Connect (initialize) a Git client with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        // For Git, remote_addr can be either:
        // 1. A repository URL (for cloning)
        // 2. A local path (for existing repo)
        // We'll determine this based on the instruction

        info!(
            "Git client {} initializing with target: {}",
            client_id, remote_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!("[CLIENT] Git client {} initialized", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Create a dummy socket address since Git doesn't use network sockets
        // We use a placeholder address to satisfy the return type
        let dummy_addr: SocketAddr = "127.0.0.1:0".parse()?;

        // Get initial instruction
        let instruction = app_state
            .get_instruction_for_client(client_id)
            .await
            .unwrap_or_default();

        // Seed the session from `remote_addr` when it already names a repository on
        // disk. Without this, every verb other than `git_clone` was a no-op on a
        // freshly created client - `repo_path` started `None` and only a clone could
        // ever set it - even though the documented contract is that `remote_addr` may
        // be "a local path (for existing repo)".
        //
        // `local_path` is tried the same way, and for the same reason: all three startup
        // examples tell the model to set it, and until now nothing in the client read it.
        let local_path = read_field(&app_state, client_id, "local_path").await;
        let session = Arc::new(Mutex::new(GitSession {
            repo_path: [Some(remote_addr.clone()), local_path.clone()]
                .into_iter()
                .flatten()
                .find_map(|candidate| match Repository::open(&candidate) {
                    Ok(repo) => {
                        let path = repo
                            .workdir()
                            .map(|w| w.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from(&candidate));
                        info!(
                            "Git client {} opened existing repository at {}",
                            client_id,
                            path.display()
                        );
                        Some(path)
                    }
                    Err(_) => None,
                }),
            local_path,
            // Seed the declared credentials. They were plumbed all the way to
            // `Cred::userpass_plaintext` but the session was built with
            // `..Default::default()`, so both were always `None` and authenticated
            // Git over HTTPS could never work -- a parameter declared, threaded and
            // never actually supplied. CLAUDE.md calls a declared-but-unread
            // parameter dead weight the model will try to use; this was the shape
            // where the plumbing existed and only the seeding was missing.
            username: read_field(&app_state, client_id, "username").await,
            password: read_field(&app_state, client_id, "password").await,
        }));

        // Command channel for injected actions (the dashboard's [ send ] / composer).
        // Registered BEFORE the connected-event LLM call, which a manual `*` routing rule
        // can park for minutes - the operator must be able to run a Git operation while it
        // waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            client_id,
            session.clone(),
            app_state.clone(),
            llm_client.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Spawn task to handle LLM-driven Git operations
        // Registered with AppState so stop_client can abort this task —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let llm_session = session.clone();
        let task_handle = tokio::spawn(async move {
            let protocol = Arc::new(GitClientProtocol::new());

            // Send connected event
            let event = Event::new(
                &GIT_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "repository_path": remote_addr,
                }),
            );

            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            // Initial LLM call
            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                protocol.as_ref(),
                &status_tx,
            )
            .await
            {
                Ok(ClientLlmResult {
                    actions,
                    memory_updates,
                }) => {
                    // Update memory
                    if let Some(mem) = memory_updates {
                        app_state.set_memory_for_client(client_id, mem).await;
                    }

                    // Execute initial actions through the same path injected commands
                    // use, so the git2 dispatch exists exactly once. Each result is
                    // reported back to the model as a git_operation_completed /
                    // git_operation_error event, so a clone can be followed by a log and
                    // the log's text actually reaches the model.
                    for action in actions {
                        if Self::run_and_report(
                            action,
                            &protocol,
                            &llm_session,
                            client_id,
                            &llm_client,
                            &app_state,
                            &status_tx,
                            0,
                        )
                        .await
                        {
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error for Git client {}: {}", client_id, e);
                    app_state
                        .update_client_status(client_id, ClientStatus::Error(e.to_string()))
                        .await;
                    let _ = status_tx.send("__UPDATE_UI__".to_string());
                }
            }

            // Git client doesn't have a persistent connection, so we just mark it as done
            debug!("Git client {} operations completed", client_id);
        });
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        Ok(dummy_addr)
    }

    /// Drain injected commands until the channel closes (the client was removed) or an
    /// injected `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client: Git owns
    /// no socket NetGet can write to, and every Git verb yields
    /// `ClientActionResult::Custom`. So the action goes through
    /// [`Self::execute_git_action`], the same function the connected-event path uses.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        session: Arc<Mutex<GitSession>>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;

        let protocol = Arc::new(GitClientProtocol::new());

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match Self::execute_git_action(
                &action,
                &protocol,
                &session,
                client_id,
                &llm_client,
                &app_state,
                &status_tx,
            )
            .await
            {
                // Never `Sent`: git2 talks to the repository (and, for clone/fetch/pull/
                // push, to the remote) over sockets it owns and never reports byte
                // counts for, so a number here would be invented. `Executed` carries
                // what the operation actually produced instead.
                Ok(Applied::Ran { detail, .. }) => Ok(ClientSendOutcome::Executed { detail }),
                Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                // `execute_git_action` returns `Err` both for an action the protocol
                // rejects and for a git2 operation that failed. Only the first is
                // `Rejected`; the caller sees the git2 failure as an error, not as a
                // silent success.
                Err(e) => {
                    if e.downcast_ref::<RejectedAction>().is_some() {
                        Ok(ClientSendOutcome::Rejected {
                            error: e.to_string(),
                        })
                    } else {
                        Err(e)
                    }
                }
            };

            let outcome_json = match &outcome {
                Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
                Err(e) => serde_json::json!({"error": e.to_string()}),
            };
            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![outcome_json],
                )
                .await;

            let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
            if let Err(e) = &outcome {
                error!("Git client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                break;
            }
        }

        // Nothing can be injected any more: stop the dashboard offering [ send ].
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        info!("Git client {} command loop ended", client_id);
    }

    /// Run one action and report what it produced back to the model, then run whatever the
    /// model answers with. Returns `true` when the session ended.
    ///
    /// Boxed because it is genuinely self-referential: an action raises an event, the event
    /// is answered with more actions, and those come back here. An `async fn` that awaits
    /// itself has an infinitely-sized future (E0391), and `+ Send` has to be named because
    /// this is awaited inside a `tokio::spawn`.
    #[allow(clippy::too_many_arguments)]
    fn run_and_report<'a>(
        action: serde_json::Value,
        protocol: &'a Arc<GitClientProtocol>,
        session: &'a Arc<Mutex<GitSession>>,
        client_id: ClientId,
        llm_client: &'a OllamaClient,
        app_state: &'a Arc<AppState>,
        status_tx: &'a mpsc::UnboundedSender<String>,
        depth: u8,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send + 'a>> {
        Box::pin(async move {
            let operation = action
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let event = match Self::execute_git_action(
                &action, protocol, session, client_id, llm_client, app_state, status_tx,
            )
            .await
            {
                Ok(Applied::Ran { detail, output }) => {
                    info!("Git client {}: {}", client_id, detail);
                    let mut data = serde_json::json!({
                        "operation": operation,
                        "detail": detail,
                    });
                    if let Some(output) = output {
                        data["output"] = serde_json::Value::String(crate::utils::truncate_for_llm(
                            &output,
                            MAX_OUTPUT_BYTES_FOR_MODEL,
                        ));
                    }
                    Event::new(&GIT_OPERATION_COMPLETED_EVENT, data)
                }
                Ok(Applied::Disconnect) => return true,
                Err(e) => {
                    error!("Git client {} action error: {}", client_id, e);
                    let _ =
                        status_tx.send(format!("[CLIENT] Git client {} error: {}", client_id, e));
                    Event::new(
                        &GIT_OPERATION_ERROR_EVENT,
                        serde_json::json!({
                            "operation": operation,
                            "error": e.to_string(),
                        }),
                    )
                }
            };

            if depth >= MAX_FOLLOWUP_DEPTH {
                warn!(
                    "Git client {} reached the follow-up depth limit ({}); not reporting {} \
                     to the model",
                    client_id, MAX_FOLLOWUP_DEPTH, event.event_type.id
                );
                return false;
            }

            let instruction = app_state
                .get_instruction_for_client(client_id)
                .await
                .unwrap_or_default();
            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            match call_llm_for_client(
                llm_client,
                app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                protocol.as_ref(),
                status_tx,
            )
            .await
            {
                Ok(ClientLlmResult {
                    actions,
                    memory_updates,
                }) => {
                    if let Some(mem) = memory_updates {
                        app_state.set_memory_for_client(client_id, mem).await;
                    }
                    for action in actions {
                        if Self::run_and_report(
                            action,
                            protocol,
                            session,
                            client_id,
                            llm_client,
                            app_state,
                            status_tx,
                            depth + 1,
                        )
                        .await
                        {
                            return true;
                        }
                    }
                }
                Err(e) => {
                    // Nothing is written to a wire here: git2 owns any socket and no peer is
                    // waiting on us, so silence costs nothing but the log must say why.
                    error!(
                        "LLM error for Git client {} on {}: {}",
                        client_id, event.event_type.id, e
                    );
                }
            }

            false
        })
    }

    /// Execute a Git action based on LLM decision (or on an injected command).
    ///
    /// Every git2 call is blocking - `Repository::clone` and `remote.push` do real
    /// network I/O - so the whole dispatch runs on `spawn_blocking` rather than on the
    /// async runtime's worker.
    async fn execute_git_action(
        action: &serde_json::Value,
        protocol: &Arc<GitClientProtocol>,
        session: &Arc<Mutex<GitSession>>,
        client_id: ClientId,
        _llm_client: &OllamaClient,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        let (name, data) = match protocol
            .execute_action(action.clone())
            .map_err(|e| anyhow::Error::new(RejectedAction(e.to_string())))?
        {
            ClientActionResult::Custom { name, data } => (name, data),
            ClientActionResult::Disconnect => {
                info!("Git client {} disconnecting", client_id);
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                let _ = status_tx.send("__UPDATE_UI__".to_string());
                return Ok(Applied::Disconnect);
            }
            ClientActionResult::WaitForMore => return Ok(Applied::ran("wait_for_more")),
            ClientActionResult::NoAction => return Ok(Applied::ran("no_action")),
            ClientActionResult::SendData(_) => {
                return Ok(Applied::ran(
                    "send_data has no meaning for a Git client (git2 owns any socket)",
                ))
            }
            ClientActionResult::Multiple(_) => {
                return Ok(Applied::ran(
                    "multiple results are not produced by the Git client",
                ))
            }
        };

        // Copy the session out; git2 is synchronous, so nothing awaits while we hold it.
        let (repo_path, local_path, username, password) = {
            let guard = session.lock().await;
            (
                guard.repo_path.clone(),
                guard.local_path.clone(),
                guard.username.clone(),
                guard.password.clone(),
            )
        };

        let op_status_tx = status_tx.clone();
        let op_name = name.clone();
        let outcome = tokio::task::spawn_blocking(move || {
            Self::run_git_operation(
                &op_name,
                &data,
                repo_path,
                local_path.as_deref(),
                username.as_deref(),
                password.as_deref(),
                client_id,
                &op_status_tx,
            )
        })
        .await
        .context("Git operation task panicked")??;

        if let Some(path) = outcome.repo_path {
            session.lock().await.repo_path = Some(path);
        }

        Ok(Applied::Ran {
            detail: outcome.detail,
            output: outcome.output,
        })
    }

    /// The blocking half of [`Self::execute_git_action`]: one git2 operation.
    #[allow(clippy::too_many_arguments)]
    fn run_git_operation(
        name: &str,
        data: &serde_json::Value,
        repo_path: Option<PathBuf>,
        local_path: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
        client_id: ClientId,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<OperationOutcome> {
        // Every verb but `git_clone` needs an open repository. This used to be a silent
        // `if let Some(..)` that did nothing when no repository was open, so a model (or
        // an operator) got success-shaped silence for an operation that never ran.
        let require_repo = || -> Result<&PathBuf> {
            repo_path.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "{name} needs an open repository: this client has none (clone one with \
                     git_clone, or point remote_addr at a local repository)"
                )
            })
        };

        match name {
            "git_clone" => {
                let url = data
                    .get("url")
                    .and_then(|v| v.as_str())
                    .context("Missing url")?;
                // `path` falls back to the client's `local_path` startup parameter, which is
                // what every startup example sets and what nothing used to read.
                let path = data
                    .get("path")
                    .and_then(|v| v.as_str())
                    .or(local_path)
                    .context(
                        "git_clone needs a 'path' to clone into, or a 'local_path' startup \
                         parameter on the client",
                    )?;

                info!("Git client {} cloning {} to {}", client_id, url, path);
                let _ = status_tx.send(format!(
                    "[CLIENT] Git client {} cloning {} to {}",
                    client_id, url, path
                ));

                Self::git_clone(url, path, username, password)
                    .with_context(|| format!("clone of {url} failed"))?;
                info!("Git client {} clone successful", client_id);
                let _ = status_tx.send(format!(
                    "[CLIENT] Git client {} clone successful",
                    client_id
                ));
                Ok(OperationOutcome {
                    detail: format!("git_clone {url} -> {path}"),
                    output: None,
                    repo_path: Some(PathBuf::from(path)),
                })
            }
            "git_fetch" => {
                let remote_name = data
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let path = require_repo()?;

                info!(
                    "Git client {} fetching from remote {}",
                    client_id, remote_name
                );
                Self::git_fetch(path, remote_name, username, password)
                    .with_context(|| format!("fetch from {remote_name} failed"))?;
                Ok(OperationOutcome::summary(format!(
                    "git_fetch from '{remote_name}'"
                )))
            }
            "git_status" => {
                let path = require_repo()?;
                info!("Git client {} getting status", client_id);
                let status_text = Self::git_status(path).context("status failed")?;
                info!("Git client {} status: {}", client_id, status_text);
                Ok(OperationOutcome::with_output(
                    format!(
                        "git_status: {}",
                        if status_text.trim().is_empty() {
                            "clean working tree".to_string()
                        } else {
                            status_text.trim().replace('\n', "; ")
                        }
                    ),
                    status_text,
                ))
            }
            "git_list_branches" => {
                let include_remote = data
                    .get("remote")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let path = require_repo()?;

                info!("Git client {} listing branches", client_id);
                let branches = Self::git_list_branches(path, include_remote)
                    .context("list branches failed")?;
                info!("Git client {} branches: {}", client_id, branches.join(", "));
                Ok(OperationOutcome::with_output(
                    format!("git_list_branches: {}", branches.join(", ")),
                    branches.join("\n"),
                ))
            }
            "git_log" => {
                let max_count =
                    data.get("max_count").and_then(|v| v.as_u64()).unwrap_or(10) as usize;
                let path = require_repo()?;

                info!("Git client {} getting log (max {})", client_id, max_count);
                let log_text = Self::git_log(path, max_count).context("log failed")?;
                info!("Git client {} log retrieved", client_id);
                debug!("Log:\n{}", log_text);
                // The log itself goes to the model, not just its line count: an
                // instruction like "show me the last 5 commits" is unanswerable otherwise.
                Ok(OperationOutcome::with_output(
                    format!("git_log: {} line(s)", log_text.lines().count()),
                    log_text,
                ))
            }
            "git_pull" => {
                let remote_name = data
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let branch = data.get("branch").and_then(|v| v.as_str());
                let path = require_repo()?;

                info!("Git client {} pulling from {}", client_id, remote_name);
                let result = Self::git_pull(path, remote_name, branch, username, password)
                    .with_context(|| format!("pull from {remote_name} failed"))?;
                info!("Git client {} pull: {}", client_id, result);
                Ok(OperationOutcome::with_output(
                    format!("git_pull: {result}"),
                    result,
                ))
            }
            "git_push" => {
                let remote_name = data
                    .get("remote")
                    .and_then(|v| v.as_str())
                    .unwrap_or("origin");
                let branch = data.get("branch").and_then(|v| v.as_str());
                let path = require_repo()?;

                info!("Git client {} pushing to {}", client_id, remote_name);
                let result = Self::git_push(path, remote_name, branch, username, password)
                    .with_context(|| format!("push to {remote_name} failed"))?;
                info!("Git client {} push: {}", client_id, result);
                Ok(OperationOutcome::with_output(
                    format!("git_push: {result}"),
                    result,
                ))
            }
            "git_checkout" => {
                let target = data
                    .get("target")
                    .and_then(|v| v.as_str())
                    .context("Missing 'target' field")?;
                let create = data
                    .get("create")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let path = require_repo()?;

                info!("Git client {} checking out {}", client_id, target);
                let result = Self::git_checkout(path, target, create)
                    .with_context(|| format!("checkout of {target} failed"))?;
                info!("Git client {} checkout: {}", client_id, result);
                Ok(OperationOutcome::with_output(
                    format!("git_checkout: {result}"),
                    result,
                ))
            }
            "git_delete_branch" => {
                let branch = data
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .context("Missing 'branch' field")?;
                let force = data.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                let remote = data.get("remote").and_then(|v| v.as_str());
                let path = require_repo()?;

                info!("Git client {} deleting branch {}", client_id, branch);
                let result =
                    Self::git_delete_branch(path, branch, force, remote, username, password)
                        .with_context(|| format!("delete of branch {branch} failed"))?;
                info!("Git client {} delete branch: {}", client_id, result);
                Ok(OperationOutcome::with_output(
                    format!("git_delete_branch: {result}"),
                    result,
                ))
            }
            "git_list_tags" => {
                let path = require_repo()?;
                info!("Git client {} listing tags", client_id);
                let tags = Self::git_list_tags(path).context("list tags failed")?;
                info!("Git client {} tags: {}", client_id, tags);
                Ok(OperationOutcome::with_output(
                    format!("git_list_tags: {tags}"),
                    tags,
                ))
            }
            "git_create_tag" => {
                let tag_name = data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .context("Missing 'name' field")?;
                let target = data.get("target").and_then(|v| v.as_str());
                let message = data.get("message").and_then(|v| v.as_str());
                let path = require_repo()?;

                info!("Git client {} creating tag {}", client_id, tag_name);
                let result = Self::git_create_tag(path, tag_name, target, message)
                    .with_context(|| format!("creation of tag {tag_name} failed"))?;
                info!("Git client {} create tag: {}", client_id, result);
                Ok(OperationOutcome::with_output(
                    format!("git_create_tag: {result}"),
                    result,
                ))
            }
            "git_diff" => {
                let target = data.get("target").and_then(|v| v.as_str());
                let staged = data
                    .get("staged")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let path = require_repo()?;

                info!("Git client {} getting diff", client_id);
                let diff_text = Self::git_diff(path, target, staged).context("diff failed")?;
                info!("Git client {} diff: {}", client_id, diff_text);
                Ok(OperationOutcome::with_output(
                    format!("git_diff: {} byte(s) of diff", diff_text.len()),
                    diff_text,
                ))
            }
            other => {
                debug!("Unhandled Git action: {}", other);
                Ok(OperationOutcome::summary(format!(
                    "custom result '{other}' is not handled by the Git client"
                )))
            }
        }
    }

    /// Clone a Git repository
    fn git_clone(
        url: &str,
        path: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<Repository> {
        let mut callbacks = RemoteCallbacks::new();

        // Set up authentication callback
        if let (Some(user), Some(pass)) = (username, password) {
            let user = user.to_string();
            let pass = pass.to_string();
            callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                Cred::userpass_plaintext(&user, &pass)
            });
        }

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        let mut builder = git2::build::RepoBuilder::new();
        builder.fetch_options(fetch_options);

        let repo = builder.clone(url, std::path::Path::new(path))?;
        Ok(repo)
    }

    /// Fetch from a remote
    fn git_fetch(
        path: &PathBuf,
        remote_name: &str,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<()> {
        let repo = Repository::open(path)?;
        let mut remote = repo.find_remote(remote_name)?;

        let mut callbacks = RemoteCallbacks::new();
        if let (Some(user), Some(pass)) = (username, password) {
            let user = user.to_string();
            let pass = pass.to_string();
            callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                Cred::userpass_plaintext(&user, &pass)
            });
        }

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        remote.fetch(
            &["refs/heads/*:refs/remotes/origin/*"],
            Some(&mut fetch_options),
            None,
        )?;
        Ok(())
    }

    /// Get repository status
    fn git_status(path: &PathBuf) -> Result<String> {
        let repo = Repository::open(path)?;
        let statuses = repo.statuses(Some(StatusOptions::new().include_untracked(true)))?;

        let mut result = String::new();
        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                let status = entry.status();
                result.push_str(&format!("{:?} - {}\n", status, path));
            }
        }

        if result.is_empty() {
            result = "Working tree clean".to_string();
        }

        Ok(result)
    }

    /// List branches
    fn git_list_branches(path: &PathBuf, include_remote: bool) -> Result<Vec<String>> {
        let repo = Repository::open(path)?;
        let mut branches = Vec::new();

        let local_branches = repo.branches(Some(BranchType::Local))?;
        for branch in local_branches {
            let (branch, _) = branch?;
            if let Some(name) = branch.name()? {
                branches.push(name.to_string());
            }
        }

        if include_remote {
            let remote_branches = repo.branches(Some(BranchType::Remote))?;
            for branch in remote_branches {
                let (branch, _) = branch?;
                if let Some(name) = branch.name()? {
                    branches.push(name.to_string());
                }
            }
        }

        Ok(branches)
    }

    /// Get commit log
    fn git_log(path: &PathBuf, max_count: usize) -> Result<String> {
        let repo = Repository::open(path)?;
        let mut revwalk = repo.revwalk()?;
        revwalk.push_head()?;

        let mut result = String::new();
        let mut count = 0;

        for oid in revwalk {
            if count >= max_count {
                break;
            }

            let oid = oid?;
            let commit = repo.find_object(oid, Some(ObjectType::Commit))?;
            let commit = commit.as_commit().context("Not a commit")?;

            let time = commit.time();
            let datetime = chrono::DateTime::from_timestamp(time.seconds(), 0)
                .map(|dt| dt.naive_utc())
                .unwrap_or_default();

            result.push_str(&format!(
                "commit {}\nAuthor: {}\nDate: {}\n\n    {}\n\n",
                oid,
                commit.author(),
                datetime.format("%Y-%m-%d %H:%M:%S"),
                commit.message().unwrap_or("")
            ));

            count += 1;
        }

        Ok(result)
    }

    /// Pull updates from remote (fetch + merge)
    fn git_pull(
        path: &PathBuf,
        remote_name: &str,
        branch_name: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<String> {
        let repo = Repository::open(path)?;

        // Get current branch if not specified
        let current_branch_name = if let Some(branch) = branch_name {
            branch.to_string()
        } else {
            let head = repo.head()?;
            head.shorthand()
                .context("Could not get current branch name")?
                .to_string()
        };

        // Fetch first
        let mut remote = repo.find_remote(remote_name)?;
        let mut callbacks = RemoteCallbacks::new();

        if let (Some(user), Some(pass)) = (username, password) {
            let user = user.to_string();
            let pass = pass.to_string();
            callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                Cred::userpass_plaintext(&user, &pass)
            });
        }

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        remote.fetch(
            &[format!(
                "refs/heads/{}:refs/remotes/{}/{}",
                current_branch_name, remote_name, current_branch_name
            )],
            Some(&mut fetch_options),
            None,
        )?;

        // Now merge the fetched changes
        let fetch_head = repo.find_reference("FETCH_HEAD")?;
        let fetch_commit = repo.reference_to_annotated_commit(&fetch_head)?;

        // Perform the merge analysis
        let (analysis, _) = repo.merge_analysis(&[&fetch_commit])?;

        if analysis.is_up_to_date() {
            Ok("Already up to date".to_string())
        } else if analysis.is_fast_forward() {
            // Fast-forward merge
            let refname = format!("refs/heads/{}", current_branch_name);
            let mut reference = repo.find_reference(&refname)?;
            reference.set_target(fetch_commit.id(), "pull: Fast-forward")?;
            repo.set_head(&refname)?;
            repo.checkout_head(Some(git2::build::CheckoutBuilder::default().force()))?;
            Ok(format!("Fast-forward merge completed"))
        } else if analysis.is_normal() {
            // Normal merge (requires commit)
            Ok("Merge required but auto-merge not implemented. Please manually merge.".to_string())
        } else {
            Ok("Unknown merge analysis result".to_string())
        }
    }

    /// Push commits to remote
    fn git_push(
        path: &PathBuf,
        remote_name: &str,
        branch_name: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<String> {
        let repo = Repository::open(path)?;

        // Get current branch if not specified
        let current_branch_name = if let Some(branch) = branch_name {
            branch.to_string()
        } else {
            let head = repo.head()?;
            head.shorthand()
                .context("Could not get current branch name")?
                .to_string()
        };

        let mut remote = repo.find_remote(remote_name)?;
        let mut callbacks = RemoteCallbacks::new();

        if let (Some(user), Some(pass)) = (username, password) {
            let user = user.to_string();
            let pass = pass.to_string();
            callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                Cred::userpass_plaintext(&user, &pass)
            });
        }

        let mut push_options = git2::PushOptions::new();
        push_options.remote_callbacks(callbacks);

        // Push the branch
        let refspec = format!(
            "refs/heads/{}:refs/heads/{}",
            current_branch_name, current_branch_name
        );
        remote.push(&[&refspec], Some(&mut push_options))?;

        Ok(format!(
            "Successfully pushed {} to {}",
            current_branch_name, remote_name
        ))
    }

    /// Checkout a branch or create a new branch
    fn git_checkout(path: &PathBuf, target: &str, create: bool) -> Result<String> {
        let repo = Repository::open(path)?;

        if create {
            // Create and checkout new branch
            let head = repo.head()?;
            let oid = head.target().context("Could not get HEAD target")?;
            let commit = repo.find_commit(oid)?;

            repo.branch(target, &commit, false)?;

            let obj = repo.revparse_single(&format!("refs/heads/{}", target))?;
            repo.checkout_tree(&obj, None)?;
            repo.set_head(&format!("refs/heads/{}", target))?;

            Ok(format!("Created and checked out new branch: {}", target))
        } else {
            // Checkout existing branch or commit
            let obj = repo.revparse_single(target)?;
            repo.checkout_tree(&obj, None)?;

            // Try to set HEAD to the branch reference if it exists
            let refname = format!("refs/heads/{}", target);
            if repo.find_reference(&refname).is_ok() {
                repo.set_head(&refname)?;
                Ok(format!("Checked out branch: {}", target))
            } else {
                // Detached HEAD for commit
                repo.set_head_detached(obj.id())?;
                Ok(format!("Checked out commit: {} (detached HEAD)", target))
            }
        }
    }

    /// Delete a local or remote branch
    fn git_delete_branch(
        path: &PathBuf,
        branch_name: &str,
        force: bool,
        remote_name: Option<&str>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Result<String> {
        let repo = Repository::open(path)?;
        let mut result_msgs = Vec::new();

        // Delete local branch if no remote specified, or always delete local
        if remote_name.is_none() {
            let mut branch = repo.find_branch(branch_name, git2::BranchType::Local)?;

            // Check if branch is fully merged (unless force is true)
            if !force {
                let head = repo.head()?;
                let head_commit = head.peel_to_commit()?;

                let branch_ref = branch.get();
                let branch_commit = branch_ref.peel_to_commit()?;

                // Check if branch is merged into HEAD
                let merge_base = repo.merge_base(head_commit.id(), branch_commit.id())?;
                if merge_base != branch_commit.id() {
                    anyhow::bail!(
                        "Branch '{}' is not fully merged. Use force=true to delete anyway.",
                        branch_name
                    );
                }
            }

            branch.delete()?;
            result_msgs.push(format!("Deleted local branch: {}", branch_name));
        }

        // Delete remote branch if specified
        if let Some(remote) = remote_name {
            let mut remote_obj = repo.find_remote(remote)?;

            let mut callbacks = RemoteCallbacks::new();
            if let (Some(user), Some(pass)) = (username, password) {
                let user = user.to_string();
                let pass = pass.to_string();
                callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
                    Cred::userpass_plaintext(&user, &pass)
                });
            }

            let mut push_options = git2::PushOptions::new();
            push_options.remote_callbacks(callbacks);

            // Push empty refspec to delete remote branch
            let refspec = format!(":refs/heads/{}", branch_name);
            remote_obj.push(&[&refspec], Some(&mut push_options))?;

            result_msgs.push(format!("Deleted remote branch: {}/{}", remote, branch_name));
        }

        Ok(result_msgs.join("; "))
    }

    /// List all tags in the repository
    fn git_list_tags(path: &PathBuf) -> Result<String> {
        let repo = Repository::open(path)?;
        let tag_names = repo.tag_names(None)?;

        let mut tags = Vec::new();
        for tag_name in tag_names.iter() {
            if let Some(name) = tag_name {
                tags.push(name.to_string());
            }
        }

        if tags.is_empty() {
            Ok("No tags found".to_string())
        } else {
            Ok(format!("Tags ({}): {}", tags.len(), tags.join(", ")))
        }
    }

    /// Create a new tag
    fn git_create_tag(
        path: &PathBuf,
        tag_name: &str,
        target: Option<&str>,
        message: Option<&str>,
    ) -> Result<String> {
        let repo = Repository::open(path)?;

        // Resolve target (default to HEAD)
        let target_str = target.unwrap_or("HEAD");
        let obj = repo.revparse_single(target_str)?;
        let target_commit = obj.peel_to_commit()?;

        // Get git signature for annotated tags
        let sig = repo.signature().or_else(|_| {
            // Fallback signature if not configured
            git2::Signature::now("NetGet", "netget@localhost")
        })?;

        if let Some(msg) = message {
            // Create annotated tag
            repo.tag(tag_name, &obj, &sig, msg, false)?;
            Ok(format!(
                "Created annotated tag '{}' at {} with message: {}",
                tag_name,
                target_commit.id(),
                msg
            ))
        } else {
            // Create lightweight tag
            repo.tag_lightweight(tag_name, &obj, false)?;
            Ok(format!(
                "Created lightweight tag '{}' at {}",
                tag_name,
                target_commit.id()
            ))
        }
    }

    /// View differences in the repository
    fn git_diff(path: &PathBuf, target: Option<&str>, staged: bool) -> Result<String> {
        let repo = Repository::open(path)?;

        let diff = if staged {
            // Show staged changes (index vs HEAD)
            let head_tree = repo.head()?.peel_to_tree()?;
            let mut index = repo.index()?;
            let index_tree = repo.find_tree(index.write_tree()?)?;
            repo.diff_tree_to_tree(Some(&head_tree), Some(&index_tree), None)?
        } else if let Some(target_ref) = target {
            // Show diff against specific target
            let target_obj = repo.revparse_single(target_ref)?;
            let target_tree = target_obj.peel_to_tree()?;
            let head_tree = repo.head()?.peel_to_tree()?;
            repo.diff_tree_to_tree(Some(&target_tree), Some(&head_tree), None)?
        } else {
            // Show working directory changes (working dir vs index)
            repo.diff_index_to_workdir(None, None)?
        };

        // Format diff statistics
        let stats = diff.stats()?;
        let files_changed = stats.files_changed();
        let insertions = stats.insertions();
        let deletions = stats.deletions();

        // Get patch text
        let mut patch_text = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let origin = line.origin();
            let content = std::str::from_utf8(line.content()).unwrap_or("");

            match origin {
                '+' | '-' | ' ' => {
                    patch_text.push(origin);
                    patch_text.push_str(content);
                }
                _ => {
                    patch_text.push_str(content);
                }
            }
            true
        })?;

        if patch_text.is_empty() {
            Ok("No differences found".to_string())
        } else {
            Ok(format!(
                "Diff: {} file(s) changed, {} insertion(s), {} deletion(s)\n\n{}",
                files_changed,
                insertions,
                deletions,
                patch_text.lines().take(50).collect::<Vec<_>>().join("\n")
            ))
        }
    }
}

/// What one git2 operation produced.
struct OperationOutcome {
    /// One-line summary for the operator and the log.
    detail: String,
    /// The text the operation produced — commit log, diff, branch list, status — for the
    /// model to read. `None` for operations whose whole result fits in `detail`.
    ///
    /// Before this existed, `git_log` reported "12 line(s)" and threw the log away, so a
    /// model instructed to "show me the last 5 commits" could never see one.
    output: Option<String>,
    /// The repository the operation established, when it established one (`git_clone`).
    repo_path: Option<PathBuf>,
}

impl OperationOutcome {
    fn summary(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            output: None,
            repo_path: None,
        }
    }

    fn with_output(detail: impl Into<String>, output: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
            output: Some(output.into()),
            repo_path: None,
        }
    }
}

/// An action the Git protocol itself refused (unknown type, missing field), as opposed
/// to a git2 operation that ran and failed. The injected-command loop maps the first to
/// `ClientSendOutcome::Rejected` and the second to an error.
#[derive(Debug)]
struct RejectedAction(String);

impl std::fmt::Display for RejectedAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for RejectedAction {}
