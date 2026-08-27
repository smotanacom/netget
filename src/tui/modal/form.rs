//! Generic field form behind `[+ add]` and `e` (edit).
//!
//! Fields are derived from `ServerForm` / `ClientForm` plus the protocol's own
//! declared startup parameters (`management::server_declared_params` /
//! `client_declared_params`). `ParameterDefinition` has no default, so
//! `example` is rendered as a dim placeholder — the only prefill signal there
//! is.
//!
//! Applying routes through the same `management` APIs the LLM and MCP use, so
//! validation, the hot-apply/restart split, and validation-before-mutation all
//! behave identically to every other path.

use anyhow::Result;
use tokio::sync::mpsc;

use crate::cli::management::{self, ClientForm, ServerForm};
use crate::llm::actions::ParameterDefinition;
use crate::state::app_state::AppState;
use crate::tui::app::{Section, UiKey};

/// What a field edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldTarget {
    Port,
    Host,
    Interface,
    MacAddress,
    RemoteAddr,
    SendFirst,
    Instruction,
    InitialMemory,
    FeedbackInstructions,
    /// A protocol-declared startup parameter, by name.
    StartupParam(String),
    /// Raw JSON of the whole event-handler list (the routing editor is the
    /// friendlier path; this is the escape hatch).
    EventHandlersJson,
    /// The routing draft's script code (text-editor return path).
    DraftCode,
    /// The routing draft's LLM instruction (text-editor return path).
    DraftInstruction,
    /// The routing draft's static action list, as a JSON array.
    DraftActions,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub target: FieldTarget,
    pub label: String,
    pub value: String,
    /// The value this field was opened with. `update_server`/`update_client`
    /// take a **partial** form — supplying an unchanged binding field would
    /// read as a change and force a needless restart (new id, dropped
    /// connections), so unchanged fields are omitted on apply.
    pub original: String,
    /// Shown dimmed when the value is empty.
    pub placeholder: String,
    pub help: String,
    pub required: bool,
    /// Multi-line fields open the text editor rather than editing inline.
    pub multiline: bool,
}

impl Field {
    fn simple(target: FieldTarget, label: &str, help: &str) -> Self {
        Self {
            target,
            label: label.to_string(),
            value: String::new(),
            original: String::new(),
            placeholder: String::new(),
            help: help.to_string(),
            required: false,
            multiline: false,
        }
    }

    pub fn changed(&self) -> bool {
        self.value.trim() != self.original.trim()
    }
}

/// Whether the form creates a new instance or edits an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormMode {
    Create(Section),
    Edit(UiKey),
}

/// The editable form state held by the modal.
#[derive(Debug, Clone)]
pub struct FormModel {
    pub mode: FormMode,
    pub protocol: String,
    pub fields: Vec<Field>,
    pub selected: usize,
    /// Inline edit buffer for the selected single-line field; `None` when not
    /// editing.
    pub editing: Option<String>,
    pub error: Option<String>,
    /// An apply is in flight. The work is spawned (it does network I/O), so
    /// the form stays on screen, refuses a second submit, and says so.
    pub busy: bool,
    /// `Some(i)` when focus is on the i-th button rather than a field.
    pub focused_button: Option<usize>,
}

impl FormModel {
    /// Build a create-form for a freshly picked protocol.
    pub fn for_create(section: Section, protocol: &str, default_port: Option<u16>) -> Self {
        let mut fields = Vec::new();
        match section {
            Section::Servers => {
                let mut port = Field::simple(
                    FieldTarget::Port,
                    "port",
                    "TCP/UDP port to bind. 0 asks the OS for a free one.",
                );
                // A protocol with no declared default still has a good one:
                // port 0 asks the OS for a free port. That keeps "pick a
                // protocol and go" working for TCP and friends, and the port
                // remains editable afterwards like everything else.
                port.value = default_port
                    .map(|p| p.to_string())
                    .unwrap_or_else(|| "0".to_string());
                port.placeholder = if default_port.is_none() {
                    "0 = let the OS choose a free port".to_string()
                } else {
                    String::new()
                };
                fields.push(port);
                fields.push(Field::simple(
                    FieldTarget::Host,
                    "host",
                    "Address to bind. Defaults to the protocol's own default.",
                ));
                fields.push(Field::simple(
                    FieldTarget::Interface,
                    "interface",
                    "Network interface, for layer-2/raw protocols (arp, datalink…).",
                ));
                fields.push(Field::simple(
                    FieldTarget::MacAddress,
                    "mac_address",
                    "Source MAC, only meaningful together with an interface.",
                ));
                let mut send_first = Field::simple(
                    FieldTarget::SendFirst,
                    "send_first",
                    "Speak first on connect. NOTE: currently ignored on every path.",
                );
                send_first.value = "false".to_string();
                fields.push(send_first);
            }
            Section::Clients => {
                let mut remote = Field::simple(
                    FieldTarget::RemoteAddr,
                    "remote_addr",
                    "Server to connect to, host:port.",
                );
                remote.required = true;
                remote.placeholder = "required — e.g. 127.0.0.1:2323".to_string();
                fields.push(remote);
            }
        }
        let mut model = Self::finish(
            FormMode::Create(section),
            protocol,
            fields,
            declared_params(section, protocol),
        );
        // An instance created interactively defaults to MANUAL routing: every
        // event parks at the dashboard for YOU to answer. That is the whole
        // point of creating one by hand — you are here, driving. The rules are
        // ordinary handlers, so the routing editor can retarget them to
        // static/script/LLM or delete them outright; instances the model
        // creates through its own tools get no such default and keep LLM
        // behavior.
        //
        // Clients get one extra rule ahead of the wildcard: their connect
        // event(s) are answered with a zero-action static handler ("answer
        // with nothing"). Without it, `<proto>_connected` parked like any
        // other event — and several clients handle that event inline in
        // `connect()` before returning, so creating a client through the
        // dashboard stalled on a question ("you connected — say anything?")
        // that nobody needed answered before the loop could even start.
        // Matching is first-match-wins on exact event ids (`EventPattern`
        // knows only literal ids plus the `*`/`all` wildcard — there is no
        // globbing), so the ids are enumerated from the protocol's own event
        // declarations rather than written as a `*_connected` pattern.
        model.set_field_value(
            &FieldTarget::EventHandlersJson,
            default_event_handlers(section, protocol).to_string(),
        );
        model
    }

    /// Build an edit-form pre-filled from a live instance.
    pub fn for_edit_server(row: &crate::tui::projection::ServerRow) -> Self {
        let mut fields = Vec::new();
        let mut port = Field::simple(
            FieldTarget::Port,
            "port",
            "Changing this RESTARTS the server: it gets a new id and drops connections.",
        );
        // The port it is actually bound to, not the one that was requested. They differ
        // exactly when the request was `port: 0`, and this field sat next to a `host` field
        // that already read `local_addr` — so the form showed the real host beside a literal
        // `0`. An edit form is a snapshot the operator changes in place: showing `0` invites
        // submitting `0`, which is a request for *a different* random port, and this field's
        // own description says changing it drops every connection.
        port.value = row
            .local_addr
            .as_ref()
            .and_then(|a| a.rsplit_once(':'))
            .and_then(|(_, p)| p.parse::<u16>().ok())
            .filter(|p| *p != 0)
            .unwrap_or(row.port)
            .to_string();
        // `original` matches, so an untouched field is still omitted from the update and
        // still does not restart the server.
        port.original = port.value.clone();
        fields.push(port);
        let mut host = Field::simple(
            FieldTarget::Host,
            "host",
            "Changing this RESTARTS the server.",
        );
        host.value = row
            .local_addr
            .as_ref()
            .and_then(|a| a.rsplit_once(':').map(|(h, _)| h.to_string()))
            .unwrap_or_default();
        host.original = host.value.clone();
        fields.push(host);
        Self::finish_edit(
            UiKey::Server(row.id),
            &row.protocol,
            fields,
            declared_params(Section::Servers, &row.protocol),
            &row.startup_params,
            &row.instruction,
            row.routing.as_ref(),
        )
    }

    pub fn for_edit_client(row: &crate::tui::projection::ClientRow) -> Self {
        let mut fields = Vec::new();
        let mut remote = Field::simple(
            FieldTarget::RemoteAddr,
            "remote_addr",
            "Changing this RECONNECTS the client: it gets a new id.",
        );
        remote.value = row.remote_addr.clone();
        remote.original = remote.value.clone();
        fields.push(remote);
        Self::finish_edit(
            UiKey::Client(row.id),
            &row.protocol,
            fields,
            declared_params(Section::Clients, &row.protocol),
            &row.startup_params,
            &row.instruction,
            row.routing.as_ref(),
        )
    }

    fn finish(
        mode: FormMode,
        protocol: &str,
        mut fields: Vec<Field>,
        params: Vec<ParameterDefinition>,
    ) -> Self {
        push_param_fields(&mut fields, &params, None);
        push_text_fields(&mut fields, "", None);
        Self {
            mode,
            protocol: protocol.to_string(),
            fields,
            selected: 0,
            editing: None,
            error: None,
            busy: false,
            focused_button: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_edit(
        key: UiKey,
        protocol: &str,
        mut fields: Vec<Field>,
        params: Vec<ParameterDefinition>,
        startup_params: &Option<serde_json::Value>,
        instruction: &str,
        routing: Option<&crate::scripting::EventHandlerConfig>,
    ) -> Self {
        push_param_fields(&mut fields, &params, startup_params.as_ref());
        push_text_fields(&mut fields, instruction, routing);
        Self {
            mode: FormMode::Edit(key),
            protocol: protocol.to_string(),
            fields,
            selected: 0,
            editing: None,
            error: None,
            busy: false,
            focused_button: None,
        }
    }

    /// Buttons, in Tab order after the fields.
    pub fn buttons(&self) -> Vec<crate::tui::hit::ModalAction> {
        use crate::tui::hit::ModalAction::*;
        vec![FormApply, FormWireshark, FormCancel]
    }

    /// What Wireshark would need to watch the instance this form describes,
    /// read from the fields **as they currently are** (an in-progress edit
    /// included) — not from the changed-only view `apply` uses, because the
    /// capture needs the whole address whether or not it changed.
    pub fn capture_target(&self) -> crate::tui::wireshark::CaptureTarget {
        use crate::tui::wireshark::{CaptureTarget, Role};
        let current = |target: &FieldTarget| -> Option<String> {
            let field = self.fields.iter().find(|f| &f.target == target)?;
            let value = match (&self.editing, self.selected == self.position_of(target)) {
                (Some(buffer), true) => buffer.trim().to_string(),
                _ => field.value.trim().to_string(),
            };
            (!value.is_empty()).then_some(value)
        };
        let section = match &self.mode {
            FormMode::Create(section) => *section,
            FormMode::Edit(UiKey::Server(_)) => Section::Servers,
            FormMode::Edit(UiKey::Client(_)) => Section::Clients,
        };
        match section {
            Section::Servers => CaptureTarget {
                protocol: self.protocol.clone(),
                role: Role::Server,
                host: current(&FieldTarget::Host),
                port: current(&FieldTarget::Port).and_then(|p| p.parse().ok()),
                interface: current(&FieldTarget::Interface),
            },
            Section::Clients => {
                CaptureTarget::client(&self.protocol, current(&FieldTarget::RemoteAddr).as_deref())
            }
        }
    }

    fn position_of(&self, target: &FieldTarget) -> usize {
        self.fields
            .iter()
            .position(|f| &f.target == target)
            .unwrap_or(usize::MAX)
    }

    pub fn focused_action(&self) -> Option<crate::tui::hit::ModalAction> {
        self.focused_button
            .and_then(|index| self.buttons().get(index).copied())
    }

    /// Tab through fields, then buttons, then back to the first field.
    pub fn cycle_focus(&mut self, backward: bool) {
        let fields = self.fields.len();
        let buttons = self.buttons().len();
        let total = fields + buttons;
        if total == 0 {
            return;
        }
        let current = match self.focused_button {
            None => self.selected.min(fields.saturating_sub(1)),
            Some(index) => fields + index,
        };
        let next = if backward {
            (current + total - 1) % total
        } else {
            (current + 1) % total
        };
        if next < fields {
            self.selected = next;
            self.focused_button = None;
        } else {
            self.focused_button = Some(next - fields);
        }
    }

    pub fn selected_field(&self) -> Option<&Field> {
        self.fields.get(self.selected)
    }

    /// The first required field with no value, if any. When this is `None` the
    /// instance can be started without asking the user anything.
    pub fn missing_required(&self) -> Option<String> {
        self.fields
            .iter()
            .find(|f| f.required && f.value.trim().is_empty())
            .map(|f| f.label.clone())
    }

    /// Put the cursor on the first required field with no value, so a form
    /// opened because something is missing starts where the user must type.
    pub fn focus_first_missing_required(&mut self) {
        if let Some(i) = self
            .fields
            .iter()
            .position(|f| f.required && f.value.trim().is_empty())
        {
            self.selected = i;
            self.focused_button = None;
        }
    }

    pub fn move_selection(&mut self, delta: isize) {
        if self.fields.is_empty() {
            return;
        }
        let len = self.fields.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len);
        self.selected = next as usize;
    }

    /// Begin editing the selected field inline (single-line fields only).
    pub fn begin_edit(&mut self) {
        if let Some(field) = self.fields.get(self.selected) {
            if !field.multiline {
                self.editing = Some(field.value.clone());
            }
        }
    }

    pub fn commit_edit(&mut self) {
        if let Some(buffer) = self.editing.take() {
            if let Some(field) = self.fields.get_mut(self.selected) {
                field.value = buffer;
            }
        }
    }

    pub fn cancel_edit(&mut self) {
        self.editing = None;
    }

    pub fn set_field_value(&mut self, target: &FieldTarget, value: String) {
        if let Some(field) = self.fields.iter_mut().find(|f| &f.target == target) {
            field.value = value;
        }
    }

    /// The value to submit for a field: its text when non-empty, and — when
    /// editing — only if the user actually changed it. On create, every
    /// non-empty field is submitted.
    fn value_of(&self, target: &FieldTarget) -> Option<String> {
        let field = self.fields.iter().find(|f| &f.target == target)?;
        let editing = matches!(self.mode, FormMode::Edit(_));
        if editing && !field.changed() {
            return None;
        }
        let value = field.value.trim().to_string();
        (!value.is_empty()).then_some(value)
    }

    /// Assemble the startup-params object from the per-parameter fields.
    ///
    /// On edit, an untouched parameter is omitted: `update_server` merges the
    /// supplied params over the stored ones, and re-sending them all would
    /// trigger a restart for no reason.
    fn startup_params(&self) -> Option<serde_json::Value> {
        let editing = matches!(self.mode, FormMode::Edit(_));
        let mut map = serde_json::Map::new();
        for field in &self.fields {
            if let FieldTarget::StartupParam(name) = &field.target {
                if editing && !field.changed() {
                    continue;
                }
                let raw = field.value.trim();
                if raw.is_empty() {
                    continue;
                }
                // Preserve JSON types where the text parses as JSON (numbers,
                // booleans, objects); otherwise treat it as a string.
                let value = serde_json::from_str::<serde_json::Value>(raw)
                    .unwrap_or_else(|_| serde_json::Value::String(raw.to_string()));
                map.insert(name.clone(), value);
            }
        }
        (!map.is_empty()).then(|| serde_json::Value::Object(map))
    }

    fn event_handlers(&self) -> Result<Option<Vec<serde_json::Value>>> {
        let Some(raw) = self.value_of(&FieldTarget::EventHandlersJson) else {
            return Ok(None);
        };
        let parsed: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("event_handlers is not valid JSON: {e}"))?;
        match parsed {
            serde_json::Value::Array(items) => Ok(Some(items)),
            _ => anyhow::bail!("event_handlers must be a JSON array of handler objects"),
        }
    }

    pub fn to_server_form(&self) -> Result<ServerForm> {
        let port = match self.value_of(&FieldTarget::Port) {
            Some(text) => Some(
                text.parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("port must be a number 0-65535"))?,
            ),
            None => None,
        };
        Ok(ServerForm {
            protocol: self.protocol.clone(),
            port,
            host: self.value_of(&FieldTarget::Host),
            interface: self.value_of(&FieldTarget::Interface),
            mac_address: self.value_of(&FieldTarget::MacAddress),
            send_first: self
                .value_of(&FieldTarget::SendFirst)
                .map(|v| v == "true")
                .unwrap_or(false),
            instruction: self.value_of(&FieldTarget::Instruction),
            initial_memory: self.value_of(&FieldTarget::InitialMemory),
            startup_params: self.startup_params(),
            event_handlers: self.event_handlers()?,
            scheduled_tasks: None,
            feedback_instructions: self.value_of(&FieldTarget::FeedbackInstructions),
        })
    }

    pub fn to_client_form(&self) -> Result<ClientForm> {
        Ok(ClientForm {
            protocol: self.protocol.clone(),
            remote_addr: self.value_of(&FieldTarget::RemoteAddr),
            instruction: self.value_of(&FieldTarget::Instruction),
            initial_memory: self.value_of(&FieldTarget::InitialMemory),
            startup_params: self.startup_params(),
            event_handlers: self.event_handlers()?,
            scheduled_tasks: None,
            feedback_instructions: self.value_of(&FieldTarget::FeedbackInstructions),
        })
    }

    /// Apply the form: create or update, returning a line for the chat log.
    pub async fn apply(
        &self,
        state: &AppState,
        llm_client: crate::llm::OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<String> {
        match &self.mode {
            FormMode::Create(Section::Servers) => {
                let form = self.to_server_form()?;
                let id = form.create(state, status_tx.clone()).await?;
                Ok(format!(
                    "Started server #{} ({})",
                    id.as_u32(),
                    self.protocol
                ))
            }
            FormMode::Create(Section::Clients) => {
                let form = self.to_client_form()?;
                let id = form.create(state, llm_client, status_tx.clone()).await?;
                Ok(format!(
                    "Connected client #{} ({})",
                    id.as_u32(),
                    self.protocol
                ))
            }
            FormMode::Edit(UiKey::Server(server_id)) => {
                let form = self.to_server_form()?;
                let outcome =
                    management::update_server(state, *server_id, form, status_tx.clone()).await?;
                Ok(outcome.summary)
            }
            FormMode::Edit(UiKey::Client(client_id)) => {
                let form = self.to_client_form()?;
                let outcome = management::update_client(
                    state,
                    *client_id,
                    form,
                    llm_client,
                    status_tx.clone(),
                )
                .await?;
                Ok(outcome.summary)
            }
        }
    }
}

/// The default routing an interactively created instance starts with.
///
/// Servers: one `*` → manual rule — every event waits for the human driving
/// the dashboard.
///
/// Clients: the same `*` → manual rule, preceded by one zero-action static
/// rule per connect event (`<proto>_connected`), so establishing the
/// connection itself never parks. The connect event is bookkeeping — the
/// interesting questions (data arrived, what do we send?) still park for the
/// human. `EventHandlerConfig::find_handler` is first-match-wins over exact
/// event ids ('*'/'all' are the only wildcards), which is why the ids come
/// from the client protocol's own `get_event_types()` instead of a
/// `*_connected` glob that nothing would ever match. A client whose connect
/// event does not follow the `_connected` naming keeps the plain manual
/// default.
fn default_event_handlers(section: Section, protocol: &str) -> serde_json::Value {
    let manual_fallback = serde_json::json!({
        "event_pattern": "*",
        "handler": {"type": "manual"}
    });
    let mut rules = Vec::new();
    if section == Section::Clients {
        // `resolve` rather than `get`: case-insensitive, same lookup the
        // management paths use for a protocol the user named.
        if let Ok(client) = crate::protocol::CLIENT_REGISTRY.resolve(protocol) {
            for event_type in client.get_event_types() {
                if event_type.id.ends_with("_connected") {
                    rules.push(serde_json::json!({
                        "event_pattern": event_type.id,
                        // Zero actions: acknowledge the connect, say nothing —
                        // the same answer as the intercept modal's
                        // [ Answer with nothing ], made ahead of time.
                        "handler": {"type": "static", "actions": []}
                    }));
                }
            }
        }
    }
    rules.push(manual_fallback);
    serde_json::Value::Array(rules)
}

fn declared_params(section: Section, protocol: &str) -> Vec<ParameterDefinition> {
    match section {
        Section::Servers => management::server_declared_params(protocol),
        Section::Clients => management::client_declared_params(protocol),
    }
    .unwrap_or_default()
}

fn push_param_fields(
    fields: &mut Vec<Field>,
    params: &[ParameterDefinition],
    current: Option<&serde_json::Value>,
) {
    for param in params {
        // Several protocols declare a startup parameter that the form already
        // has a first-class field for (`send_first`, `port`, `host`). Showing
        // both would offer two controls for one value.
        if fields.iter().any(|f| f.label == param.name) {
            continue;
        }
        let existing = current
            .and_then(|v| v.get(&param.name))
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        let example = match &param.example {
            serde_json::Value::Null => String::new(),
            serde_json::Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        fields.push(Field {
            target: FieldTarget::StartupParam(param.name.clone()),
            label: param.name.clone(),
            value: existing.clone(),
            original: existing,
            placeholder: if example.is_empty() {
                String::new()
            } else {
                format!("e.g. {example}")
            },
            help: format!("{} ({})", param.description, param.type_hint),
            required: param.required,
            multiline: false,
        });
    }
}

fn push_text_fields(
    fields: &mut Vec<Field>,
    instruction: &str,
    routing: Option<&crate::scripting::EventHandlerConfig>,
) {
    let mut instruction_field = Field::simple(
        FieldTarget::Instruction,
        "instruction",
        "Natural-language instruction for the LLM fallback path.",
    );
    instruction_field.value = instruction.to_string();
    instruction_field.original = instruction_field.value.clone();
    instruction_field.multiline = true;
    fields.push(instruction_field);

    let mut memory = Field::simple(
        FieldTarget::InitialMemory,
        "initial_memory",
        "Seed the instance's LLM memory.",
    );
    memory.multiline = true;
    fields.push(memory);

    let mut feedback = Field::simple(
        FieldTarget::FeedbackInstructions,
        "feedback_instructions",
        "Instructions for the automatic feedback loop.",
    );
    feedback.multiline = true;
    fields.push(feedback);

    let mut handlers = Field::simple(
        FieldTarget::EventHandlersJson,
        "event_handlers",
        "Routing as raw JSON. Press r on the band for the guided editor.",
    );
    handlers.value = routing
        .map(|r| serde_json::to_string_pretty(&r.handlers).unwrap_or_default())
        .unwrap_or_default();
    handlers.original = handlers.value.clone();
    handlers.multiline = true;
    fields.push(handlers);
}
