//! Protocol picker for `+ new server or client`: every server and client
//! protocol in one filterable list with maturity badges and a preview of what
//! the protocol is and needs. `http server` and `http client` sit next to
//! each other, so there is no kind to choose before choosing.

use crate::privilege::SystemCapabilities;
use crate::protocol::metadata::DevelopmentState;
use crate::tui::app::Section;

#[derive(Debug, Clone)]
pub struct ProtocolEntry {
    /// Server or client.
    pub kind: Section,
    pub name: String,
    pub description: String,
    pub state: DevelopmentState,
    pub notes: Option<String>,
    /// Default port, when the protocol declares a port-based binding.
    pub default_port: Option<u16>,
    /// `None` when the protocol declares no binding defaults, meaning a port
    /// must be supplied explicitly.
    pub has_binding_defaults: bool,
    /// Set when this build cannot satisfy the protocol's privilege
    /// requirement; the entry stays listed but says why it will refuse.
    pub privilege_note: Option<String>,
}

impl ProtocolEntry {
    pub fn kind_label(&self) -> &'static str {
        match self.kind {
            Section::Servers => "server",
            Section::Clients => "client",
        }
    }

    pub fn badge(&self) -> &'static str {
        match self.state {
            DevelopmentState::Stable => "[stable]",
            DevelopmentState::Beta => "[beta]",
            DevelopmentState::Experimental => "[exp]",
            DevelopmentState::Incomplete => "[incomplete]",
        }
    }
}

/// All protocols available for a section, sorted by name. The client registry
/// returns names unsorted, so both are sorted here.
pub fn entries(section: Section, caps: &SystemCapabilities) -> Vec<ProtocolEntry> {
    let mut entries = match section {
        Section::Servers => {
            let registry = crate::protocol::server_registry::registry();
            registry
                .available_protocols()
                .into_iter()
                .filter_map(|name| {
                    let protocol = registry.get(name)?;
                    let metadata = protocol.metadata();
                    let binding = protocol.default_binding();
                    let privilege_note = if metadata.privilege_requirement.is_met_by(caps) {
                        None
                    } else {
                        Some(format!(
                            "needs {:?} — will refuse to start in this process",
                            metadata.privilege_requirement
                        ))
                    };
                    Some(ProtocolEntry {
                        kind: Section::Servers,
                        name: name.to_string(),
                        description: protocol.description().to_string(),
                        state: metadata.state,
                        notes: metadata.notes.map(|n| n.to_string()),
                        default_port: binding.as_ref().and_then(|b| b.port),
                        has_binding_defaults: binding.is_some(),
                        privilege_note,
                    })
                })
                .collect::<Vec<_>>()
        }
        Section::Clients => {
            let registry = &crate::protocol::CLIENT_REGISTRY;
            registry
                .list_protocols()
                .into_iter()
                .filter_map(|name| {
                    let protocol = registry.get(&name)?;
                    let metadata = protocol.metadata();
                    Some(ProtocolEntry {
                        kind: Section::Clients,
                        name: name.clone(),
                        description: protocol.description().to_string(),
                        state: metadata.state,
                        notes: metadata.notes.map(|n| n.to_string()),
                        default_port: None,
                        has_binding_defaults: false,
                        privilege_note: None,
                    })
                })
                .collect::<Vec<_>>()
        }
    };
    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries
}

/// Both kinds together, sorted by name with the server before the client of
/// the same protocol.
pub fn all_entries(caps: &SystemCapabilities) -> Vec<ProtocolEntry> {
    let mut all = entries(Section::Servers, caps);
    all.extend(entries(Section::Clients, caps));
    all.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| (a.kind == Section::Clients).cmp(&(b.kind == Section::Clients)))
    });
    all
}

/// Case-insensitive filter over name, kind and description, ranked by how
/// well the match fits the name.
///
/// Ranking matters once every protocol is compiled in: typing "tcp" matches
/// Modbus, MQTT and a dozen others whose *description* mentions TCP, and an
/// alphabetical list puts one of those first. Someone typing "tcp" wants TCP.
/// Words after the first narrow further — `http client` is the http client.
pub fn filter<'a>(entries: &'a [ProtocolEntry], needle: &str) -> Vec<&'a ProtocolEntry> {
    let needle = needle.to_lowercase();
    let mut words = needle.split_whitespace();
    let Some(first) = words.next() else {
        return entries.iter().collect();
    };
    let rest: Vec<&str> = words.collect();

    let mut matches: Vec<(u8, &ProtocolEntry)> = entries
        .iter()
        .filter_map(|entry| {
            let name = entry.name.to_lowercase();
            let kind = entry.kind_label();
            let description = entry.description.to_lowercase();
            let rank = if name == first || (kind.starts_with(first) && rest.is_empty()) {
                0
            } else if name.starts_with(first) {
                1
            } else if name.contains(first) {
                2
            } else if description.contains(first) {
                3
            } else {
                return None;
            };
            let narrowed = rest.iter().all(|word| {
                name.contains(word) || kind.starts_with(word) || description.contains(word)
            });
            narrowed.then_some((rank, entry))
        })
        .collect();

    // Stable sort keeps the alphabetical order within each rank.
    matches.sort_by_key(|(rank, _)| *rank);
    matches.into_iter().map(|(_, entry)| entry).collect()
}
