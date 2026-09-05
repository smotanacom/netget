//! Event handler configuration system
//!
//! This module defines how events are handled - either by LLM, script, or static responses.
//!
//! # Static handler event interpolation
//!
//! A [`EventHandlerType::Static`] handler emits its configured actions without calling
//! the LLM. To let those actions echo values from the event that triggered them —
//! correlation ids such as a DNS `query_id`, a DHCP/BOOTP `xid`, an SNMP `request-id`,
//! a STUN transaction id — action JSON may contain references of the form:
//!
//! ```text
//! {{event.<field>}}          e.g. {{event.query_id}}
//! {{event.<a>.<b>}}          e.g. {{event.headers.host}}
//! {{event.<list>.<index>}}   e.g. {{event.questions.0.name}}
//! {{event}}                  the whole event payload
//! ```
//!
//! Substitution is performed by [`interpolate_actions`] immediately before the actions
//! are executed. Three rules define it:
//!
//! 1. **Whole-string reference preserves type.** A JSON string whose *entire* value is
//!    one reference is replaced by the referenced JSON value itself, so
//!    `"query_id": "{{event.query_id}}"` yields a *number*, not `"4660"`. Objects,
//!    arrays, booleans and null survive equally.
//! 2. **Embedded reference interpolates text.** `"reply to {{event.domain}}"` produces a
//!    string; non-string values are rendered in their JSON form (`null`, `true`, `42`,
//!    `{"a":1}`).
//! 3. **Everything else is byte-identical.** Only `{{` … `}}` groups whose contents are
//!    `event` or begin with `event.` are touched. Any other braces — Handlebars snippets
//!    in a served template, `{{ msg }}` in a Vue page, `{` in a JSON body or a regex —
//!    pass through unchanged, so handlers written before this feature keep working.
//!
//! An unresolvable reference is a hard error naming the reference and listing the fields
//! the event actually carries; it is never silently rendered as `null` or the empty
//! string, because a static handler with a typo'd field name must not appear to work.
//!
//! ## Why not Handlebars
//!
//! Handlebars is already a dependency (`src/llm/template_engine.rs`) and lends the
//! familiar `{{…}}` spelling, but it is the wrong engine for this path: it renders to a
//! `String`, so rule 1 would require re-parsing the output and would turn the string
//! `"007"` into the number `7`; it HTML-escapes `{{…}}` by default, corrupting JSON and
//! URL payloads unless every reference uses the triple-stash; and it claims the whole
//! `{{…}}` namespace, so a handler that serves a Handlebars/Vue template, or that
//! contains `{{#if}}`/`{{!--`/`{{>` text, would be rewritten or rejected. The resolver
//! below is ~150 lines, borrows only the spelling, and leaves every non-`event`
//! reference alone.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pattern for matching events
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPattern {
    /// Match a specific event type ID
    Specific(String),
    /// Match all events
    Wildcard,
}

impl EventPattern {
    /// Check if this pattern matches the given event type ID
    pub fn matches(&self, event_type_id: &str) -> bool {
        match self {
            EventPattern::Specific(pattern) => pattern == event_type_id,
            EventPattern::Wildcard => true,
        }
    }

    /// Create a wildcard pattern
    pub fn wildcard() -> Self {
        EventPattern::Wildcard
    }

    /// Create a specific pattern
    pub fn specific(event_type_id: impl Into<String>) -> Self {
        EventPattern::Specific(event_type_id.into())
    }
}

impl From<String> for EventPattern {
    fn from(s: String) -> Self {
        if s == "*" || s == "all" {
            EventPattern::Wildcard
        } else {
            EventPattern::Specific(s)
        }
    }
}

impl From<&str> for EventPattern {
    fn from(s: &str) -> Self {
        if s == "*" || s == "all" {
            EventPattern::Wildcard
        } else {
            EventPattern::Specific(s.to_string())
        }
    }
}

/// Handler type for an event
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventHandlerType {
    /// Handle with LLM (default behavior)
    Llm {
        /// Instruction for how the LLM should handle this event
        instruction: String,
    },

    /// Handle with inline script
    Script {
        /// Scripting language (python, javascript, go, perl)
        language: String,
        /// Inline script code
        code: String,
        /// Run the script as a **resident** process: spawned once per scope and
        /// driven with one event per stdin line, keeping in-process state
        /// between events. Opt-in; defaults to `false` (the stateless per-event
        /// path). A resident script defines `handle(event_type, event, message)`
        /// instead of reading stdin itself. See `src/scripting/resident.rs`.
        #[serde(default)]
        resident: bool,
        /// Resident scope: `"server"` (default — one process shared by all the
        /// server's connections) or `"connection"` (one process per
        /// connection). Ignored unless `resident` is `true`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },

    /// Handle with static response (actions array)
    ///
    /// Action JSON may reference the triggering event with `{{event.field}}`; see the
    /// [module docs](self#static-handler-event-interpolation) for the substitution rules.
    Static {
        /// Actions to execute (actual JSON values, not strings)
        actions: Vec<serde_json::Value>,
    },

    /// Handled by a human at the dashboard.
    ///
    /// The event parks as a pending question (`AppState::park_intercept`) and the
    /// connection waits, exactly as it would for a slow model. Whatever actions the
    /// operator composes are executed as the answer; `{{event.field}}` references work
    /// the same as in a static handler. If nobody answers within `timeout_secs`, the
    /// handler **fails closed** — the dispatch errors and the protocol's LLM-failure
    /// branch answers the peer with a category, never with an invented success.
    Manual {
        /// Seconds to wait for the operator before failing closed.
        #[serde(default = "default_manual_timeout_secs")]
        timeout_secs: u64,
    },
}

/// How long a manual handler waits for the operator by default.
///
/// Generous on purpose: the whole point is that a human reads the event and composes an
/// answer, and most protocol peers apply their own (shorter) timeout anyway. It exists so
/// an unattended dashboard eventually fails closed instead of parking connections forever.
pub const DEFAULT_MANUAL_TIMEOUT_SECS: u64 = 300;

fn default_manual_timeout_secs() -> u64 {
    DEFAULT_MANUAL_TIMEOUT_SECS
}

impl EventHandlerType {
    /// Create a per-event (stateless) script handler.
    pub fn script(language: impl Into<String>, code: impl Into<String>) -> Self {
        EventHandlerType::Script {
            language: language.into(),
            code: code.into(),
            resident: false,
            scope: None,
        }
    }

    /// Create a resident (persistent) script handler with the given scope
    /// (`"server"` or `"connection"`; `None` defaults to server scope).
    pub fn script_resident(
        language: impl Into<String>,
        code: impl Into<String>,
        scope: Option<String>,
    ) -> Self {
        EventHandlerType::Script {
            language: language.into(),
            code: code.into(),
            resident: true,
            scope,
        }
    }

    /// Create a static handler
    pub fn static_response(actions: Vec<serde_json::Value>) -> Self {
        EventHandlerType::Static { actions }
    }

    /// Create an LLM handler
    pub fn llm(instruction: impl Into<String>) -> Self {
        EventHandlerType::Llm {
            instruction: instruction.into(),
        }
    }

    /// Create a manual (human-answered) handler.
    pub fn manual(timeout_secs: u64) -> Self {
        EventHandlerType::Manual { timeout_secs }
    }

    /// Validate the handler's `{{event.…}}` references *without* an event.
    ///
    /// This is the parse-time half of the check: it catches malformed references
    /// (`{{event.}}`, `{{event..x}}`, an opening `{{event.` that is never closed), which
    /// are wrong regardless of which event arrives. Whether a *well-formed* reference
    /// resolves depends on the event payload and can only be decided at dispatch time by
    /// [`interpolate_actions`].
    ///
    /// Callers that parse handler configuration (e.g. `EventHandler::parse_event_handlers`
    /// in `src/events/handler.rs`) should call this so a typo is reported to the MCP
    /// caller at `start_server` time rather than silently at the first packet.
    pub fn validate(&self) -> Result<(), InterpolationError> {
        match self {
            EventHandlerType::Static { actions } => {
                for action in actions {
                    validate_event_references(action)?;
                }
                Ok(())
            }
            // A script's contract depends on `resident`, and getting it wrong fails
            // silently: a resident script defines `handle(event_type, event, message)`
            // while a non-resident one reads stdin and prints its own output. Define
            // `handle` without `resident: true` and the script produces nothing, the
            // handler yields no actions, and the server fails closed -- which the peer
            // sees as a generic protocol error with no hint of the real cause.
            EventHandlerType::Script {
                language,
                code,
                resident,
                ..
            } if !*resident && defines_handle(language, code) => {
                Err(InterpolationError::script_contract(language.clone()))
            }
            _ => Ok(()),
        }
    }
}

/// Event handler configuration - maps event patterns to handlers
///
/// `deny_unknown_fields` is load-bearing. There are exactly two keys here, and there is no
/// data-based matching: a rule matches on the event id alone. An `event_data_contains` key
/// -- an entirely reasonable thing to assume exists, and something an LLM writing handlers
/// will invent -- used to be accepted and silently ignored, so every rule registered
/// against one event id matched every occurrence and first-match-wins picked the first.
/// The config looked like it discriminated between requests and simply did not.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventHandler {
    /// Pattern to match events
    pub event_pattern: EventPattern,

    /// Handler to use for matched events
    pub handler: EventHandlerType,
}

impl EventHandler {
    /// Create a new event handler
    pub fn new(event_pattern: EventPattern, handler: EventHandlerType) -> Self {
        Self {
            event_pattern,
            handler,
        }
    }

    /// Check if this handler matches the given event type ID
    pub fn matches(&self, event_type_id: &str) -> bool {
        self.event_pattern.matches(event_type_id)
    }
}

/// Configuration for all event handlers
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventHandlerConfig {
    /// List of event handlers (processed in order, first match wins)
    pub handlers: Vec<EventHandler>,
}

impl EventHandlerConfig {
    /// Create a new empty configuration
    pub fn new() -> Self {
        Self {
            handlers: Vec::new(),
        }
    }

    /// Add a handler to the configuration
    pub fn add_handler(&mut self, handler: EventHandler) {
        self.handlers.push(handler);
    }

    /// Find the first handler that matches the given event type ID
    pub fn find_handler(&self, event_type_id: &str) -> Option<&EventHandlerType> {
        self.handlers
            .iter()
            .find(|h| h.matches(event_type_id))
            .map(|h| &h.handler)
    }

    /// Check if any handlers are configured
    pub fn is_empty(&self) -> bool {
        self.handlers.is_empty()
    }

    /// Get the number of handlers
    pub fn len(&self) -> usize {
        self.handlers.len()
    }
}

// ---------------------------------------------------------------------------
// Static handler event interpolation
// ---------------------------------------------------------------------------

/// Opening delimiter of a reference.
const REF_OPEN: &str = "{{";
/// Closing delimiter of a reference.
const REF_CLOSE: &str = "}}";
/// The only root identifier that is substituted. `{{anything.else}}` is left alone.
const REF_ROOT: &str = "event";
/// `REF_ROOT` followed by the path separator.
const REF_ROOT_DOT: &str = "event.";

/// A `{{event.…}}` reference in a static handler action could not be resolved.
///
/// Carries the offending reference verbatim plus a human-readable reason that names the
/// missing field and lists what the event actually offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolationError {
    /// The reference exactly as it appeared, e.g. `{{event.headers.hsot}}`, or the
    /// script's language when `kind` is [`ErrorKind::ScriptContract`].
    pub reference: String,
    /// Why it could not be resolved, including the available alternatives
    pub detail: String,
    /// Which validation failed, so the message reads correctly for each.
    pub kind: ErrorKind,
}

/// What `EventHandlerType::validate` rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// A `{{event.…}}` reference in a static handler could not be resolved.
    Interpolation,
    /// A script's entry point contradicts its `resident` flag.
    ScriptContract,
}

impl InterpolationError {
    fn new(reference: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            detail: detail.into(),
            kind: ErrorKind::Interpolation,
        }
    }

    /// A script defines `handle(...)` but is not marked `resident` (or vice versa).
    fn script_contract(language: impl Into<String>) -> Self {
        Self {
            reference: language.into(),
            detail: String::new(),
            kind: ErrorKind::ScriptContract,
        }
    }
}

impl std::fmt::Display for InterpolationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.kind {
            ErrorKind::Interpolation => write!(
                f,
                "static handler reference `{}` could not be resolved: {}",
                self.reference, self.detail
            ),
            ErrorKind::ScriptContract => write!(
                f,
                "this {} script defines `handle(...)`, which is the RESIDENT entry point, \
                 but the handler is not marked `resident: true`. As written the script \
                 reads nothing from stdin and prints nothing, so the handler produces no \
                 actions and the server fails closed -- with no hint of why. Add \
                 `\"resident\": true`, or rewrite the script to read the event from stdin \
                 and print its actions.",
                self.reference
            ),
        }
    }
}

impl std::error::Error for InterpolationError {}

/// A located reference inside a string.
struct FoundRef<'a> {
    /// Byte offset of the opening `{{`
    start: usize,
    /// Byte offset just past the closing `}}`
    end: usize,
    /// The reference including delimiters, for error messages
    raw: &'a str,
    /// The trimmed contents between the delimiters (`event` or `event.…`)
    inner: &'a str,
}

/// Find the next `{{event…}}` reference at or after byte offset `from`.
///
/// `{{` groups whose contents are not rooted at `event` are skipped, not consumed, so a
/// Handlebars/Vue template embedded in a handler is left untouched. The scan advances one
/// byte at a time on a miss, so `{{{event.x}}}` still finds the inner reference.
fn find_reference(s: &str, from: usize) -> Option<FoundRef<'_>> {
    let mut cursor = from;
    while cursor < s.len() {
        let open = cursor + s[cursor..].find(REF_OPEN)?;
        let after_open = open + REF_OPEN.len();
        let Some(rel_close) = s[after_open..].find(REF_CLOSE) else {
            // No closing delimiter anywhere after this point: nothing left to find.
            return None;
        };
        let close = after_open + rel_close;
        let inner = s[after_open..close].trim();
        if inner == REF_ROOT || inner.starts_with(REF_ROOT_DOT) {
            return Some(FoundRef {
                start: open,
                end: close + REF_CLOSE.len(),
                raw: &s[open..close + REF_CLOSE.len()],
                inner,
            });
        }
        cursor = open + 1;
    }
    None
}

/// Split a reference's contents into path segments. `{{event}}` yields an empty path.
fn parse_path<'a>(found: &FoundRef<'a>) -> Result<Vec<&'a str>, InterpolationError> {
    if found.inner == REF_ROOT {
        return Ok(Vec::new());
    }
    let rest = &found.inner[REF_ROOT_DOT.len()..];
    if rest.is_empty() {
        return Err(InterpolationError::new(
            found.raw,
            format!(
                "the path after `{}.` is empty; write `{{{{event.field}}}}` or `{{{{event}}}}` for the whole payload",
                REF_ROOT
            ),
        ));
    }
    let mut segments = Vec::new();
    for segment in rest.split('.') {
        let segment = segment.trim();
        if segment.is_empty() {
            return Err(InterpolationError::new(
                found.raw,
                "it contains an empty path segment (a doubled or trailing `.`)",
            ));
        }
        segments.push(segment);
    }
    Ok(segments)
}

/// Describe what can be reached from `value`, for the "available:" half of an error.
fn describe_available(value: &Value) -> String {
    match value {
        Value::Object(map) if map.is_empty() => "this object is empty".to_string(),
        Value::Object(map) => format!(
            "available fields: {}",
            map.keys().cloned().collect::<Vec<_>>().join(", ")
        ),
        Value::Array(items) if items.is_empty() => "this array is empty".to_string(),
        Value::Array(items) => format!("available indices: 0..{}", items.len() - 1),
        other => format!("it is a {} and has no fields", json_type_name(other)),
    }
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Walk `segments` through the event payload.
fn resolve<'a>(
    event: &'a Value,
    segments: &[&str],
    raw: &str,
) -> Result<&'a Value, InterpolationError> {
    let mut current = event;
    let mut walked = REF_ROOT.to_string();
    for segment in segments {
        let next = match current {
            Value::Object(map) => map.get(*segment),
            Value::Array(items) => segment.parse::<usize>().ok().and_then(|i| items.get(i)),
            _ => None,
        };
        current = next.ok_or_else(|| {
            InterpolationError::new(
                raw,
                format!(
                    "`{}` has no `{}` ({})",
                    walked,
                    segment,
                    describe_available(current)
                ),
            )
        })?;
        walked.push('.');
        walked.push_str(segment);
    }
    Ok(current)
}

/// Render a resolved value for embedding inside a larger string.
///
/// Strings embed as their contents (no quotes); everything else embeds as its JSON form,
/// so a number stays `42`, a boolean `true`, an absent-but-present field `null`.
fn value_to_display(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The event payload was absent, but a reference needed it.
fn missing_event_error(raw: &str) -> InterpolationError {
    InterpolationError::new(
        raw,
        "this event carries no structured data, so nothing can be substituted",
    )
}

/// Interpolate one string, returning a `Value` so a whole-string reference keeps its type.
fn interpolate_string(s: &str, event: Option<&Value>) -> Result<Value, InterpolationError> {
    let Some(first) = find_reference(s, 0) else {
        // No reference at all: byte-identical pass-through.
        return Ok(Value::String(s.to_string()));
    };

    // Rule 1: the string is exactly one reference -> substitute the JSON value itself.
    if first.start == 0 && first.end == s.len() {
        let segments = parse_path(&first)?;
        let event = event.ok_or_else(|| missing_event_error(first.raw))?;
        return Ok(resolve(event, &segments, first.raw)?.clone());
    }

    // Rule 2: one or more references embedded in surrounding text -> string splice.
    let mut out = String::with_capacity(s.len());
    let mut cursor = 0usize;
    let mut found = Some(first);
    while let Some(f) = found {
        out.push_str(&s[cursor..f.start]);
        let segments = parse_path(&f)?;
        let event = event.ok_or_else(|| missing_event_error(f.raw))?;
        out.push_str(&value_to_display(resolve(event, &segments, f.raw)?));
        cursor = f.end;
        found = find_reference(s, cursor);
    }
    out.push_str(&s[cursor..]);
    Ok(Value::String(out))
}

/// Substitute every `{{event.…}}` reference in a JSON value tree.
///
/// Object keys are interpolated too, always as text (a JSON key must be a string).
/// Values with no references are returned unchanged.
pub fn interpolate_value(
    value: &Value,
    event_data: Option<&Value>,
) -> Result<Value, InterpolationError> {
    match value {
        Value::String(s) => interpolate_string(s, event_data),
        Value::Array(items) => items
            .iter()
            .map(|item| interpolate_value(item, event_data))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, val) in map {
                let key = match interpolate_string(key, event_data)? {
                    Value::String(s) => s,
                    other => value_to_display(&other),
                };
                out.insert(key, interpolate_value(val, event_data)?);
            }
            Ok(Value::Object(out))
        }
        // Numbers, booleans and null cannot contain references.
        other => Ok(other.clone()),
    }
}

/// Substitute `{{event.…}}` references across a static handler's action list.
///
/// Returns the actions unchanged (and never errors) when none of them contains a
/// reference, so pre-existing handlers are unaffected — including handlers whose payloads
/// contain literal braces.
///
/// # Errors
/// [`InterpolationError`] if a reference is malformed, names a field the event does not
/// have, or needs event data that this event did not carry.
pub fn interpolate_actions(
    actions: &[Value],
    event_data: Option<&Value>,
) -> Result<Vec<Value>, InterpolationError> {
    if !actions.iter().any(contains_event_reference) {
        return Ok(actions.to_vec());
    }
    actions
        .iter()
        .map(|action| interpolate_value(action, event_data))
        .collect()
}

/// Whether a value tree contains at least one `{{event…}}` reference.
pub fn contains_event_reference(value: &Value) -> bool {
    match value {
        Value::String(s) => find_reference(s, 0).is_some(),
        Value::Array(items) => items.iter().any(contains_event_reference),
        Value::Object(map) => map
            .iter()
            .any(|(k, v)| find_reference(k, 0).is_some() || contains_event_reference(v)),
        _ => false,
    }
}

/// Whether a script defines the resident entry point `handle(...)`.
///
/// Deliberately syntactic and conservative: it looks for a definition, not a call, so a
/// script that merely mentions the word is not flagged.
fn defines_handle(language: &str, code: &str) -> bool {
    let needles: &[&str] = match language.to_ascii_lowercase().as_str() {
        "python" => &["def handle("],
        "javascript" | "js" | "node" => &["function handle(", "handle = function(", "handle = ("],
        "go" => &["func handle("],
        "perl" => &["sub handle "],
        _ => return false,
    };
    let code = code
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    needles.iter().any(|n| code.contains(n))
}

/// Check every reference in a value tree for syntactic validity, without an event.
///
/// Catches malformed paths (`{{event.}}`, `{{event..x}}`) that are wrong for any event.
/// Field existence is deliberately *not* checked here: it depends on the payload.
pub fn validate_event_references(value: &Value) -> Result<(), InterpolationError> {
    match value {
        Value::String(s) => {
            let mut cursor = 0usize;
            while let Some(found) = find_reference(s, cursor) {
                parse_path(&found)?;
                cursor = found.end;
            }
            Ok(())
        }
        Value::Array(items) => items.iter().try_for_each(validate_event_references),
        Value::Object(map) => map.iter().try_for_each(|(k, v)| {
            validate_event_references(&Value::String(k.clone()))?;
            validate_event_references(v)
        }),
        _ => Ok(()),
    }
}
