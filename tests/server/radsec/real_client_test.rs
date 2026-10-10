//! Independent RadSec clients against NetGet's RadSec server, failing rather than skipping when
//! absent. FreeRADIUS's `radclient` (UDP only) sends each request to a proxy that carries it to
//! NetGet over mutual TLS:
//!
//! * **radsecproxy** (C), with a client certificate from the test CA;
//! * **FreeRADIUS** as a proxy, NetGet configured as its TLS home server.
//!
//! Each proxy verifies NetGet's Response Authenticator and Message-Authenticator with the
//! RadSec secret before re-signing the reply for radclient with its own, so an Access-Accept
//! reaching radclient is two independent implementations agreeing with NetGet's signatures.
use super::wire_test::{policy, start};
use crate::helpers::radsec_pki::make;
use crate::helpers::real_server::{InstallHint, RealServer};
use std::{path::Path, time::Duration};

const RADSECPROXY: InstallHint = InstallHint {
    brew: "radsecproxy",
    apt: "radsecproxy",
};
const FREERADIUS: InstallHint = InstallHint {
    brew: "freeradius-server",
    apt: "freeradius (and link /usr/sbin/freeradius to radiusd, as CI does)",
};

/// Run radclient against a proxy's UDP port; its full output.
async fn radclient(port: u16, user: &str, password: &str) -> String {
    use tokio::io::AsyncWriteExt;
    let bin = crate::helpers::real_server::find_binary("radclient")
        .unwrap_or_else(|| panic!("radclient is required: apt-get install freeradius-utils (brew install freeradius-server)"));
    let mut child = tokio::process::Command::new(bin)
        .args([
            "-x",
            "-r",
            "3",
            "-t",
            "5",
            &format!("127.0.0.1:{port}"),
            "auth",
            "testing123",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin
        .write_all(format!("User-Name = \"{user}\"\nUser-Password = \"{password}\"\n").as_bytes())
        .await
        .unwrap();
    drop(stdin);
    let out = tokio::time::timeout(Duration::from_secs(60), child.wait_with_output())
        .await
        .expect("radclient deadline")
        .unwrap();
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn assert_decisions(accepted: &str, rejected: &str, log: &str) {
    assert!(
        accepted.contains("Received Access-Accept"),
        "{accepted}\n--- proxy log ---\n{log}"
    );
    assert!(accepted.contains("welcome alice"), "{accepted}");
    assert!(
        rejected.contains("Received Access-Reject"),
        "{rejected}\n--- proxy log ---\n{log}"
    );
    assert!(rejected.contains("go away"), "{rejected}");
}

#[tokio::test]
async fn radclient_through_radsecproxy() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let (_state, _id, netget) = start(&pki, policy(), true).await;
    let conf = format!(
        "ListenUDP 127.0.0.1:{{port}}
LogLevel 4
tls default {{
    CACertificateFile {ca}
    CertificateFile {cert}
    CertificateKeyFile {key}
}}
client udpclient {{
    host 127.0.0.1
    type udp
    secret testing123
}}
server netget {{
    host 127.0.0.1
    port {port}
    type tls
    secret radsec
    tls default
}}
realm * {{
    server netget
}}
",
        ca = pki.ca.display(),
        cert = pki.client_cert.display(),
        key = pki.client_key.display(),
        port = netget.port()
    );
    let proxy = RealServer::builder("radsecproxy", RADSECPROXY)
        .config_file("radsecproxy.conf", &conf)
        .args(["-f", "-d", "4", "-c", "{dir}/radsecproxy.conf"])
        .without_tcp_readiness()
        .ready_when_log_matches("radsecproxy .* starting")
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
        .expect("start radsecproxy");
    let port: u16 = proxy.addr().rsplit_once(':').unwrap().1.parse().unwrap();
    let accepted = radclient(port, "alice", "wonderland").await;
    let rejected = radclient(port, "mallory", "guess").await;
    assert_decisions(&accepted, &rejected, &proxy.log());
}

/// Where FreeRADIUS keeps its dictionaries and modules: Homebrew or a distribution layout.
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

#[tokio::test]
async fn radclient_through_freeradius_proxy() {
    let dir = tempfile::tempdir().unwrap();
    let pki = make(dir.path());
    let (_state, _id, netget) = start(&pki, policy(), true).await;
    let (prefix, libdir) = freeradius_layout();
    let conf = format!(
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
proxy_requests = yes
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
client localhost {{
  ipaddr = 127.0.0.1
  secret = testing123
}}
proxy server {{
  default_fallback = no
}}
home_server netget {{
  type = auth
  ipaddr = 127.0.0.1
  port = {port}
  proto = tcp
  secret = radsec
  status_check = none
  tls {{
    private_key_file = {key}
    certificate_file = {cert}
    ca_file = {ca}
    fragment_size = 8192
    check_cert_cn = "localhost"
  }}
}}
home_server_pool netget_pool {{
  type = fail-over
  home_server = netget
}}
realm netget {{
  auth_pool = netget_pool
  nostrip
}}
modules {{
}}
server default {{
  listen {{
    type = auth
    ipaddr = 127.0.0.1
    port = {{port}}
  }}
  authorize {{
    update control {{
      &Proxy-To-Realm := "netget"
    }}
  }}
  authenticate {{
  }}
}}
"#,
        port = netget.port(),
        ca = pki.ca.display(),
        cert = pki.client_cert.display(),
        key = pki.client_key.display(),
    );
    let proxy = RealServer::builder("radiusd", FREERADIUS)
        .config_file("radiusd.conf", &conf)
        .args(["-f", "-X", "-d", "{dir}"])
        .without_tcp_readiness()
        .ready_when_log_matches("Ready to process requests")
        .startup_timeout(Duration::from_secs(30))
        .start()
        .await
        .expect("start FreeRADIUS");
    let port: u16 = proxy.addr().rsplit_once(':').unwrap().1.parse().unwrap();
    let accepted = radclient(port, "alice", "wonderland").await;
    let rejected = radclient(port, "mallory", "guess").await;
    assert_decisions(&accepted, &rejected, &proxy.log());
}
