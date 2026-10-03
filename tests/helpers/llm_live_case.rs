//! Hand-authored event-level live tests, for protocols whose transport cannot
//! run on this machine (Bluetooth, USB, NFC, raw sockets, VPNs).
//!
//! The LLM request/response layer is transport-independent: a protocol's
//! server code turns wire input into an *event*, the model turns the event
//! into an *action*, and the server code turns the action back into wire
//! output. The first and last steps need hardware; the middle one — the one
//! the model owns — does not. An [`EventCase`] exercises exactly that middle
//! step, with **hand-written protocol knowledge** on both sides:
//!
//! - the event payload is authored per protocol (a real GATT read of the
//!   Heart Rate Measurement characteristic, a real USB HID report request…),
//!   not synthesized;
//! - the assertion names the one correct action for that event **and checks
//!   its parameter values** (the right characteristic UUID, a correctly
//!   flagged measurement payload…), not merely "some declared action".
//!
//! The prompt is built with netget's real `PromptBuilder` network-event path
//! — the same system prompt, action vocabulary and trigger message a live
//! connection would produce — so a failure indicts either the protocol's
//! prompting surface or the model, and the report shows the model's actual
//! actions to tell which.
//!
//! Wire-capable protocols do NOT use this: they get real-client wire tests
//! (tcp.rs, dns.rs, …), which subsume this layer.

use super::common::E2EResult;
use super::llm_live::{
    ensure_model_available, live_llm_enabled, live_model, live_test_lock, LLM_REQUEST_TIMEOUT_SECS,
};
use netget::llm::actions::{get_network_event_common_actions, normalize_action_object};
use netget::llm::{prompt::PromptBuilder, ConversationHandler, OllamaClient, RequestSource};
use netget::protocol::server_registry::registry;
use netget::state::app_state::AppState;
use netget::state::app_state::WebSearchMode;
use netget::state::ServerId;
use serde_json::Value;

/// A predicate on one parameter of the expected action.
pub struct ParamCheck {
    pub name: &'static str,
    pub describe: String,
    pub check: Box<dyn Fn(&Value) -> Result<(), String> + Send + Sync>,
}

impl ParamCheck {
    /// Parameter must be present and its string form must contain `needle`
    /// (case-insensitive).
    pub fn contains(name: &'static str, needle: &'static str) -> Self {
        Self {
            name,
            describe: format!("{} contains {:?}", name, needle),
            check: Box::new(move |v| {
                let s = match v.as_str() {
                    Some(s) => s.to_string(),
                    None => v.to_string(),
                };
                if s.to_lowercase().contains(&needle.to_lowercase()) {
                    Ok(())
                } else {
                    Err(format!("value {:?} does not contain {:?}", s, needle))
                }
            }),
        }
    }

    /// Parameter must equal the given JSON value (number/string tolerant:
    /// "42" and 42 are treated as equal).
    pub fn equals(name: &'static str, expected: Value) -> Self {
        let shown = expected.to_string();
        Self {
            name,
            describe: format!("{} == {}", name, shown),
            check: Box::new(move |v| {
                let eq = v == &expected
                    || (v.as_str().map(|s| s.to_string())
                        == Some(expected.to_string().trim_matches('"').to_string()))
                    || (expected.as_str().map(|s| s.to_string())
                        == Some(v.to_string().trim_matches('"').to_string()));
                if eq {
                    Ok(())
                } else {
                    Err(format!("value {} != expected {}", v, shown))
                }
            }),
        }
    }

    /// Parameter must be present and non-empty (string/array/object).
    pub fn non_empty(name: &'static str) -> Self {
        Self {
            name,
            describe: format!("{} non-empty", name),
            check: Box::new(|v| {
                let empty = match v {
                    Value::String(s) => s.is_empty(),
                    Value::Array(a) => a.is_empty(),
                    Value::Object(o) => o.is_empty(),
                    Value::Null => true,
                    _ => false,
                };
                if empty {
                    Err("value is empty".to_string())
                } else {
                    Ok(())
                }
            }),
        }
    }

    /// Free-form predicate.
    pub fn custom(
        name: &'static str,
        describe: impl Into<String>,
        f: impl Fn(&Value) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name,
            describe: describe.into(),
            check: Box::new(f),
        }
    }
}

/// One hand-authored protocol event case.
pub struct EventCase {
    protocol: String,
    instruction: String,
    event_id: String,
    event_data: Value,
    expect_action: String,
    /// Extra action names that are equally correct (a protocol may declare two
    /// actions with identical effect — WireGuard's reject_peer and
    /// disconnect_peer both remove the peer from the interface).
    also_accept: Vec<String>,
    param_checks: Vec<ParamCheck>,
    /// Predicates over the whole chosen action, for invariants that span
    /// several parameters (a result set whose rows must match its columns).
    action_checks: Vec<(
        String,
        Box<dyn Fn(&Value) -> Result<(), String> + Send + Sync>,
    )>,
}

impl EventCase {
    /// `protocol`: registry name (aliases accepted). `instruction`: the
    /// per-server instruction, written for this protocol. `event_id`: must be
    /// an event type the protocol actually declares — the case fails loudly
    /// on drift. `event_data`: hand-written realistic payload.
    pub fn new(
        protocol: impl Into<String>,
        instruction: impl Into<String>,
        event_id: impl Into<String>,
        event_data: Value,
    ) -> Self {
        Self {
            protocol: protocol.into(),
            instruction: instruction.into(),
            event_id: event_id.into(),
            event_data,
            expect_action: String::new(),
            also_accept: Vec::new(),
            param_checks: Vec::new(),
            action_checks: Vec::new(),
        }
    }

    /// The one action type the model must answer this event with.
    pub fn expect_action(mut self, action: impl Into<String>) -> Self {
        self.expect_action = action.into();
        self
    }

    /// Accept this action too, when the protocol declares two names with the
    /// same effect. Parameter checks run against whichever was chosen.
    pub fn or_action(mut self, action: impl Into<String>) -> Self {
        self.also_accept.push(action.into());
        self
    }

    /// Add a predicate on the expected action's parameters.
    pub fn check(mut self, check: ParamCheck) -> Self {
        self.param_checks.push(check);
        self
    }

    /// Add a predicate over the whole chosen action, for invariants no single
    /// parameter expresses on its own.
    pub fn check_action(
        mut self,
        f: impl Fn(&Value) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        self.action_checks
            .push(("action as a whole".to_string(), Box::new(f)));
        self
    }

    /// Run the case against the live model. Skips (Ok) when the live gate is
    /// off or the protocol is not compiled into this build.
    pub async fn run(self) -> E2EResult<()> {
        if !live_llm_enabled() {
            return Ok(());
        }
        let model = live_model();
        ensure_model_available(&model).await?;
        let _guard = live_test_lock().await;

        let (canonical, protocol) =
            match registry().all_protocols().into_iter().find(|(name, p)| {
                name.eq_ignore_ascii_case(&self.protocol)
                    || registry().parse_from_str(&self.protocol).as_deref() == Some(name)
                    || p.protocol_name().eq_ignore_ascii_case(&self.protocol)
            }) {
                Some(p) => p,
                None => {
                    eprintln!(
                        "skipped: protocol '{}' not compiled into this build",
                        self.protocol
                    );
                    return Ok(());
                }
            };

        // The event must exist — drift between test and protocol is a failure,
        // not a skip.
        let event_type = protocol
            .get_event_types()
            .into_iter()
            .find(|e| e.id == self.event_id)
            .ok_or_else(|| {
                format!(
                    "protocol '{}' declares no event '{}'. Declared: {:?}",
                    canonical,
                    self.event_id,
                    protocol
                        .get_event_types()
                        .iter()
                        .map(|e| e.id.clone())
                        .collect::<Vec<_>>()
                )
            })?;

        // Sanity: the expected action must be one the event advertises,
        // otherwise the case tests something the model cannot do.
        let mut advertised = event_type.actions.clone();
        if event_type.has_no_usable_actions() {
            advertised = protocol.get_sync_actions();
        }
        let mut accepted: Vec<String> = vec![self.expect_action.clone()];
        accepted.extend(self.also_accept.iter().cloned());
        // Common actions are offered on every event alongside the protocol's
        // own, and are the only thing an event marked `with_no_actions()` can
        // be answered with (LDAP's unbind, which RFC 4511 forbids replying to).
        let common: Vec<String> = get_network_event_common_actions()
            .into_iter()
            .map(|a| a.name)
            .collect();
        for name in &accepted {
            if common.contains(name) {
                continue;
            }
            if !advertised.iter().any(|a| &a.name == name) {
                return Err(format!(
                    "case bug: event '{}' of '{}' does not advertise action '{}'. Advertised: {:?}",
                    self.event_id,
                    canonical,
                    name,
                    advertised
                        .iter()
                        .map(|a| a.name.clone())
                        .collect::<Vec<_>>()
                )
                .into());
            }
        }

        println!(
            "🤖 event case: {}::{} model={} expecting {}",
            canonical,
            self.event_id,
            model,
            accepted.join(" or ")
        );

        // Real prompt path, synthetic never-spawned server.
        let state = AppState::new();
        let server_id = ServerId::new(1);
        state
            .add_server_with_id(netget::state::server::ServerInstance::new(
                server_id,
                8080,
                canonical.clone(),
                self.instruction.clone(),
            ))
            .await;
        let mut all_actions = get_network_event_common_actions();
        all_actions.extend(advertised);

        // Build and validate the same action-only network-event prompt as call_llm.
        let (system_prompt, advertised_actions) =
            PromptBuilder::build_network_event_action_prompt_for_server_with_actions(
                &state,
                server_id,
                all_actions,
            )
            .await;
        let event_message = PromptBuilder::build_event_trigger_message_with_id(
            &event_type.id,
            &event_type.description,
            self.event_data.clone(),
        );

        let base_url = std::env::var("OLLAMA_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        // Deliberately does NOT override the completion-token budget: the
        // shipped default is what users get, so it is what the suite grades.
        // Only the wall clock is raised, because a live 27B answer legitimately
        // outruns the interactive 120s default.
        let client = OllamaClient::new(base_url)
            .with_request_timeout(std::time::Duration::from_secs(LLM_REQUEST_TIMEOUT_SECS));

        // Go through `ConversationHandler::generate_with_tools_and_retry`, which
        // is what `call_llm` calls — not the one-shot `generate_with_retry`.
        //
        // This preserves production validation and retries. Network events do not
        // advertise tools; operator and scheduled-task conversations still can.
        let mut conversation = ConversationHandler::new(
            system_prompt,
            std::sync::Arc::new(client),
            model.clone(),
            state.get_rate_limiter().await,
            RequestSource::Network,
        );

        // Native tool schemas are OFF, because `call_llm` does not attach them —
        // see tests/llm_native_tools_test.rs. This suite is what established that:
        // with schemas attached, 6 of 6 failing protocol cases (modbus x2, radius,
        // memcached, etcd, ldap) passed the moment they were removed, same model
        // and same prompts. Event responses use only the advertised actions.
        //
        // `NETGET_LIVE_NATIVE_TOOLS=1` attaches them again. That is a diagnostic for
        // re-measuring the effect, not a supported mode — it makes the harness
        // diverge from the server, which is exactly the bug that made two cases look
        // like model failures earlier in this suite's life.
        if std::env::var("NETGET_LIVE_NATIVE_TOOLS").as_deref() == Ok("1") {
            eprintln!(
                "⚠  native tool schemas ENABLED (NETGET_LIVE_NATIVE_TOOLS=1) — diagnostic \
                 mode; the server does not do this"
            );
            conversation = conversation.with_native_tools(&advertised_actions);
        }
        conversation.add_user_message(event_message);

        let actions: Vec<Value> = conversation
            .generate_with_tools_and_retry(None, WebSearchMode::Off, advertised_actions)
            .await
            .map_err(|e| format!("model call failed: {}", e))?
            .iter()
            .map(normalize_action_object)
            .collect();

        // A model may legitimately emit several actions of the same type — a BLE
        // server laying out Generic Access (0x1800) before its profile's own
        // service, say. Checking only the first would fail the case for an
        // answer that is correct, so every candidate is tried and the one that
        // satisfies the checks wins; the failure report keeps the last one's
        // detail when none does.
        let candidates: Vec<&Value> = actions
            .iter()
            .filter(|a| {
                a["type"]
                    .as_str()
                    .map(|t| accepted.iter().any(|n| n == t))
                    .unwrap_or(false)
            })
            .collect();
        let chosen = candidates.first().copied().ok_or_else(|| {
            format!(
                "model did not use {} for event '{}'. It answered: {}",
                accepted
                    .iter()
                    .map(|n| format!("'{}'", n))
                    .collect::<Vec<_>>()
                    .join(" or "),
                self.event_id,
                serde_json::to_string_pretty(&actions).unwrap_or_default()
            )
        })?;

        let mut failures = Vec::new();
        let mut chosen = chosen;
        for (index, candidate) in candidates.iter().enumerate() {
            let mut attempt_failures = Vec::new();
            for (describe, check) in &self.action_checks {
                if let Err(e) = check(candidate) {
                    attempt_failures.push(format!("{} — {}", describe, e));
                }
            }
            for pc in &self.param_checks {
                let value = candidate.get(pc.name).cloned().unwrap_or(Value::Null);
                if let Err(e) = (pc.check)(&value) {
                    attempt_failures.push(format!("{} — {}", pc.describe, e));
                }
            }
            chosen = candidate;
            failures = attempt_failures;
            if failures.is_empty() {
                if index > 0 {
                    println!(
                        "  (matched the {}{} action of this type)",
                        index + 1,
                        match index + 1 {
                            2 => "nd",
                            3 => "rd",
                            _ => "th",
                        }
                    );
                }
                break;
            }
        }
        for (describe, check) in &self.action_checks {
            if check(chosen).is_ok() {
                println!("  ✅ {}", describe);
            } else {
                println!("  ❌ {}", describe);
            }
        }
        for pc in &self.param_checks {
            let value = chosen.get(pc.name).cloned().unwrap_or(Value::Null);
            if (pc.check)(&value).is_ok() {
                println!("  ✅ {}", pc.describe);
            } else {
                println!("  ❌ {}", pc.describe);
            }
        }
        if failures.is_empty() {
            println!(
                "✅ {}::{} answered with {}",
                canonical,
                self.event_id,
                chosen["type"].as_str().unwrap_or("?")
            );
            Ok(())
        } else {
            Err(format!(
                "action '{}' chosen but {} parameter check(s) failed:\n  {}\nFull action: {}",
                chosen["type"].as_str().unwrap_or("?"),
                failures.len(),
                failures.join("\n  "),
                serde_json::to_string_pretty(chosen).unwrap_or_default()
            )
            .into())
        }
    }
}
