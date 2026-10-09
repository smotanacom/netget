//! Independent clients against the inetd services, each failing rather than skipping when
//! absent: Perl's Net::Ping (its udp and stream modes send data to the echo port and check
//! what comes back), Perl's Net::Time (daytime and time, TCP and UDP), rdate (RFC 868, TCP and
//! UDP) and netcat for QOTD and Chargen, whose output the test checks against the RFCs.
use crate::helpers::inetd::{answer, start};
use serde_json::json;
use std::{path::PathBuf, time::Duration};

fn binary(env: &str, name: &str, hint: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .or_else(|| crate::helpers::real_server::find_binary(name))
        .unwrap_or_else(|| panic!("{name} is required for inetd evidence: {hint}, or set {env}"))
}

async fn output(program: PathBuf, args: &[&str], stdin: Option<&[u8]>) -> (bool, String) {
    let mut command = tokio::process::Command::new(&program);
    command
        .args(args)
        .kill_on_drop(true)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    command.stdin(if stdin.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    });
    let mut child = command
        .spawn()
        .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    if let Some(input) = stdin {
        use tokio::io::AsyncWriteExt;
        let mut pipe = child.stdin.take().unwrap();
        pipe.write_all(input).await.unwrap();
    }
    let out = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .expect("client deadline")
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr),
    )
}

fn perl() -> PathBuf {
    binary(
        "NETGET_PERL_BIN",
        "perl",
        "install perl (Net::Ping and Net::Time ship with it)",
    )
}

const PING: &str = "use Net::Ping; my ($proto, $port) = @ARGV; my $p = Net::Ping->new($proto, 5); $p->port_number($port); print $p->ping('127.0.0.1') ? \"up\\n\" : \"down\\n\";";

#[tokio::test]
async fn perl_net_ping_verifies_echo_over_udp_and_tcp() {
    let (state, id, addr) = start(
        "echo",
        answer("echo_request", json!({"type":"echo_reply"})),
        json!({}),
    )
    .await;
    let port = addr.port().to_string();
    for proto in ["udp", "stream"] {
        let (_, out) = output(perl(), &["-e", PING, proto, &port], None).await;
        assert_eq!(
            out.trim(),
            "up",
            "Net::Ping {proto} against a verbatim echo: {out}"
        );
    }
    state.remove_server(id).await;
    // Control: Net::Ping really checks the bytes. An echo that rewrites them is reported down.
    let rewrite = vec![
        json!({"event_pattern":"echo_request","handler":{"type":"static","actions":[{"type":"echo_reply","data":"not what you sent"}]}}),
    ];
    let (state, id, addr) = start("echo", rewrite, json!({})).await;
    let (_, out) = output(perl(), &["-e", PING, "udp", &addr.port().to_string()], None).await;
    assert_eq!(
        out.trim(),
        "down",
        "Net::Ping must reject a wrong echo: {out}"
    );
    state.remove_server(id).await;
}

const NET_TIME: &str = "use Net::Time qw(inet_time inet_daytime); my ($mode, $port, $proto) = @ARGV; my $v = $mode eq 'daytime' ? inet_daytime(\"127.0.0.1:$port\", $proto, 5) : inet_time(\"127.0.0.1:$port\", $proto, 5); print defined $v ? $v : '(none)';";

#[tokio::test]
async fn perl_net_time_and_rdate_read_daytime_and_time() {
    let (dstate, did, daytime) = start(
        "daytime",
        answer(
            "daytime_request",
            json!({"type":"daytime_reply","text":"Saturday, January 1, 2000 12:00:00-UTC"}),
        ),
        json!({}),
    )
    .await;
    let (tstate, tid, time) = start(
        "time",
        answer(
            "time_request",
            json!({"type":"time_reply","unix_seconds":946728000}),
        ),
        json!({}),
    )
    .await;
    for proto in ["tcp", "udp"] {
        let (_, day) = output(
            perl(),
            &[
                "-e",
                NET_TIME,
                "daytime",
                &daytime.port().to_string(),
                proto,
            ],
            None,
        )
        .await;
        assert!(
            day.starts_with("Saturday, January 1, 2000 12:00:00-UTC"),
            "Net::Time daytime over {proto}: {day:?}"
        );
        let (_, t) = output(
            perl(),
            &["-e", NET_TIME, "time", &time.port().to_string(), proto],
            None,
        )
        .await;
        assert_eq!(t, "946728000", "Net::Time time over {proto}: {t:?}");
    }
    let rdate = binary("NETGET_RDATE_BIN", "rdate", "install rdate");
    for udp in [false, true] {
        let port = time.port().to_string();
        let mut args = vec!["-p", "-o", &port];
        if udp {
            args.push("-u");
        }
        args.push("127.0.0.1");
        let (ok, out) = output(rdate.clone(), &args, None).await;
        assert!(
            ok && out.contains("Sat Jan  1 12:00:00") && out.contains("2000"),
            "rdate (udp={udp}): {out}"
        );
    }
    dstate.remove_server(did).await;
    tstate.remove_server(tid).await;
}

#[tokio::test]
async fn netcat_reads_qotd_and_a_conforming_chargen_stream() {
    let nc = binary("NETGET_NC_BIN", "nc", "install netcat-openbsd");
    let (state, id, addr) = start(
        "qotd",
        answer(
            "qotd_request",
            json!({"type":"qotd_reply","quote":"Premature optimization is the root of all evil."}),
        ),
        json!({}),
    )
    .await;
    let port = addr.port().to_string();
    let (_, tcp) = output(nc.clone(), &["-N", "127.0.0.1", &port], Some(b"")).await;
    assert_eq!(tcp, "Premature optimization is the root of all evil.\r\n");
    let (_, udp) = output(
        nc.clone(),
        &["-u", "-w", "2", "127.0.0.1", &port],
        Some(b"\n"),
    )
    .await;
    assert_eq!(udp, "Premature optimization is the root of all evil.\r\n");
    state.remove_server(id).await;
    let (state, id, addr) = start(
        "chargen",
        answer(
            "chargen_request",
            json!({"type":"chargen_reply","max_bytes":740}),
        ),
        json!({}),
    )
    .await;
    let (_, stream) = output(
        nc,
        &["-N", "127.0.0.1", &addr.port().to_string()],
        Some(b""),
    )
    .await;
    assert_eq!(stream.len(), 740);
    assert!(
        netget::server::inetd::wire::conforms_to_chargen(
            &stream,
            &netget::server::inetd::wire::default_charset()
        ),
        "{stream}"
    );
    state.remove_server(id).await;
}
