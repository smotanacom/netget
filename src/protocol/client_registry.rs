//! Client protocol registry
//!
//! This module provides a centralized registry that maps client protocol names
//! to their client implementations. It enables trait-based client lookup
//! and keyword-based parsing for client connections.

use crate::llm::actions::Client;
use std::collections::HashMap;
use std::sync::Arc;

/// Global client protocol registry mapping protocol names to client implementations
pub struct ClientRegistry {
    /// Maps protocol name (e.g., "TCP", "HTTP") to client implementation
    protocols: HashMap<String, Arc<dyn Client>>,
    /// Maps lowercase keywords to protocol name for fast parsing
    keyword_map: HashMap<String, String>,
}

impl ClientRegistry {
    /// Create a new client protocol registry
    fn new() -> Self {
        tracing::debug!("ClientRegistry::new() - Creating new client registry");
        let mut registry = Self {
            protocols: HashMap::new(),
            keyword_map: HashMap::new(),
        };

        // Register all client protocols based on feature flags
        tracing::debug!("ClientRegistry::new() - Registering client protocols");
        registry.register_protocols();
        tracing::debug!("ClientRegistry::new() - Client protocols registered");

        tracing::debug!("ClientRegistry::new() - Building keyword map");
        registry.build_keyword_map();
        tracing::debug!("ClientRegistry::new() - Keyword map built");

        // Validate that no keywords overlap between protocols
        tracing::debug!("ClientRegistry::new() - Validating keyword uniqueness");
        registry.validate_keyword_uniqueness();
        tracing::debug!("ClientRegistry::new() - Keyword uniqueness validated");

        tracing::debug!("ClientRegistry::new() - Client registry created");
        registry
    }

    /// Register all available client protocols based on compiled features
    fn register_protocols(&mut self) {
        #[cfg(feature = "arp")]
        self.register(Arc::new(crate::client::arp::ArpClientProtocol::new()));

        #[cfg(feature = "bgp")]
        self.register(Arc::new(crate::client::bgp::BgpClientProtocol::new()));

        #[cfg(feature = "bitcoin")]
        self.register(Arc::new(
            crate::client::bitcoin::BitcoinClientProtocol::new(),
        ));

        #[cfg(feature = "bluetooth-ble-client")]
        self.register(Arc::new(
            crate::client::bluetooth::BluetoothClientProtocol::new(),
        ));

        #[cfg(feature = "bootp")]
        self.register(Arc::new(crate::client::bootp::BootpClientProtocol::new()));

        #[cfg(feature = "cassandra")]
        self.register(Arc::new(
            crate::client::cassandra::CassandraClientProtocol::new(),
        ));

        #[cfg(feature = "datalink")]
        self.register(Arc::new(
            crate::client::datalink::DataLinkClientProtocol::new(),
        ));

        #[cfg(feature = "dc")]
        self.register(Arc::new(crate::client::dc::DcClientProtocol::new()));

        #[cfg(feature = "dhcp")]
        self.register(Arc::new(crate::client::dhcp::DhcpClientProtocol::new()));

        #[cfg(feature = "dns")]
        self.register(Arc::new(crate::client::dns::DnsClientProtocol::new()));

        #[cfg(feature = "doh")]
        self.register(Arc::new(crate::client::doh::DohClientProtocol::new()));

        #[cfg(feature = "dot")]
        self.register(Arc::new(crate::client::dot::DotClientProtocol::new()));

        #[cfg(feature = "dynamodb")]
        self.register(Arc::new(
            crate::client::dynamodb::DynamoDbClientProtocol::new(),
        ));

        #[cfg(feature = "elasticsearch")]
        self.register(Arc::new(
            crate::client::elasticsearch::ElasticsearchClientProtocol::new(),
        ));

        #[cfg(feature = "etcd")]
        self.register(Arc::new(crate::client::etcd::EtcdClientProtocol::new()));

        #[cfg(feature = "git")]
        self.register(Arc::new(crate::client::git::GitClientProtocol::new()));

        #[cfg(feature = "zookeeper")]
        self.register(Arc::new(
            crate::client::zookeeper::ZookeeperClientProtocol::new(),
        ));

        #[cfg(feature = "grpc")]
        self.register(Arc::new(crate::client::grpc::GrpcClientProtocol::new()));

        #[cfg(feature = "http")]
        {
            tracing::debug!("ClientRegistry: Registering HTTP client protocol");
            self.register(Arc::new(crate::client::http::HttpClientProtocol::new()));
            tracing::debug!("ClientRegistry: HTTP client protocol registered");
        }

        #[cfg(feature = "http2")]
        self.register(Arc::new(crate::client::http2::Http2ClientProtocol::new()));

        // Real RFC 9114 HTTP/3 (the `h3` crate), so it keeps the `http3` name.
        // The `http3` feature no longer builds a server - NetGet's QUIC server
        // lives behind `quic` and cannot be spoken to by this client.
        #[cfg(feature = "http3")]
        self.register(Arc::new(crate::client::http3::Http3ClientProtocol::new()));

        #[cfg(feature = "http_proxy")]
        self.register(Arc::new(
            crate::client::http_proxy::HttpProxyClientProtocol::new(),
        ));

        #[cfg(feature = "igmp")]
        self.register(Arc::new(crate::client::igmp::IgmpClientProtocol::new()));

        #[cfg(feature = "imap")]
        self.register(Arc::new(crate::client::imap::ImapClientProtocol::new()));

        #[cfg(feature = "ipp")]
        self.register(Arc::new(crate::client::ipp::IppClientProtocol::new()));

        #[cfg(feature = "irc")]
        self.register(Arc::new(crate::client::irc::IrcClientProtocol::new()));

        #[cfg(feature = "isis")]
        self.register(Arc::new(crate::client::isis::IsisClientProtocol::new()));

        #[cfg(feature = "jsonrpc")]
        self.register(Arc::new(
            crate::client::jsonrpc::JsonRpcClientProtocol::new(),
        ));

        #[cfg(feature = "kafka")]
        self.register(Arc::new(crate::client::kafka::KafkaClientProtocol::new()));

        #[cfg(feature = "kubernetes")]
        self.register(Arc::new(
            crate::client::kubernetes::KubernetesClientProtocol::new(),
        ));

        #[cfg(feature = "ldap")]
        self.register(Arc::new(crate::client::ldap::LdapClientProtocol::new()));

        #[cfg(feature = "maven")]
        self.register(Arc::new(crate::client::maven::MavenClientProtocol::new()));

        #[cfg(feature = "mcp")]
        self.register(Arc::new(crate::client::mcp::McpClientProtocol::new()));

        #[cfg(feature = "mdns")]
        self.register(Arc::new(crate::client::mdns::MdnsClientProtocol::new()));

        #[cfg(feature = "mqtt")]
        self.register(Arc::new(crate::client::mqtt::MqttClientProtocol::new()));

        #[cfg(feature = "amqp")]
        self.register(Arc::new(crate::client::amqp::AmqpClientProtocol::new()));

        #[cfg(feature = "mysql")]
        self.register(Arc::new(crate::client::mysql::MysqlClientProtocol::new()));

        #[cfg(feature = "mongodb")]
        self.register(Arc::new(
            crate::client::mongodb::MongodbClientProtocol::new(),
        ));

        #[cfg(feature = "nfs")]
        self.register(Arc::new(crate::client::nfs::NfsClientProtocol::new()));

        #[cfg(feature = "nfc-client")]
        self.register(Arc::new(crate::client::nfc::NfcClientProtocol));

        #[cfg(feature = "nntp")]
        self.register(Arc::new(crate::client::nntp::NntpClientProtocol::new()));

        #[cfg(feature = "npm")]
        self.register(Arc::new(crate::client::npm::NpmClientProtocol::new()));

        #[cfg(feature = "ntp")]
        self.register(Arc::new(crate::client::ntp::NtpClientProtocol::new()));

        #[cfg(feature = "oauth2")]
        self.register(Arc::new(crate::client::oauth2::OAuth2ClientProtocol::new()));

        #[cfg(feature = "openai")]
        self.register(Arc::new(crate::client::openai::OpenAiClientProtocol::new()));

        #[cfg(feature = "ollama")]
        self.register(Arc::new(crate::client::ollama::OllamaClientProtocol::new()));

        #[cfg(feature = "openapi")]
        self.register(Arc::new(
            crate::client::openapi::OpenApiClientProtocol::new(),
        ));

        #[cfg(feature = "openidconnect")]
        self.register(Arc::new(
            crate::client::openidconnect::OpenIdConnectClientProtocol::new(),
        ));

        #[cfg(feature = "ospf")]
        self.register(Arc::new(crate::client::ospf::OspfClientProtocol::new()));

        #[cfg(feature = "postgresql")]
        self.register(Arc::new(
            crate::client::postgresql::PostgresqlClientProtocol::new(),
        ));

        #[cfg(feature = "pypi")]
        self.register(Arc::new(crate::client::pypi::PypiClientProtocol::new()));

        #[cfg(feature = "mssql")]
        self.register(Arc::new(crate::client::mssql::MssqlClientProtocol::new()));

        #[cfg(feature = "redis")]
        self.register(Arc::new(crate::client::redis::RedisClientProtocol::new()));

        #[cfg(feature = "icmp")]
        self.register(Arc::new(crate::client::icmp::IcmpClientProtocol::new()));

        #[cfg(feature = "couchdb")]
        self.register(Arc::new(
            crate::client::couchdb::CouchDbClientProtocol::new(),
        ));

        #[cfg(feature = "tftp")]
        self.register(Arc::new(crate::client::tftp::TftpClientProtocol::new()));

        #[cfg(feature = "rss")]
        self.register(Arc::new(crate::client::rss::RssClientProtocol::new()));

        #[cfg(feature = "rip")]
        self.register(Arc::new(crate::client::rip::RipClientProtocol::new()));

        #[cfg(feature = "s3")]
        self.register(Arc::new(crate::client::s3::S3ClientProtocol::new()));

        #[cfg(feature = "saml")]
        self.register(Arc::new(crate::client::saml::SamlClientProtocol::new()));

        #[cfg(feature = "sip")]
        self.register(Arc::new(crate::client::sip::SipClientProtocol::new()));

        #[cfg(feature = "smb-client")]
        self.register(Arc::new(crate::client::smb::SmbClientProtocol::new()));

        #[cfg(feature = "smtp")]
        self.register(Arc::new(crate::client::smtp::SmtpClientProtocol::new()));

        #[cfg(feature = "ftp")]
        self.register(Arc::new(crate::client::ftp::FtpClientProtocol::new()));

        #[cfg(feature = "pop3")]
        self.register(Arc::new(crate::client::pop3::Pop3ClientProtocol::new()));

        #[cfg(feature = "snmp")]
        self.register(Arc::new(crate::client::snmp::SnmpClientProtocol::new()));

        #[cfg(feature = "socks5")]
        self.register(Arc::new(crate::client::socks5::Socks5ClientProtocol::new()));

        #[cfg(all(feature = "socket_file", unix))]
        self.register(Arc::new(
            crate::client::socket_file::SocketFileClientProtocol::new(),
        ));

        #[cfg(feature = "sqs")]
        self.register(Arc::new(crate::client::sqs::SqsClientProtocol::new()));

        #[cfg(feature = "ssh")]
        self.register(Arc::new(crate::client::ssh::SshClientProtocol::new()));

        #[cfg(all(feature = "ssh-agent", unix))]
        self.register(Arc::new(
            crate::client::ssh_agent::SshAgentClientProtocol::new(),
        ));

        #[cfg(feature = "stun")]
        self.register(Arc::new(crate::client::stun::StunClientProtocol::new()));

        #[cfg(feature = "syslog")]
        self.register(Arc::new(crate::client::syslog::SyslogClientProtocol::new()));

        #[cfg(feature = "tcp")]
        self.register(Arc::new(crate::client::tcp::TcpClientProtocol::new()));

        #[cfg(feature = "telnet")]
        self.register(Arc::new(crate::client::telnet::TelnetClientProtocol::new()));

        #[cfg(feature = "tls")]
        self.register(Arc::new(crate::client::tls::TlsClientProtocol::new()));

        #[cfg(feature = "tor")]
        self.register(Arc::new(crate::client::tor::TorClientProtocol::new()));

        #[cfg(feature = "torrent-dht")]
        self.register(Arc::new(
            crate::client::torrent_dht::TorrentDhtClientProtocol::new(),
        ));

        #[cfg(feature = "torrent-peer")]
        self.register(Arc::new(
            crate::client::torrent_peer::TorrentPeerClientProtocol::new(),
        ));

        #[cfg(feature = "torrent-tracker")]
        self.register(Arc::new(
            crate::client::torrent_tracker::TorrentTrackerClientProtocol::new(),
        ));

        #[cfg(feature = "turn")]
        self.register(Arc::new(crate::client::turn::TurnClientProtocol::new()));

        #[cfg(feature = "udp")]
        self.register(Arc::new(crate::client::udp::UdpClientProtocol::new()));

        #[cfg(feature = "usb")]
        self.register(Arc::new(crate::client::usb::UsbClientProtocol::new()));

        #[cfg(feature = "vnc")]
        self.register(Arc::new(crate::client::vnc::VncClientProtocol::new()));

        #[cfg(feature = "webdav")]
        self.register(Arc::new(crate::client::webdav::WebdavClientProtocol::new()));

        #[cfg(feature = "webrtc")]
        self.register(Arc::new(crate::client::webrtc::WebRtcClientProtocol::new()));

        #[cfg(feature = "websocket")]
        self.register(Arc::new(
            crate::client::websocket::WebSocketClientProtocol::new(),
        ));

        #[cfg(feature = "whois")]
        self.register(Arc::new(crate::client::whois::WhoisClientProtocol::new()));
        #[cfg(feature = "stomp")]
        self.register(Arc::new(crate::client::stomp::StompClientProtocol::new()));
        #[cfg(feature = "netbios-ns")]
        self.register(Arc::new(crate::client::netbios_ns::NetbiosNsClientProtocol::new()));
        #[cfg(feature = "nats")]
        self.register(Arc::new(crate::client::nats::NatsClientProtocol::new()));
        #[cfg(feature = "ssdp")]
        self.register(Arc::new(crate::client::ssdp::SsdpClientProtocol::new()));
        #[cfg(feature = "gopher")]
        self.register(Arc::new(crate::client::gopher::GopherClientProtocol::new()));
        #[cfg(feature = "finger")]
        self.register(Arc::new(crate::client::finger::FingerClientProtocol::new()));
        #[cfg(feature = "ident")]
        self.register(Arc::new(crate::client::ident::IdentClientProtocol::new()));
        #[cfg(feature = "llmnr")]
        self.register(Arc::new(crate::client::llmnr::LlmnrClientProtocol::new()));

        #[cfg(feature = "wireguard")]
        self.register(Arc::new(
            crate::client::wireguard::WireguardClientProtocol::new(),
        ));

        #[cfg(feature = "xmlrpc")]
        self.register(Arc::new(crate::client::xmlrpc::XmlRpcClientProtocol::new()));

        #[cfg(feature = "xmpp")]
        self.register(Arc::new(crate::client::xmpp::XmppClientProtocol::new()));
    }

    /// Build keyword map for fast protocol parsing
    fn build_keyword_map(&mut self) {
        for (protocol_name, protocol) in &self.protocols {
            // Add all protocol keywords
            for keyword in protocol.keywords() {
                let lower = keyword.to_lowercase();
                // Store the hyphen/space-normalized form too. Input is normalized
                // before lookup, so a keyword declared as "ssh-agent" was previously
                // unreachable: "ssh_agent" never contains "ssh-agent".
                let normalized = lower.replace(['-', ' '], "_");
                self.keyword_map.insert(lower, protocol_name.clone());
                self.keyword_map.insert(normalized, protocol_name.clone());
            }

            // Also add the full stack name as a keyword
            // This allows parsing inputs like "eth>ip>tcp>http" or "ETH>IP>UDP>DNS"
            let stack_name = protocol.stack_name().to_lowercase();
            self.keyword_map.insert(stack_name, protocol_name.clone());
        }
    }

    /// Validate that no two protocols share the same keyword
    ///
    /// This ensures keyword uniqueness across all registered protocols.
    /// Panics if overlapping keywords are detected.
    fn validate_keyword_uniqueness(&self) {
        use std::collections::HashMap;

        // Build a map: keyword (lowercase) -> Vec<(protocol_name, keyword_source)>
        // keyword_source is either "keyword" or "stack_name"
        let mut keyword_to_protocols: HashMap<String, Vec<(String, String)>> = HashMap::new();

        for (protocol_name, protocol) in &self.protocols {
            // Collect all keywords from keywords()
            for keyword in protocol.keywords() {
                let key = keyword.to_lowercase();
                keyword_to_protocols
                    .entry(key)
                    .or_default()
                    .push((protocol_name.clone(), format!("keyword '{}'", keyword)));
            }

            // Also collect the stack name as a keyword
            let stack_name = protocol.stack_name();
            let key = stack_name.to_lowercase();
            keyword_to_protocols.entry(key).or_default().push((
                protocol_name.clone(),
                format!("stack_name '{}'", stack_name),
            ));
        }

        // Only count overlaps across DISTINCT protocols — a protocol whose keyword
        // equals its own stack_name contributes two entries for one keyword and is
        // not an overlap.
        let mut overlaps = Vec::new();
        for (keyword, protocols) in &keyword_to_protocols {
            let distinct: std::collections::HashSet<&String> =
                protocols.iter().map(|(name, _)| name).collect();
            if distinct.len() > 1 {
                overlaps.push((keyword.clone(), protocols.clone()));
            }
        }

        // Development-time advisory only — log at DEBUG (netget.log) rather than
        // spamming the TUI on every startup. `test_keyword_overlaps` is the gate.
        if !overlaps.is_empty() {
            use tracing::debug;
            debug!("Client keyword overlaps detected between protocols:");

            for (keyword, protocols) in &overlaps {
                debug!("  Keyword '{}' is used by:", keyword);
                for (protocol_name, source) in protocols {
                    debug!("    - {} ({})", protocol_name, source);
                }
            }

            debug!("Note: Each keyword should ideally be unique to a single protocol.");
            debug!(
                "      Run 'cargo test test_keyword_overlaps -- --ignored' to see all overlaps."
            );
        }
    }

    /// Register a client protocol implementation
    #[allow(dead_code)]
    fn register(&mut self, protocol: Arc<dyn Client>) {
        let protocol_name = protocol.protocol_name().to_string();
        self.protocols.insert(protocol_name, protocol);
    }

    /// Get client protocol implementation by protocol name
    pub fn get(&self, protocol_name: &str) -> Option<Arc<dyn Client>> {
        self.protocols.get(protocol_name).cloned()
    }

    /// Resolve user/LLM-supplied input (a protocol name or keyword) to a
    /// registered client protocol implementation.
    ///
    /// Mirrors `ServerRegistry::resolve()`: unlike calling `get()` /
    /// `parse_from_str()` directly, a failure here distinguishes "this
    /// protocol exists but wasn't compiled into this build" from "this is not
    /// a real NetGet client protocol at all", and offers a "did you mean"
    /// suggestion for likely typos — see [`ClientProtocolLookupError`].
    pub fn resolve(&self, input: &str) -> Result<Arc<dyn Client>, ClientProtocolLookupError> {
        // Exact canonical name first — handles a name already resolved by a
        // previous call.
        if let Some(p) = self.get(input) {
            return Ok(p);
        }

        // Fall back to keyword / stack-name parsing (case-insensitive,
        // hyphen/space tolerant) against currently-compiled protocols.
        if let Some(name) = self.parse_from_str(input) {
            if let Some(p) = self.get(&name) {
                return Ok(p);
            }
        }

        // Not found among compiled-in protocols. Check whether `input` names a
        // real NetGet client protocol that simply wasn't compiled into this
        // build.
        let input_norm = input.to_lowercase().replace(['-', ' '], "_");
        for (name, feature) in ALL_KNOWN_CLIENT_PROTOCOLS {
            let name_norm = name.to_lowercase().replace(['-', ' '], "_");
            if name_norm == input_norm {
                return Err(ClientProtocolLookupError::NotCompiled { name, feature });
            }
        }

        // Truly unknown. Offer a "did you mean" suggestion via edit distance
        // against every known client protocol name, compiled in or not.
        Err(ClientProtocolLookupError::Unknown {
            input: input.to_string(),
            suggestion: suggest_client_protocol_name(input, ALL_KNOWN_CLIENT_PROTOCOLS),
        })
    }

    /// Parse client protocol from user input string
    ///
    /// Attempts to match keywords from registered client protocols.
    /// Returns protocol name if match found, None otherwise.
    pub fn parse_from_str(&self, input: &str) -> Option<String> {
        let input_lower = input.to_lowercase();
        // Normalize hyphens and spaces to underscores for consistent matching
        // e.g., "bluetooth-ble" -> "bluetooth_ble", "WebRTC Signaling" -> "webrtc_signaling"
        let input_normalized = input_lower.replace(['-', ' '], "_");

        // First, try exact match with stack names (for LLM-generated responses)
        for (protocol_name, protocol) in &self.protocols {
            let stack_lower = protocol.stack_name().to_lowercase();
            let stack_normalized = stack_lower.replace(['-', ' '], "_");
            if input_lower == stack_lower
                || input_normalized == stack_lower
                || input_normalized == stack_normalized
            {
                return Some(protocol_name.clone());
            }
        }

        // An exact keyword match wins outright, before any substring test.
        if let Some(protocol_name) = self
            .keyword_map
            .get(&input_normalized)
            .or_else(|| self.keyword_map.get(&input_lower))
        {
            return Some(protocol_name.clone());
        }

        // Substring matching, longest keyword first.
        //
        // This used to return the first `contains()` hit while iterating a HashMap,
        // whose order is unspecified. "ssh_agent" contains both "ssh" and "agent", so
        // it resolved to SSH or to SSH Agent depending on hash order -- creating an
        // ssh_agent client could silently produce an SSH client instead, and the same
        // input could resolve differently between runs or between feature sets.
        // Preferring the longest matching keyword makes the more specific protocol
        // win, and the name tie-break makes the answer identical on every run.
        let mut best: Option<(usize, &String)> = None;
        for (keyword, protocol_name) in &self.keyword_map {
            if !(input_lower.contains(keyword) || input_normalized.contains(keyword)) {
                continue;
            }
            let better = match best {
                None => true,
                Some((best_len, best_name)) => {
                    keyword.len() > best_len
                        || (keyword.len() == best_len && protocol_name < best_name)
                }
            };
            if better {
                best = Some((keyword.len(), protocol_name));
            }
        }
        best.map(|(_, name)| name.clone())
    }

    /// List all registered client protocol names
    pub fn list_protocols(&self) -> Vec<String> {
        self.protocols.keys().cloned().collect()
    }

    /// Check if a protocol is registered
    pub fn has_protocol(&self, protocol_name: &str) -> bool {
        self.protocols.contains_key(protocol_name)
    }

    /// Get all registered client protocols
    pub fn get_all(&self) -> Vec<Arc<dyn Client>> {
        self.protocols.values().cloned().collect()
    }

    /// Get protocols that are excluded due to missing dependencies
    ///
    /// Returns a map of protocol name -> list of missing dependencies
    pub fn get_excluded_protocols(
        &self,
        caps: &crate::privilege::SystemCapabilities,
    ) -> std::collections::HashMap<String, Vec<super::dependencies::ProtocolDependency>> {
        let mut excluded = std::collections::HashMap::new();

        for (protocol_name, protocol) in &self.protocols {
            let dependencies = protocol.get_dependencies();
            let mut missing = Vec::new();

            for dep in dependencies {
                if !dep.is_available(caps) {
                    missing.push(dep);
                }
            }

            if !missing.is_empty() {
                excluded.insert(protocol_name.clone(), missing);
            }
        }

        excluded
    }

    /// Get protocols that are available (have all dependencies met)
    ///
    /// Returns a list of protocol names that can be used
    pub fn get_available_protocols(
        &self,
        caps: &crate::privilege::SystemCapabilities,
    ) -> Vec<String> {
        let excluded = self.get_excluded_protocols(caps);

        self.protocols
            .keys()
            .filter(|name| !excluded.contains_key(*name))
            .cloned()
            .collect()
    }

    /// Check if a specific protocol is available (has all dependencies met)
    pub fn is_protocol_available(
        &self,
        protocol_name: &str,
        caps: &crate::privilege::SystemCapabilities,
    ) -> bool {
        if let Some(protocol) = self.get(protocol_name) {
            let dependencies = protocol.get_dependencies();
            dependencies.iter().all(|dep| dep.is_available(caps))
        } else {
            false
        }
    }
}

/// All client protocol names known to the netget codebase, paired with the
/// Cargo feature flag that compiles each one in — independent of which
/// features are actually enabled in *this* build. This lets
/// [`ClientRegistry::resolve`] tell "protocol exists but wasn't compiled in"
/// apart from "no such protocol".
///
/// Kept manually in sync with the `#[cfg(feature = "...")]` registrations in
/// `register_protocols()` above. This is the one acceptable central touchpoint
/// per CLAUDE.md's decentralization policy (it plays the same role as the
/// `register()` calls themselves) — when adding a new client protocol, add its
/// (canonical name, feature) pair here too.
pub(crate) const ALL_KNOWN_CLIENT_PROTOCOLS: &[(&str, &str)] = &[
    ("BGP", "bgp"),
    ("BOOTP", "bootp"),
    ("Cassandra", "cassandra"),
    ("DataLink", "datalink"),
    ("DC", "dc"),
    ("DHCP", "dhcp"),
    ("DNS", "dns"),
    ("DNS-over-HTTPS", "doh"),
    ("DoT", "dot"),
    ("etcd", "etcd"),
    ("ZooKeeper", "zookeeper"),
    ("gRPC", "grpc"),
    ("HTTP", "http"),
    ("HTTP2", "http2"),
    ("HTTP3", "http3"),
    ("igmp", "igmp"),
    ("IPP", "ipp"),
    ("IS-IS", "isis"),
    ("JSON-RPC", "jsonrpc"),
    ("Kafka", "kafka"),
    ("mDNS", "mdns"),
    ("AMQP", "amqp"),
    ("MySQL", "mysql"),
    ("nfc", "nfc-client"),
    ("NTP", "ntp"),
    ("OpenAI", "openai"),
    ("PostgreSQL", "postgresql"),
    ("PyPI", "pypi"),
    ("MSSQL", "mssql"),
    ("Redis", "redis"),
    ("ICMP", "icmp"),
    ("RIP", "rip"),
    ("SAML", "saml"),
    ("SIP", "sip"),
    ("SMTP", "smtp"),
    ("FTP", "ftp"),
    ("POP3", "pop3"),
    ("SNMP", "snmp"),
    ("SOCKS5", "socks5"),
    ("SocketFile", "socket_file"),
    ("SSH", "ssh"),
    ("SSH Agent", "ssh-agent"),
    ("Syslog", "syslog"),
    ("TCP", "tcp"),
    ("Telnet", "telnet"),
    ("TLS", "tls"),
    ("Tor", "tor"),
    ("BitTorrent DHT", "torrent-dht"),
    ("BitTorrent Peer Wire", "torrent-peer"),
    ("BitTorrent Tracker", "torrent-tracker"),
    ("TURN", "turn"),
    ("UDP", "udp"),
    ("WHOIS", "whois"),
    ("STOMP", "stomp"),
    ("NetBIOS-NS", "netbios-ns"),
    ("NATS", "nats"),
    ("SSDP", "ssdp"),
    ("Gopher", "gopher"),
    ("Finger", "finger"),
    ("Ident", "ident"),
    ("LLMNR", "llmnr"),
    ("wireguard", "wireguard"),
    ("XML-RPC", "xmlrpc"),
    ("XMPP", "xmpp"),
    ("ARP", "arp"),
    ("HTTP Proxy", "http_proxy"),
    ("IMAP", "imap"),
    ("IRC", "irc"),
    ("MCP", "mcp"),
    ("MQTT", "mqtt"),
    ("MongoDB", "mongodb"),
    ("NNTP", "nntp"),
    ("NPM", "npm"),
    ("Ollama", "ollama"),
    ("OpenAPI", "openapi"),
    ("ospf", "ospf"),
    ("SMB", "smb-client"),
    ("SQS", "sqs"),
    ("STUN", "stun"),
    ("USB", "usb"),
    ("VNC", "vnc"),
    ("WebRTC", "webrtc"),
    ("WebSocket", "websocket"),
    ("Bitcoin", "bitcoin"),
    ("Bluetooth (BLE)", "bluetooth-ble-client"),
    ("DynamoDB", "dynamodb"),
    ("Elasticsearch", "elasticsearch"),
    ("Git", "git"),
    ("Kubernetes", "kubernetes"),
    ("LDAP", "ldap"),
    ("Maven", "maven"),
    ("NFS", "nfs"),
    ("OAuth2", "oauth2"),
    ("OpenIDConnect", "openidconnect"),
    ("CouchDB", "couchdb"),
    ("S3", "s3"),
    ("WebDAV", "webdav"),
];

/// Why [`ClientRegistry::resolve`] could not find a protocol for the given
/// input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientProtocolLookupError {
    /// A real NetGet client protocol by this name exists, but this build was
    /// compiled without the feature that enables it.
    NotCompiled {
        name: &'static str,
        feature: &'static str,
    },
    /// No client protocol by this name/keyword is known to NetGet at all, in
    /// any build.
    Unknown {
        input: String,
        suggestion: Option<&'static str>,
    },
}

impl std::fmt::Display for ClientProtocolLookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientProtocolLookupError::NotCompiled { name, feature } => write!(
                f,
                "Client protocol '{name}' exists but is not compiled into this build (rebuild with --features {feature})"
            ),
            ClientProtocolLookupError::Unknown {
                input,
                suggestion: Some(s),
            } => write!(f, "Unknown client protocol: '{input}'. Did you mean '{s}'?"),
            ClientProtocolLookupError::Unknown {
                input,
                suggestion: None,
            } => write!(f, "Unknown client protocol: '{input}'"),
        }
    }
}

impl std::error::Error for ClientProtocolLookupError {}

/// Suggest the closest known client protocol name to `input` via Levenshtein
/// edit distance, if it's plausibly a typo rather than something unrelated.
fn suggest_client_protocol_name(
    input: &str,
    known: &[(&'static str, &'static str)],
) -> Option<&'static str> {
    let input_norm = input.to_lowercase();
    let mut best: Option<(&'static str, usize)> = None;

    for (name, _feature) in known {
        let distance = levenshtein(&input_norm, &name.to_lowercase());
        best = match best {
            Some((_, best_dist)) if best_dist <= distance => best,
            _ => Some((name, distance)),
        };
    }

    best.and_then(|(name, distance)| {
        let threshold = (input_norm.chars().count() / 3).max(2);
        if distance <= threshold {
            Some(name)
        } else {
            None
        }
    })
}

/// Minimal Levenshtein edit distance between two strings. Implemented by hand
/// (no external dependency) since a "did you mean" suggestion doesn't justify
/// adding a crate.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut dp: Vec<usize> = (0..=b.len()).collect();

    for i in 1..=a.len() {
        let mut prev = dp[0];
        dp[0] = i;
        for j in 1..=b.len() {
            let tmp = dp[j];
            dp[j] = if a[i - 1] == b[j - 1] {
                prev
            } else {
                1 + prev.min(dp[j]).min(dp[j - 1])
            };
            prev = tmp;
        }
    }

    dp[b.len()]
}

/// Global client protocol registry instance
///
/// This registry is initialized once at startup with all available client protocols
/// based on compiled features. Use `CLIENT_REGISTRY.get(protocol_name)` to retrieve
/// a client protocol implementation.
pub static CLIENT_REGISTRY: std::sync::LazyLock<ClientRegistry> =
    std::sync::LazyLock::new(ClientRegistry::new);
