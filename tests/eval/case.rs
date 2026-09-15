//! What one eval case *is*: a canonical operator instruction in plain English,
//! a real third-party client to drive the server with, and a check on what that
//! client observed.
//!
//! The instruction is the input under test. It is deliberately written the way
//! an operator would type it into the dashboard — not the way a prompt engineer
//! would write it — because the thing being measured is whether the protocol's
//! own action descriptions carry the model the rest of the way.

#![allow(dead_code)]

use serde_json::Value;

/// How strong the evidence a probe produces actually is.
///
/// The repo's standing rule is that a hand-written client inside the test is an
/// independent *reading* of the spec, not an independent *implementation*. The
/// same distinction applies here, and the results table prints it, so a pass
/// driven by `nc` is never read as a pass driven by `dig`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Independence {
    /// A real, independently written client for *this* protocol: `dig`, `curl`,
    /// `redis-cli`, `psql`, `ldapsearch`, `ipptool`, `whois`, `ftp`.
    ProtocolClient,
    /// A generic transport tool (`nc`) that carries bytes but understands
    /// nothing about the protocol. The check is then only as good as the
    /// substring it looks for.
    GenericTransport,
}

impl Independence {
    pub fn label(self) -> &'static str {
        match self {
            Independence::ProtocolClient => "protocol-client",
            Independence::GenericTransport => "generic-transport",
        }
    }
}

/// How this case reaches the server.
#[derive(Clone, Debug)]
pub enum ProbeKind {
    /// Run an external client binary.
    Command(Probe),
    /// No installed client can drive this protocol on an unprivileged
    /// loopback port. Recorded with the reason rather than silently skipped —
    /// a skip that looks like a pass is the failure mode this repo keeps
    /// finding in its own suite.
    Unavailable { reason: &'static str },
}

/// An external client invocation. `{PORT}`, `{HOST}` and `{ADDR}` in `args`,
/// `stdin` and env values are substituted before the process is spawned.
#[derive(Clone, Debug)]
pub struct Probe {
    pub bin: &'static str,
    pub args: Vec<String>,
    pub stdin: Option<String>,
    pub env: Vec<(String, String)>,
    pub independence: Independence,
    /// Keep the client's stdin open for the whole exchange.
    ///
    /// `nc` closes the socket when stdin reaches EOF, so writing the payload and
    /// dropping the handle hangs up on the server before the model has been
    /// asked — every raw-TCP case failed that way until this existed. Clients
    /// that own their own connection lifecycle (curl, dig, psql, ftp) want the
    /// EOF and set this false.
    pub hold_stdin: bool,
}

impl Probe {
    /// A real client for the protocol under test.
    pub fn client(bin: &'static str, args: &[&str]) -> Self {
        Self {
            bin,
            args: args.iter().map(|a| a.to_string()).collect(),
            stdin: None,
            env: Vec::new(),
            independence: Independence::ProtocolClient,
            hold_stdin: false,
        }
    }

    /// A generic byte pipe (`nc`). Weaker evidence; labelled as such, and it
    /// holds stdin open because closing it is a hang-up.
    pub fn generic(bin: &'static str, args: &[&str]) -> Self {
        Self {
            independence: Independence::GenericTransport,
            hold_stdin: true,
            ..Self::client(bin, args)
        }
    }

    pub fn stdin(mut self, data: &str) -> Self {
        self.stdin = Some(data.to_string());
        self
    }

    pub fn env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    /// Human-readable command line, for the results table.
    pub fn describe(&self) -> String {
        format!("{} {}", self.bin, self.args.join(" "))
    }
}

/// What the client must observe for the case to pass.
///
/// Checks are substring-based and case-insensitive on purpose: a model that
/// answers `Hello, world!` where the operator said "say hello" has done the job,
/// and an eval that demands an exact byte string measures prompt-following
/// pedantry rather than protocol usability.
#[derive(Clone, Debug, Default)]
pub struct Expect {
    /// Every one of these must appear in the probe's combined stdout+stderr.
    pub all_of: Vec<String>,
    /// None of these may appear.
    pub none_of: Vec<String>,
    /// Optional regex over the probe's combined output.
    pub regex: Option<String>,
    /// Every one of these must name an action netget actually **executed**.
    ///
    /// The only observable for one-way protocols — syslog writes nothing back,
    /// so there is no wire to check.
    ///
    /// Deliberately not "appears anywhere in the log", and the first version of
    /// it was: that produced a **false 3/3 for syslog**, because netget dumps a
    /// rejected model reply into the log verbatim, so the check matched
    /// `{"type": "ignore_syslog_message"}` inside a reply that had been *thrown
    /// away*. The model naming an action and netget running it are different
    /// events, and only the second one is a pass.
    pub executed_actions_all_of: Vec<String>,
}

impl Expect {
    pub fn contains(needles: &[&str]) -> Self {
        Self {
            all_of: needles.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    pub fn not_containing(mut self, needles: &[&str]) -> Self {
        self.none_of = needles.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn matching(mut self, pattern: &str) -> Self {
        self.regex = Some(pattern.to_string());
        self
    }

    /// Assert netget executed an action of each name, for protocols with no
    /// reply to inspect.
    pub fn executed_action(needles: &[&str]) -> Self {
        Self {
            executed_actions_all_of: needles.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    pub fn and_executed_action(mut self, needles: &[&str]) -> Self {
        self.executed_actions_all_of = needles.iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if !self.all_of.is_empty() {
            parts.push(format!("contains {:?}", self.all_of));
        }
        if !self.none_of.is_empty() {
            parts.push(format!("lacks {:?}", self.none_of));
        }
        if let Some(r) = &self.regex {
            parts.push(format!("matches /{}/", r));
        }
        if !self.executed_actions_all_of.is_empty() {
            parts.push(format!("executes {:?}", self.executed_actions_all_of));
        }
        parts.join(" and ")
    }

    /// Evaluate against one probe run. `Ok(())` is a pass.
    pub fn check(&self, probe_output: &str, executed: &str) -> Result<(), String> {
        let hay = probe_output.to_lowercase();
        for needle in &self.all_of {
            if !hay.contains(&needle.to_lowercase()) {
                return Err(format!("client output does not contain {:?}", needle));
            }
        }
        for needle in &self.none_of {
            if hay.contains(&needle.to_lowercase()) {
                return Err(format!("client output unexpectedly contains {:?}", needle));
            }
        }
        if let Some(pattern) = &self.regex {
            match regex::Regex::new(pattern) {
                Ok(re) => {
                    if !re.is_match(probe_output) {
                        return Err(format!("client output does not match /{}/", pattern));
                    }
                }
                // A bad pattern is the harness's bug, not the model's; say so
                // rather than scoring it as a model miss.
                Err(e) => return Err(format!("HARNESS: bad regex /{}/: {}", pattern, e)),
            }
        }
        // `executed` carries only netget's `Executing action` lines, never the
        // whole log — see the field's own note about the false syslog pass.
        let executed_lc = executed.to_lowercase();
        for needle in &self.executed_actions_all_of {
            if !executed_lc.contains(&needle.to_lowercase()) {
                return Err(format!("netget executed no {:?} action", needle));
            }
        }
        Ok(())
    }
}

/// One canonical operator instruction, and how to tell whether the model
/// managed to serve it.
pub struct EvalCase {
    /// Stable identifier, `protocol/slug`. Used as the key in the results file,
    /// so it must not change once published.
    pub id: &'static str,
    /// Registry stack name, passed straight to `netget --server`.
    pub protocol: &'static str,
    /// The operator instruction, in plain English. This is the input under test.
    pub instruction: &'static str,
    /// `--server-params` for protocols that consult the model only when
    /// configured to (SOCKS5's `filter_mode`, and friends).
    pub server_params: Option<Value>,
    pub probe: ProbeKind,
    pub expect: Expect,
    /// Anything a reader of the results needs to know about this case.
    pub note: Option<&'static str>,
}

impl EvalCase {
    pub fn new(
        id: &'static str,
        protocol: &'static str,
        instruction: &'static str,
        probe: Probe,
        expect: Expect,
    ) -> Self {
        Self {
            id,
            protocol,
            instruction,
            server_params: None,
            probe: ProbeKind::Command(probe),
            expect,
            note: None,
        }
    }

    /// A case whose protocol has no usable installed client. Recorded, never
    /// silently passed.
    pub fn unavailable(
        id: &'static str,
        protocol: &'static str,
        instruction: &'static str,
        reason: &'static str,
    ) -> Self {
        Self {
            id,
            protocol,
            instruction,
            server_params: None,
            probe: ProbeKind::Unavailable { reason },
            expect: Expect::default(),
            note: None,
        }
    }

    pub fn params(mut self, params: Value) -> Self {
        self.server_params = Some(params);
        self
    }

    pub fn note(mut self, note: &'static str) -> Self {
        self.note = Some(note);
        self
    }
}
