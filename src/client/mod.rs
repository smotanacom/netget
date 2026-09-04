//! Client protocol implementations
//!
//! This module contains all client protocol implementations.
//! Each protocol provides LLM-controlled client behavior for connecting
//! to remote servers and exchanging data.

// Shared plumbing: budget-checked LLM entry point used by every client protocol.
// See the module docs for why clients need a hard call ceiling.
pub mod command_support;
pub mod llm_budget;

// arp client
#[cfg(feature = "arp")]
pub mod arp;
#[cfg(feature = "arp")]
pub use arp::actions::ArpClientProtocol;

// bgp client
#[cfg(feature = "bgp")]
pub mod bgp;
#[cfg(feature = "bgp")]
pub use bgp::actions::BgpClientProtocol;

// bitcoin client
#[cfg(feature = "bitcoin")]
pub mod bitcoin;
#[cfg(feature = "bitcoin")]
pub use bitcoin::actions::BitcoinClientProtocol;

// bluetooth-ble client
#[cfg(feature = "bluetooth-ble-client")]
pub mod bluetooth;
#[cfg(feature = "bluetooth-ble-client")]
pub use bluetooth::actions::BluetoothClientProtocol;

// bootp client
#[cfg(feature = "bootp")]
pub mod bootp;
#[cfg(feature = "bootp")]
pub use bootp::actions::BootpClientProtocol;

// cassandra client
#[cfg(feature = "cassandra")]
pub mod cassandra;
#[cfg(feature = "cassandra")]
pub use cassandra::actions::CassandraClientProtocol;

// datalink client
#[cfg(feature = "datalink")]
pub mod datalink;
#[cfg(feature = "datalink")]
pub use datalink::actions::DataLinkClientProtocol;

// dc client
#[cfg(feature = "dc")]
pub mod dc;
#[cfg(feature = "dc")]
pub use dc::actions::DcClientProtocol;

// dhcp client
#[cfg(feature = "dhcp")]
pub mod dhcp;
#[cfg(feature = "dhcp")]
pub use dhcp::actions::DhcpClientProtocol;

// dns client
#[cfg(feature = "dns")]
pub mod dns;
#[cfg(feature = "dns")]
pub use dns::actions::DnsClientProtocol;

// doh client
#[cfg(feature = "doh")]
pub mod doh;
#[cfg(feature = "doh")]
pub use doh::actions::DohClientProtocol;

// dot client
#[cfg(feature = "dot")]
pub mod dot;
#[cfg(feature = "dot")]
pub use dot::actions::DotClientProtocol;

// dynamodb client
#[cfg(feature = "dynamodb")]
pub mod dynamodb;
#[cfg(feature = "dynamodb")]
pub use dynamodb::actions::DynamoDbClientProtocol;

// elasticsearch client
#[cfg(feature = "elasticsearch")]
pub mod elasticsearch;
#[cfg(feature = "elasticsearch")]
pub use elasticsearch::actions::ElasticsearchClientProtocol;

// etcd client
#[cfg(feature = "etcd")]
pub mod etcd;
#[cfg(feature = "etcd")]
pub use etcd::actions::EtcdClientProtocol;

// zookeeper client
#[cfg(feature = "zookeeper")]
pub mod zookeeper;
#[cfg(feature = "zookeeper")]
pub use zookeeper::actions::ZookeeperClientProtocol;

// git client
#[cfg(feature = "git")]
pub mod git;
#[cfg(feature = "git")]
pub use git::actions::GitClientProtocol;

// grpc client
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "grpc")]
pub use grpc::actions::GrpcClientProtocol;

// http client
#[cfg(feature = "http")]
pub mod http;
#[cfg(feature = "http")]
pub use http::actions::HttpClientProtocol;

// http2 client
#[cfg(feature = "http2")]
pub mod http2;
#[cfg(feature = "http2")]
pub use http2::actions::Http2ClientProtocol;

// http3 client
#[cfg(feature = "http3")]
pub mod http3;
#[cfg(feature = "http3")]
pub use http3::actions::Http3ClientProtocol;

// http_proxy client
#[cfg(feature = "http_proxy")]
pub mod http_proxy;
#[cfg(feature = "http_proxy")]
pub use http_proxy::actions::HttpProxyClientProtocol;

// igmp client
#[cfg(feature = "igmp")]
pub mod igmp;
#[cfg(feature = "igmp")]
pub use igmp::actions::IgmpClientProtocol;

// imap client
#[cfg(feature = "imap")]
pub mod imap;
#[cfg(feature = "imap")]
pub use imap::actions::ImapClientProtocol;

// ipp client
#[cfg(feature = "ipp")]
pub mod ipp;
#[cfg(feature = "ipp")]
pub use ipp::actions::IppClientProtocol;

// irc client
#[cfg(feature = "irc")]
pub mod irc;
#[cfg(feature = "irc")]
pub use irc::actions::IrcClientProtocol;

// isis client
#[cfg(feature = "isis")]
pub mod isis;
#[cfg(feature = "isis")]
pub use isis::actions::IsisClientProtocol;

// jsonrpc client
#[cfg(feature = "jsonrpc")]
pub mod jsonrpc;
#[cfg(feature = "jsonrpc")]
pub use jsonrpc::actions::JsonRpcClientProtocol;

// kafka client (pure Rust, shares the broker's kafka-protocol codecs)
#[cfg(feature = "kafka")]
pub mod kafka;
#[cfg(feature = "kafka")]
pub use kafka::actions::KafkaClientProtocol;

// kubernetes client
#[cfg(feature = "kubernetes")]
pub mod kubernetes;
#[cfg(feature = "kubernetes")]
pub use kubernetes::actions::KubernetesClientProtocol;

// ldap client
#[cfg(feature = "ldap")]
pub mod ldap;
#[cfg(feature = "ldap")]
pub use ldap::actions::LdapClientProtocol;

// maven client
#[cfg(feature = "maven")]
pub mod maven;
#[cfg(feature = "maven")]
pub use maven::actions::MavenClientProtocol;

// mcp client
#[cfg(feature = "mcp")]
pub mod mcp;
#[cfg(feature = "mcp")]
pub use mcp::actions::McpClientProtocol;

// mdns client
#[cfg(feature = "mdns")]
pub mod mdns;
#[cfg(feature = "mdns")]
pub use mdns::actions::MdnsClientProtocol;

// mqtt client
#[cfg(feature = "mqtt")]
pub mod mqtt;
#[cfg(feature = "mqtt")]
pub use mqtt::actions::MqttClientProtocol;

// amqp client
#[cfg(feature = "amqp")]
pub mod amqp;
#[cfg(feature = "amqp")]
pub use amqp::actions::AmqpClientProtocol;

// mysql client
#[cfg(feature = "mysql")]
pub mod mysql;
#[cfg(feature = "mysql")]
pub use mysql::actions::MysqlClientProtocol;

// mongodb client
#[cfg(feature = "mongodb")]
pub mod mongodb;
#[cfg(feature = "mongodb")]
pub use mongodb::actions::MongodbClientProtocol;

// nfs client
#[cfg(feature = "nfs")]
pub mod nfs;
#[cfg(feature = "nfs")]
pub use nfs::actions::NfsClientProtocol;

// nfc client
#[cfg(feature = "nfc-client")]
pub mod nfc;
#[cfg(feature = "nfc-client")]
pub use nfc::NfcClientProtocol;

// nntp client
#[cfg(feature = "nntp")]
pub mod nntp;
#[cfg(feature = "nntp")]
pub use nntp::actions::NntpClientProtocol;

// npm client
#[cfg(feature = "npm")]
pub mod npm;
#[cfg(feature = "npm")]
pub use npm::actions::NpmClientProtocol;

// ntp client
#[cfg(feature = "ntp")]
pub mod ntp;
#[cfg(feature = "ntp")]
pub use ntp::actions::NtpClientProtocol;

// oauth2 client
#[cfg(feature = "oauth2")]
pub mod oauth2;
#[cfg(feature = "oauth2")]
pub use oauth2::actions::OAuth2ClientProtocol;

// openai client
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "openai")]
pub use openai::actions::OpenAiClientProtocol;

// ollama client
#[cfg(feature = "ollama")]
pub mod ollama;
#[cfg(feature = "ollama")]
pub use ollama::actions::OllamaClientProtocol;

// openapi client
#[cfg(feature = "openapi")]
pub mod openapi;
#[cfg(feature = "openapi")]
pub use openapi::actions::OpenApiClientProtocol;

// openidconnect client
#[cfg(feature = "openidconnect")]
pub mod openidconnect;
#[cfg(feature = "openidconnect")]
pub use openidconnect::actions::OpenIdConnectClientProtocol;

// ospf client
#[cfg(feature = "ospf")]
pub mod ospf;
#[cfg(feature = "ospf")]
pub use ospf::actions::OspfClientProtocol;

// postgresql client
#[cfg(feature = "postgresql")]
pub mod postgresql;
#[cfg(feature = "postgresql")]
pub use postgresql::actions::PostgresqlClientProtocol;

// pypi client
#[cfg(feature = "pypi")]
pub mod pypi;
#[cfg(feature = "pypi")]
pub use pypi::actions::PypiClientProtocol;

// mssql client
#[cfg(feature = "mssql")]
pub mod mssql;
#[cfg(feature = "mssql")]
pub use mssql::actions::MssqlClientProtocol;

// redis client
#[cfg(feature = "redis")]
pub mod redis;
#[cfg(feature = "redis")]
pub use redis::actions::RedisClientProtocol;

// icmp client
#[cfg(feature = "icmp")]
pub mod icmp;
#[cfg(feature = "icmp")]
pub use icmp::actions::IcmpClientProtocol;

// couchdb client
#[cfg(feature = "couchdb")]
pub mod couchdb;
#[cfg(feature = "couchdb")]
pub use couchdb::actions::CouchDbClientProtocol;

// tftp client
#[cfg(feature = "tftp")]
pub mod tftp;
#[cfg(feature = "tftp")]
pub use tftp::actions::TftpClientProtocol;

// rss client
#[cfg(feature = "rss")]
pub mod rss;
#[cfg(feature = "rss")]
pub use rss::actions::RssClientProtocol;

// rip client
#[cfg(feature = "rip")]
pub mod rip;
#[cfg(feature = "rip")]
pub use rip::actions::RipClientProtocol;

// s3 client
#[cfg(feature = "s3")]
pub mod s3;
#[cfg(feature = "s3")]
pub use s3::actions::S3ClientProtocol;

// saml client
#[cfg(feature = "saml")]
pub mod saml;
#[cfg(feature = "saml")]
pub use saml::actions::SamlClientProtocol;

// sip client
#[cfg(feature = "sip")]
pub mod sip;
#[cfg(feature = "sip")]
pub use sip::actions::SipClientProtocol;

// smb client
#[cfg(feature = "smb-client")]
pub mod smb;
#[cfg(feature = "smb-client")]
pub use smb::SmbClientProtocol;

// smtp client
#[cfg(feature = "smtp")]
pub mod smtp;
#[cfg(feature = "smtp")]
pub use smtp::actions::SmtpClientProtocol;

// ftp client
#[cfg(feature = "ftp")]
pub mod ftp;
#[cfg(feature = "ftp")]
pub use ftp::actions::FtpClientProtocol;

// pop3 client
#[cfg(feature = "pop3")]
pub mod pop3;
#[cfg(feature = "pop3")]
pub use pop3::actions::Pop3ClientProtocol;

// snmp client
#[cfg(feature = "snmp")]
pub mod snmp;
#[cfg(feature = "snmp")]
pub use snmp::actions::SnmpClientProtocol;

// socks5 client
#[cfg(feature = "socks5")]
pub mod socks5;
#[cfg(feature = "socks5")]
pub use socks5::actions::Socks5ClientProtocol;

// socket_file client
#[cfg(all(feature = "socket_file", unix))]
pub mod socket_file;
#[cfg(all(feature = "socket_file", unix))]
pub use socket_file::SocketFileClientProtocol;

// sqs client
#[cfg(feature = "sqs")]
pub mod sqs;
#[cfg(feature = "sqs")]
pub use sqs::actions::SqsClientProtocol;

// ssh client
#[cfg(feature = "ssh")]
pub mod ssh;
#[cfg(feature = "ssh")]
pub use ssh::actions::SshClientProtocol;

// ssh-agent client
#[cfg(all(feature = "ssh-agent", unix))]
pub mod ssh_agent;
#[cfg(all(feature = "ssh-agent", unix))]
pub use ssh_agent::actions::SshAgentClientProtocol;

// stun client
#[cfg(feature = "stun")]
pub mod stun;
#[cfg(feature = "stun")]
pub use stun::actions::StunClientProtocol;

// syslog client
#[cfg(feature = "syslog")]
pub mod syslog;
#[cfg(feature = "syslog")]
pub use syslog::actions::SyslogClientProtocol;

// tcp client
#[cfg(feature = "tcp")]
pub mod tcp;
#[cfg(feature = "tcp")]
pub use tcp::actions::TcpClientProtocol;

// telnet client
#[cfg(feature = "telnet")]
pub mod telnet;
#[cfg(feature = "telnet")]
pub use telnet::actions::TelnetClientProtocol;

// tls client
#[cfg(feature = "tls")]
pub mod tls;
#[cfg(feature = "tls")]
pub use tls::actions::TlsClientProtocol;

// tor client
#[cfg(feature = "tor")]
pub mod tor;
#[cfg(feature = "tor")]
pub use tor::actions::TorClientProtocol;

// torrent_dht client
#[cfg(feature = "torrent-dht")]
pub mod torrent_dht;
#[cfg(feature = "torrent-dht")]
pub use torrent_dht::actions::TorrentDhtClientProtocol;

// torrent_peer client
#[cfg(feature = "torrent-peer")]
pub mod torrent_peer;
#[cfg(feature = "torrent-peer")]
pub use torrent_peer::actions::TorrentPeerClientProtocol;

// torrent_tracker client
#[cfg(feature = "torrent-tracker")]
pub mod torrent_tracker;
#[cfg(feature = "torrent-tracker")]
pub use torrent_tracker::actions::TorrentTrackerClientProtocol;

// turn client
#[cfg(feature = "turn")]
pub mod turn;
#[cfg(feature = "turn")]
pub use turn::actions::TurnClientProtocol;

// udp client
#[cfg(feature = "udp")]
pub mod udp;
#[cfg(feature = "udp")]
pub use udp::actions::UdpClientProtocol;

// usb client
#[cfg(feature = "usb")]
pub mod usb;
#[cfg(feature = "usb")]
pub use usb::actions::UsbClientProtocol;

// vnc client
#[cfg(feature = "vnc")]
pub mod vnc;
#[cfg(feature = "vnc")]
pub use vnc::actions::VncClientProtocol;

// webdav client
#[cfg(feature = "webdav")]
pub mod webdav;
#[cfg(feature = "webdav")]
pub use webdav::actions::WebdavClientProtocol;

// webrtc client
#[cfg(feature = "webrtc")]
pub mod webrtc;
#[cfg(feature = "webrtc")]
pub use webrtc::actions::WebRtcClientProtocol;

// websocket client
#[cfg(feature = "websocket")]
pub mod websocket;
#[cfg(feature = "websocket")]
pub use websocket::actions::WebSocketClientProtocol;

// whois client
#[cfg(feature = "whois")]
pub mod whois;
#[cfg(feature = "whois")]
pub use whois::actions::WhoisClientProtocol;
// netbios_ns client
#[cfg(feature = "netbios-ns")]
pub mod netbios_ns;
#[cfg(feature = "netbios-ns")]
pub use netbios_ns::actions::NetbiosNsClientProtocol;
// nats client
#[cfg(feature = "nats")]
pub mod nats;
#[cfg(feature = "nats")]
pub use nats::actions::NatsClientProtocol;
// ssdp client
#[cfg(feature = "ssdp")]
pub mod ssdp;
#[cfg(feature = "ssdp")]
pub use ssdp::actions::SsdpClientProtocol;
// gopher client
#[cfg(feature = "gopher")]
pub mod gopher;
#[cfg(feature = "gopher")]
pub use gopher::actions::GopherClientProtocol;
// finger client
#[cfg(feature = "finger")]
pub mod finger;
#[cfg(feature = "finger")]
pub use finger::actions::FingerClientProtocol;
// ident client
#[cfg(feature = "ident")]
pub mod ident;
#[cfg(feature = "ident")]
pub use ident::actions::IdentClientProtocol;
// llmnr client
#[cfg(feature = "llmnr")]
pub mod llmnr;
#[cfg(feature = "llmnr")]
pub use llmnr::actions::LlmnrClientProtocol;

// wireguard client
#[cfg(feature = "wireguard")]
pub mod wireguard;
#[cfg(feature = "wireguard")]
pub use wireguard::actions::WireguardClientProtocol;

// xmlrpc client
#[cfg(feature = "xmlrpc")]
pub mod xmlrpc;
#[cfg(feature = "xmlrpc")]
pub use xmlrpc::actions::XmlRpcClientProtocol;

// xmpp client
#[cfg(feature = "xmpp")]
pub mod xmpp;
#[cfg(feature = "xmpp")]
pub use xmpp::actions::XmppClientProtocol;
