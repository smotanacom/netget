//! Kubernetes API client implementation
pub mod actions;

pub use actions::KubernetesClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::client::kubernetes::actions::K8S_CLIENT_RESOURCE_RECEIVED_EVENT;
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::actions::protocol_trait::Protocol;
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};
use crate::utils::truncate::truncate_for_log;

/// Expand a leading `~/` in a kubeconfig path.
///
/// The parameter's own example is `~/.kube/config`, and `std::fs` does no tilde expansion —
/// a path taken literally would fail with "No such file or directory" on the one value the
/// documentation suggests.
fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => match std::env::var_os("HOME") {
            Some(home) => std::path::PathBuf::from(home).join(rest),
            None => std::path::PathBuf::from(path),
        },
        None => std::path::PathBuf::from(path),
    }
}

/// Kubernetes client that interacts with Kubernetes API server
pub struct KubernetesClient;

impl KubernetesClient {
    /// Connect to a Kubernetes cluster with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        _llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // For Kubernetes, "connection" means establishing API client configuration
        // The kube client is stateless and makes requests on-demand

        info!(
            "Kubernetes client {} initializing for cluster {}",
            client_id, remote_addr
        );

        // `kubeconfig` and `namespace` were both declared and neither was read. `kubeconfig`
        // named a file this client then ignored - it always ran `try_default()`, which reads
        // `$KUBECONFIG` or `~/.kube/config` - and `namespace` was overwritten by a hardcoded
        // "default" one line later, so every operation that did not name a namespace itself
        // went to `default` whatever the operator asked for.
        let (kubeconfig_path, namespace) = match &startup_params {
            Some(params) => (
                params.get_optional_string("kubeconfig")?,
                params.get_optional_string("namespace")?,
            ),
            None => (None, None),
        };

        let k8s_client = match &kubeconfig_path {
            // An explicit kubeconfig file, read from where the operator said it is.
            Some(path) => {
                let path = expand_home(path);
                let kubeconfig = kube::config::Kubeconfig::read_from(&path).map_err(|e| {
                    anyhow::anyhow!("Failed to read kubeconfig at {}: {}", path.display(), e)
                })?;
                let config = kube::Config::from_custom_kubeconfig(
                    kubeconfig,
                    &kube::config::KubeConfigOptions::default(),
                )
                .await
                .map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to build Kubernetes configuration from {}: {}",
                        path.display(),
                        e
                    )
                })?;
                let client = kube::Client::try_from(config).map_err(|e| {
                    anyhow::anyhow!(
                        "Failed to connect to Kubernetes using kubeconfig {}: {}",
                        path.display(),
                        e
                    )
                })?;
                info!(
                    "Kubernetes client {} connected using kubeconfig {}",
                    client_id,
                    path.display()
                );
                client
            }
            None if remote_addr == "default" || remote_addr == "~/.kube/config" => {
                // Use default kubeconfig ($KUBECONFIG, else ~/.kube/config)
                match kube::Client::try_default().await {
                    Ok(client) => {
                        info!(
                            "Kubernetes client {} connected using default kubeconfig",
                            client_id
                        );
                        client
                    }
                    Err(e) => {
                        error!(
                            "Failed to connect to Kubernetes using default kubeconfig: {}",
                            e
                        );
                        return Err(anyhow::anyhow!(
                            "Failed to connect to Kubernetes: {}. Make sure kubeconfig is configured.",
                            e
                        ));
                    }
                }
            }
            None => {
                return Err(anyhow::anyhow!(
                    "Kubernetes address '{}' is not understood. Use 'default' to read \
                     $KUBECONFIG (else ~/.kube/config), or pass the `kubeconfig` startup \
                     parameter naming a kubeconfig file.",
                    remote_addr
                ));
            }
        };

        // The `namespace` startup parameter is the default for every operation that does
        // not name one itself.
        let namespace = namespace.unwrap_or_else(|| "default".to_string());

        // Store client configuration in protocol_data
        app_state
            .with_client_mut(client_id, |client| {
                client
                    .set_protocol_field("k8s_client".to_string(), serde_json::json!("initialized"));
                client.set_protocol_field("namespace".to_string(), serde_json::json!(namespace));
                client
                    .set_protocol_field("cluster_url".to_string(), serde_json::json!(remote_addr));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        Log::new(Some(&status_tx))
            .info(format!("Kubernetes client {} ready for cluster", client_id));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send ] row).
        //
        // The `kube::Client` built above is carried into the command task rather than
        // discarded: it is cheaply cloneable and internally shared, so both paths use
        // one configured handle instead of re-running `try_default()` (and re-reading
        // the kubeconfig) per operation.
        //
        // The command loop also replaces the 5s "has the client been removed yet"
        // poll: `remove_client` drops the handle, the sender goes with it, and
        // `recv()` returns None - promptly, and without a timer.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let task_registrar = app_state.clone();
        let conn_llm = _llm_client.clone();
        let conn_k8s = k8s_client.clone();
        let task_handle = tokio::spawn(Self::command_loop(
            command_rx,
            k8s_client,
            client_id,
            app_state.clone(),
            _llm_client.clone(),
            status_tx.clone(),
        ));
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        // Raise the connected event.
        //
        // K8S_CLIENT_CONNECTED_EVENT was declared and nothing raised it, so the model was
        // never consulted when a Kubernetes client came up.
        //
        // From a registered task, not inline: a dashboard-created client defaults to a
        // `*` -> manual rule and awaiting a parked answer would block creation. Operations
        // go through run_operation_once, which raises no event.
        let conn_state = app_state.clone();
        let conn_status = status_tx.clone();
        let conn_task = tokio::spawn(async move {
            let Some(instruction) = conn_state.get_instruction_for_client(client_id).await else {
                return;
            };
            let protocol = crate::client::kubernetes::actions::KubernetesClientProtocol::new();
            let event = Event::new(
                &crate::client::kubernetes::actions::K8S_CLIENT_CONNECTED_EVENT,
                serde_json::json!({}),
            );
            match crate::client::llm_budget::call_llm_for_client(
                &conn_llm,
                &conn_state,
                client_id.to_string(),
                &instruction,
                "",
                Some(&event),
                &protocol,
                &conn_status,
            )
            .await
            {
                Ok(result) => {
                    if let Some(mem) = result.memory_updates {
                        conn_state.set_memory_for_client(client_id, mem).await;
                    }
                    use crate::llm::actions::client_trait::{Client, ClientActionResult};
                    for action in result.actions {
                        let Ok(ClientActionResult::Custom { name, data }) =
                            protocol.execute_action(action.clone())
                        else {
                            continue;
                        };
                        if name != "k8s_operation" {
                            continue;
                        }
                        let str_of =
                            |k: &str| data.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
                        if let Err(e) = Self::run_operation_once(
                            &conn_k8s,
                            client_id,
                            str_of("operation").unwrap_or_default(),
                            str_of("resource_type").unwrap_or_default(),
                            str_of("namespace"),
                            str_of("name"),
                            data.get("spec").cloned().filter(|v| !v.is_null()),
                            str_of("label_selector"),
                            &conn_state,
                        )
                        .await
                        {
                            error!(
                                "Kubernetes client {} connect-time operation failed: {}",
                                client_id, e
                            );
                        }
                    }
                }
                Err(e) => error!(
                    "Kubernetes client {} LLM error on connected event: {}",
                    client_id, e
                ),
            }
        });
        task_registrar
            .register_client_task(client_id, conn_task)
            .await;

        // Return a dummy local address (Kubernetes API is HTTP-based, connectionless)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Execute a Kubernetes API operation against an already-configured client.
    ///
    /// Takes the `kube::Client` rather than calling `try_default()` itself, so the
    /// handle built once at connect time is the one every operation uses.
    #[allow(clippy::too_many_arguments)]
    /// Run one operation. Raises no event, calls no LLM.
    ///
    /// Deliberately free of any path back into `notify_response`: that would make the async
    /// type self-referential (notify -> operation -> notify), which rustc cannot prove
    /// `Send`, so `tokio::spawn` refuses it. Follow-up actions the model returns for a
    /// response event run through here, which breaks that cycle and bounds the loop.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_operation_once(
        k8s_client: &kube::Client,
        client_id: ClientId,
        operation: String,
        resource_type: String,
        namespace: Option<String>,
        name: Option<String>,
        data: Option<serde_json::Value>,
        label_selector: Option<String>,
        app_state: &Arc<AppState>,
    ) -> Result<serde_json::Value> {
        // Determine namespace
        let ns = if let Some(n) = namespace {
            n
        } else {
            app_state
                .with_client_mut(client_id, |client| {
                    client
                        .get_protocol_field("namespace")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .await
                .flatten()
                .unwrap_or_else(|| "default".to_string())
        };

        info!(
            "Kubernetes client {} executing {} on {} in namespace {}",
            client_id, operation, resource_type, ns
        );

        // Execute operation based on resource type and operation
        let result = match (operation.as_str(), resource_type.as_str()) {
            ("list", "pods") => Self::list_pods(k8s_client, &ns, label_selector.as_deref()).await,
            ("get", "pod") => {
                if let Some(pod_name) = name {
                    Self::get_pod(k8s_client, &ns, &pod_name).await
                } else {
                    Err(anyhow::anyhow!("Pod name required for get operation"))
                }
            }
            ("logs", "pod") => {
                if let Some(pod_name) = name {
                    Self::get_pod_logs(k8s_client, &ns, &pod_name).await
                } else {
                    Err(anyhow::anyhow!("Pod name required for logs operation"))
                }
            }
            ("create", "pod") => {
                if let Some(pod_spec) = data {
                    Self::create_pod(k8s_client, &ns, pod_spec).await
                } else {
                    Err(anyhow::anyhow!(
                        "Pod specification required for create operation"
                    ))
                }
            }
            ("delete", "pod") => {
                if let Some(pod_name) = name {
                    Self::delete_pod(k8s_client, &ns, &pod_name).await
                } else {
                    Err(anyhow::anyhow!("Pod name required for delete operation"))
                }
            }
            ("list", "deployments") => {
                Self::list_deployments(k8s_client, &ns, label_selector.as_deref()).await
            }
            ("list", "services") => {
                Self::list_services(k8s_client, &ns, label_selector.as_deref()).await
            }
            _ => Err(anyhow::anyhow!(
                "Unsupported operation '{}' on resource type '{}'",
                operation,
                resource_type
            )),
        };

        result
    }

    pub async fn execute_operation(
        k8s_client: &kube::Client,
        client_id: ClientId,
        operation: String,
        resource_type: String,
        namespace: Option<String>,
        name: Option<String>,
        data: Option<serde_json::Value>,
        label_selector: Option<String>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<serde_json::Value> {
        // Resolve the namespace here too: the notify payload below reports it, and
        // `run_operation_once` takes ownership of the Option.
        let ns = if let Some(n) = namespace.clone() {
            n
        } else {
            app_state
                .with_client_mut(client_id, |client| {
                    client
                        .get_protocol_field("namespace")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                })
                .await
                .flatten()
                .unwrap_or_else(|| "default".to_string())
        };

        let result = Self::run_operation_once(
            k8s_client,
            client_id,
            operation.clone(),
            resource_type.clone(),
            namespace,
            name,
            data,
            label_selector,
            &app_state,
        )
        .await;

        match result {
            Ok(response) => {
                info!("Kubernetes client {} operation successful", client_id);

                // Raise the response event from its own registered task rather than
                // inline. A dashboard-created client defaults to a `*` -> manual routing
                // rule, so this LLM call can park for minutes waiting for a human;
                // awaiting it here would wedge the command loop and make an injected
                // action that in fact succeeded look to the dashboard like a timeout.
                let event_data = serde_json::json!({
                    "operation": operation,
                    "resource_type": resource_type,
                    "namespace": ns,
                    "response": response,
                });
                let notify = tokio::spawn(Self::notify_response(
                    client_id,
                    k8s_client.clone(),
                    event_data,
                    app_state.clone(),
                    llm_client,
                    status_tx,
                ));
                app_state.register_client_task(client_id, notify).await;

                Ok(response)
            }
            Err(e) => {
                Log::new(Some(&status_tx)).error(format!(
                    "Kubernetes client {} operation failed: {}",
                    client_id, e
                ));
                Err(e)
            }
        }
    }

    /// Raise `k8s_resource_received` and fold in any memory update. Spawned, never
    /// awaited by a caller that holds the command loop: an event handler may park this
    /// call for a human answer.
    async fn notify_response(
        client_id: ClientId,
        k8s_client: kube::Client,
        event_data: serde_json::Value,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };
        let protocol = crate::client::kubernetes::actions::KubernetesClientProtocol::new();
        let event = Event::new(&K8S_CLIENT_RESOURCE_RECEIVED_EVENT, event_data);
        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        match call_llm_for_client(
            &llm_client,
            &app_state,
            client_id.to_string(),
            &instruction,
            &memory,
            Some(&event),
            &protocol,
            &status_tx,
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

                // Execute what the model asked for. These were discarded, so answering
                // k8s_resource_received did nothing -- a model that listed pods and wanted
                // to fetch the logs of one it had just seen was ignored.
                //
                // Dispatch goes through `run_operation_once`, which raises no event: that
                // bounds the loop and keeps the async type non-recursive, which is what
                // tokio::spawn's Send bound requires.
                use crate::llm::actions::client_trait::{Client, ClientActionResult};
                for action in actions {
                    let Ok(ClientActionResult::Custom { name, data }) =
                        protocol.execute_action(action.clone())
                    else {
                        continue;
                    };
                    if name != "k8s_operation" {
                        continue;
                    }
                    let str_of =
                        |k: &str| data.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
                    let outcome = Self::run_operation_once(
                        &k8s_client,
                        client_id,
                        str_of("operation").unwrap_or_default(),
                        str_of("resource_type").unwrap_or_default(),
                        str_of("namespace"),
                        str_of("name"),
                        data.get("spec").cloned().filter(|v| !v.is_null()),
                        str_of("label_selector"),
                        &app_state,
                    )
                    .await;
                    match outcome {
                        Ok(v) => info!(
                            "Kubernetes client {} follow-up -> {}",
                            client_id,
                            crate::utils::truncate_for_log(&v.to_string(), 120)
                        ),
                        Err(e) => error!(
                            "Kubernetes client {} follow-up action failed: {}",
                            client_id, e
                        ),
                    }
                }
            }
            Err(e) => {
                error!("LLM error for Kubernetes client {}: {}", client_id, e);
            }
        }
    }

    /// Apply one executed action against the cluster. Shared by every path that can
    /// produce an action, so an injected one behaves identically to an LLM one.
    ///
    /// `kube` owns the socket and reports no wire byte count, so a completed
    /// operation is `Executed { detail }` naming the operation and its result -
    /// never `Sent`, which would be a fabricated byte count.
    async fn apply_action(
        result: ClientActionResult,
        k8s_client: &kube::Client,
        client_id: ClientId,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> ClientSendOutcome {
        match result {
            ClientActionResult::Custom { name, data } if name == "k8s_operation" => {
                let operation = data
                    .get("operation")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let resource_type = data
                    .get("resource_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let namespace = data
                    .get("namespace")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let resource_name = data
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let label_selector = data
                    .get("label_selector")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                // `execute_action` names the pod body "spec"; the operation takes it
                // as its generic `data` payload.
                let spec = data.get("spec").cloned().filter(|v| !v.is_null());

                let label = format!("{} {}", operation, resource_type);
                match Self::execute_operation(
                    k8s_client,
                    client_id,
                    operation,
                    resource_type,
                    namespace,
                    resource_name,
                    spec,
                    label_selector,
                    app_state.clone(),
                    llm_client.clone(),
                    status_tx.clone(),
                )
                .await
                {
                    Ok(value) => ClientSendOutcome::Executed {
                        detail: format!(
                            "{} completed: {}",
                            label,
                            truncate_for_log(&value.to_string(), 200)
                        ),
                    },
                    Err(e) => ClientSendOutcome::Executed {
                        detail: format!(
                            "{} failed: {}",
                            label,
                            truncate_for_log(&e.to_string(), 200)
                        ),
                    },
                }
            }
            ClientActionResult::Custom { name, .. } => ClientSendOutcome::Executed {
                detail: format!("unknown Kubernetes result '{}' was not applied", name),
            },
            ClientActionResult::Disconnect => ClientSendOutcome::Disconnected,
            ClientActionResult::WaitForMore => ClientSendOutcome::Executed {
                detail: "wait_for_more".to_string(),
            },
            ClientActionResult::NoAction => ClientSendOutcome::Executed {
                detail: "no_action".to_string(),
            },
            ClientActionResult::SendData(_) => ClientSendOutcome::Executed {
                detail: "send_data has no meaning for the Kubernetes client: it speaks the \
                         apiserver REST API through `kube`, not a socket this client owns"
                    .to_string(),
            },
            ClientActionResult::Multiple(_) => ClientSendOutcome::Executed {
                detail: "the Kubernetes client's own verbs never produce Multiple; nothing was \
                         applied"
                    .to_string(),
            },
        }
    }

    /// Drain injected commands until the channel closes (the client was removed) or
    /// an injected `disconnect` ends the session.
    ///
    /// `command_support::handle_stream_client_command` cannot serve this client:
    /// every Kubernetes verb yields `ClientActionResult::Custom` and there is no
    /// write half to put bytes on. Actions therefore go through
    /// [`Self::apply_action`], and the outcome is logged and replied exactly the way
    /// the generic arm does it.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        k8s_client: kube::Client,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let protocol = crate::client::kubernetes::actions::KubernetesClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => ClientSendOutcome::Rejected {
                    error: e.to_string(),
                },
                Ok(result) => {
                    Self::apply_action(
                        result,
                        &k8s_client,
                        client_id,
                        &app_state,
                        &llm_client,
                        &status_tx,
                    )
                    .await
                }
            };
            let disconnected = matches!(outcome, ClientSendOutcome::Disconnected);

            app_state
                .record_access_log(
                    AccessLogOwner::Client(client_id.as_u32()),
                    protocol.protocol_name(),
                    None,
                    "injected_action",
                    action,
                    vec![serde_json::to_value(&outcome).unwrap_or(serde_json::Value::Null)],
                )
                .await;
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, Ok(outcome));

            if disconnected {
                info!(
                    "Kubernetes client {} disconnecting on injected action",
                    client_id
                );
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                break;
            }
        }

        info!("Kubernetes client {} command loop stopped", client_id);
        // Never leave the dashboard offering [ send ] into a client that is gone.
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// List pods in a namespace
    async fn list_pods(
        client: &kube::Client,
        namespace: &str,
        label_selector: Option<&str>,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        let mut list_params = kube::api::ListParams::default();
        if let Some(selector) = label_selector {
            list_params = list_params.labels(selector);
        }

        let pod_list = pods
            .list(&list_params)
            .await
            .context("Failed to list pods")?;

        // Convert to JSON
        let pod_names: Vec<String> = pod_list
            .items
            .iter()
            .filter_map(|pod| pod.metadata.name.clone())
            .collect();

        Ok(serde_json::json!({
            "count": pod_list.items.len(),
            "pods": pod_names,
        }))
    }

    /// Get a specific pod
    async fn get_pod(
        client: &kube::Client,
        namespace: &str,
        name: &str,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);
        let pod = pods
            .get(name)
            .await
            .with_context(|| format!("Failed to get pod {}", name))?;

        // Extract relevant info
        let status = pod
            .status
            .as_ref()
            .and_then(|s| s.phase.clone())
            .unwrap_or_else(|| "Unknown".to_string());

        Ok(serde_json::json!({
            "name": name,
            "namespace": namespace,
            "status": status,
        }))
    }

    /// Get pod logs
    async fn get_pod_logs(
        client: &kube::Client,
        namespace: &str,
        name: &str,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        let log_params = kube::api::LogParams {
            tail_lines: Some(100),
            ..Default::default()
        };

        let logs = pods
            .logs(name, &log_params)
            .await
            .with_context(|| format!("Failed to get logs for pod {}", name))?;

        Ok(serde_json::json!({
            "pod": name,
            "namespace": namespace,
            "logs": logs,
        }))
    }

    /// Create a pod
    async fn create_pod(
        client: &kube::Client,
        namespace: &str,
        pod_spec: serde_json::Value,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        // Deserialize pod spec
        let pod: Pod =
            serde_json::from_value(pod_spec).context("Failed to parse pod specification")?;

        let created_pod = pods
            .create(&kube::api::PostParams::default(), &pod)
            .await
            .context("Failed to create pod")?;

        let pod_name = created_pod
            .metadata
            .name
            .unwrap_or_else(|| "unknown".to_string());

        Ok(serde_json::json!({
            "created": true,
            "name": pod_name,
            "namespace": namespace,
        }))
    }

    /// Delete a pod
    async fn delete_pod(
        client: &kube::Client,
        namespace: &str,
        name: &str,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Pod;
        use kube::Api;

        let pods: Api<Pod> = Api::namespaced(client.clone(), namespace);

        pods.delete(name, &kube::api::DeleteParams::default())
            .await
            .with_context(|| format!("Failed to delete pod {}", name))?;

        Ok(serde_json::json!({
            "deleted": true,
            "name": name,
            "namespace": namespace,
        }))
    }

    /// List deployments in a namespace
    async fn list_deployments(
        client: &kube::Client,
        namespace: &str,
        label_selector: Option<&str>,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::apps::v1::Deployment;
        use kube::Api;

        let deployments: Api<Deployment> = Api::namespaced(client.clone(), namespace);

        let mut list_params = kube::api::ListParams::default();
        if let Some(selector) = label_selector {
            list_params = list_params.labels(selector);
        }

        let deployment_list = deployments
            .list(&list_params)
            .await
            .context("Failed to list deployments")?;

        let deployment_names: Vec<String> = deployment_list
            .items
            .iter()
            .filter_map(|dep| dep.metadata.name.clone())
            .collect();

        Ok(serde_json::json!({
            "count": deployment_list.items.len(),
            "deployments": deployment_names,
        }))
    }

    /// List services in a namespace
    async fn list_services(
        client: &kube::Client,
        namespace: &str,
        label_selector: Option<&str>,
    ) -> Result<serde_json::Value> {
        use k8s_openapi::api::core::v1::Service;
        use kube::Api;

        let services: Api<Service> = Api::namespaced(client.clone(), namespace);

        let mut list_params = kube::api::ListParams::default();
        if let Some(selector) = label_selector {
            list_params = list_params.labels(selector);
        }

        let service_list = services
            .list(&list_params)
            .await
            .context("Failed to list services")?;

        let service_names: Vec<String> = service_list
            .items
            .iter()
            .filter_map(|svc| svc.metadata.name.clone())
            .collect();

        Ok(serde_json::json!({
            "count": service_list.items.len(),
            "services": service_names,
        }))
    }
}
