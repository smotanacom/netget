//! NetGet's RadSec client against independent RadSec servers, failing rather than skipping when
//! absent:
//!
//! * **FreeRADIUS** 3 with a TLS listener (`proto = tcp`, a `tls` section requiring a client
//!   certificate), authenticating from its own `users` file;
//! * **radsecproxy** terminating TLS and forwarding over UDP to a FreeRADIUS behind it.
//!
//! The chain names the accounting session after the Reply-Message FreeRADIUS sent, and the
//! session id is then read from FreeRADIUS's own `detail` file — the client acted on the
//! server's answer, asserted from the server's side of the wire.
use super::session_test::{chain, client, client_params, wait_for};
use crate::helpers::radsec_pki::{make, Pki};
use crate::helpers::real_server::{InstallHint, RealServer};
use serde_json::json;
use std::{path::Path, time::Duration};

const FREERADIUS: InstallHint = InstallHint {
    brew: "freeradius-server",
    apt: "freeradius (and link /usr/sbin/freeradius to radiusd, as CI does)",
};
const RADSECPROXY: InstallHint = InstallHint {
    brew: "radsecproxy",
    apt: "radsecproxy",
};

fn freeradius_layout() -> (String, String) {
    let prefix = ["/opt/homebrew", "/usr/local", "/usr"]
        .into_iter()
        .find(|p| Path::new(p).join("share/freeradius/dictionary").is_file())
        .expect(
            "no FreeRADIUS dictionary under /opt/homebrew, /usr/local or /usr share/freeradius",
        );
    let libdir = [
        "/opt/homebrew/lib",
        "/usr/local/lib",
        "/usr/lib/freeradius",
        "/usr/lib/x86_64-linux-gnu/freeradius",
        "/usr/lib/aarch64-linux-gnu/freeradius",
    ]
    .into_iter()
    .find(|d| {
        ["rlm_pap.so", "rlm_pap.dylib"]
            .iter()
            .any(|m| Path::new(d).join(m).is_file())
    })
    .expect("no FreeRADIUS module directory holding rlm_pap");
    (prefix.to_string(), libdir.to_string())
}

const USERS: &str = "alice Cleartext-Password := \"wonderland\"\n\tReply-Message := \"hello from freeradius\"\n\nmallory Auth-Type := Reject\n\tReply-Message := \"not you\"\n";

/// A FreeRADIUS raddb whose `listen` blocks are given.
fn radiusd_conf(listeners: &str, clients: &str) -> String {
    let (prefix, libdir) = freeradius_layout();
    format!(
        r#"prefix = {prefix}
exec_prefix = ${{prefix}}
sysconfdir = {{dir}}
localstatedir = {{dir}}
sbindir = ${{exec_prefix}}/sbin
logdir = {{dir}}
raddbdir = {{dir}}
radacctdir = {{dir}}
name = radiusd
confdir = ${{raddbdir}}
run_dir = {{dir}}
db_dir = {{dir}}
libdir = {libdir}
pidfile = {{dir}}/radiusd.pid
max_request_time = 30
cleanup_delay = 5
max_requests = 1024
hostname_lookups = no
log {{
  destination = stdout
  colourise = no
  auth = yes
}}
security {{
  allow_core_dumps = no
  max_attributes = 200
  reject_delay = 0
}}
thread pool {{
  start_servers = 1
  max_servers = 4
  min_spare_servers = 1
  max_spare_servers = 4
  max_requests_per_server = 0
}}
{clients}
modules {{
  pap {{
  }}
  files {{
    filename = {{dir}}/users
  }}
  detail {{
    filename = {{dir}}/detail
    permissions = 0600
    header = "%t"
  }}
}}
server default {{
{listeners}
  authorize {{
    files
    pap
  }}
  authenticate {{
    pap
  }}
  accounting {{
    detail
  }}
}}
"#
    )
}

/// Run the chain against a RadSec server at `addr` and check what FreeRADIUS saw.
async fn drive(addr: String, pki: &Pki, freeradius: &RealServer) {
    let (state, id) = client(addr, client_params(pki), chain())
        .await
        .expect("connect");
    let accept = wait_for(&state, id, "radius_access_accept").await;
    assert_eq!(
        accept["reply_message"],
        "hello from freeradius",
        "{accept}\n{}",
        freeradius.log()
    );
    let acct = wait_for(&state, id, "radius_accounting_response").await;
    assert_eq!(acct["session_id"], "s-hello-from-freeradius", "{acct}");
    let detail = std::fs::read_to_string(freeradius.dir().join("detail"))
        .expect("FreeRADIUS wrote no detail file");
    assert!(
        detail.contains("Acct-Session-Id = \"s-hello-from-freeradius\""),
        "{detail}"
    );
    assert!(detail.contains("User-Name = \"alice\""), "{detail}");
    state
        .send_to_client(
            id,
            json!({"type":"radius_access_request","user_name":"mallory","password":"x"}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let reject = wait_for(&state, id, "radius_access_reject").await;
    assert_eq!(reject["reply_message"], "not you", "{reject}");
}

#[tokio::test]
async fn netget_against_freeradius_tls_listener() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let listeners = format!(
        r#"  listen {{
    type = auth+acct
    ipaddr = 127.0.0.1
    port = {{port}}
    proto = tcp
    clients = radsec
    tls {{
      private_key_file = {key}
      certificate_file = {cert}
      ca_file = {ca}
      require_client_cert = yes
      fragment_size = 8192
    }}
  }}"#,
        key = pki.server_key.display(),
        cert = pki.server_cert.display(),
        ca = pki.ca.display()
    );
    let clients = "clients radsec {\n  client localhost {\n    ipaddr = 127.0.0.1\n    proto = tls\n    secret = radsec\n  }\n}";
    let freeradius = RealServer::builder("radiusd", FREERADIUS)
        .config_file("radiusd.conf", &radiusd_conf(&listeners, clients))
        .config_file("users", USERS)
        // Not -X: it turns threading off, and FreeRADIUS refuses TLS sockets without it.
        .args(["-f", "-xx", "-l", "stdout", "-d", "{dir}"])
        .ready_when_log_matches("Ready to process requests")
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
        .expect("start FreeRADIUS");
    drive(freeradius.addr(), &pki, &freeradius).await;
}

#[tokio::test]
async fn netget_against_radsecproxy() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let listeners = "  listen {\n    type = auth\n    ipaddr = 127.0.0.1\n    port = {port}\n  }\n  listen {\n    type = acct\n    ipaddr = 127.0.0.1\n    port = {port1}\n  }";
    let clients = "client localhost {\n  ipaddr = 127.0.0.1\n  secret = testing123\n}";
    let freeradius = RealServer::builder("radiusd", FREERADIUS)
        .config_file("radiusd.conf", &radiusd_conf(listeners, clients))
        .config_file("users", USERS)
        .args(["-f", "-X", "-d", "{dir}"])
        .extra_ports(1)
        .without_tcp_readiness()
        .ready_when_log_matches("Ready to process requests")
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
        .expect("start FreeRADIUS");
    let auth_port: u16 = freeradius
        .addr()
        .rsplit_once(':')
        .unwrap()
        .1
        .parse()
        .unwrap();
    let conf = format!(
        "ListenTLS 127.0.0.1:{{port}}
LogLevel 4
tls default {{
    CACertificateFile {ca}
    CertificateFile {cert}
    CertificateKeyFile {key}
}}
client netget {{
    host 127.0.0.1
    type tls
    secret radsec
    CertificateNameCheck off
    tls default
}}
server fr-auth {{
    host 127.0.0.1
    port {auth}
    type udp
    secret testing123
}}
server fr-acct {{
    host 127.0.0.1
    port {acct}
    type udp
    secret testing123
}}
realm * {{
    server fr-auth
    accountingServer fr-acct
}}
",
        ca = pki.ca.display(),
        cert = pki.server_cert.display(),
        key = pki.server_key.display(),
        auth = auth_port,
        acct = freeradius.extra_ports[0],
    );
    let proxy = RealServer::builder("radsecproxy", RADSECPROXY)
        .config_file("radsecproxy.conf", &conf)
        .args(["-f", "-d", "4", "-c", "{dir}/radsecproxy.conf"])
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
        .expect("start radsecproxy");
    drive(proxy.addr(), &pki, &freeradius).await;
}
