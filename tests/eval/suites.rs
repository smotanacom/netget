//! The canonical operator instructions, per protocol.
//!
//! # The one rule these must obey
//!
//! **An instruction may never name an action, a parameter or an event.** It is
//! written the way an operator types it into the dashboard — "serve a page that
//! says hello", "answer example.com with 1.2.3.4" — and the model has to find
//! the vocabulary for itself, out of the protocol's own action descriptions and
//! parameter docs. The moment an instruction says `send_http_response`, the
//! thing under test stops being the descriptions and becomes the model's
//! copy-paste.
//!
//! The corollary is that these instructions are *allowed* to be under-specified
//! in exactly the way a real one is. Where the model guesses wrong, that is the
//! finding.
//!
//! Each case is feature-gated, so a build compiles only the cases whose protocol
//! it contains and `run-eval.sh` chooses the feature set.

#![allow(dead_code)]

use super::case::{EvalCase, Expect, Probe};

/// Every case this build knows about, in protocol order.
pub fn all_cases() -> Vec<EvalCase> {
    let mut cases = Vec::new();
    #[cfg(feature = "http")]
    cases.extend(http());
    #[cfg(feature = "dns")]
    cases.extend(dns());
    #[cfg(feature = "whois")]
    cases.extend(whois());
    #[cfg(feature = "gopher")]
    cases.extend(gopher());
    #[cfg(feature = "finger")]
    cases.extend(finger());
    #[cfg(feature = "redis")]
    cases.extend(redis());
    #[cfg(feature = "postgresql")]
    cases.extend(postgresql());
    #[cfg(feature = "mysql")]
    cases.extend(mysql());
    #[cfg(feature = "ldap")]
    cases.extend(ldap());
    #[cfg(feature = "ipp")]
    cases.extend(ipp());
    #[cfg(feature = "syslog")]
    cases.extend(syslog());
    #[cfg(feature = "ntp")]
    cases.extend(ntp());
    #[cfg(feature = "telnet")]
    cases.extend(telnet());
    #[cfg(feature = "tcp")]
    cases.extend(tcp());
    #[cfg(feature = "ftp")]
    cases.extend(ftp());
    #[cfg(feature = "udp")]
    cases.extend(udp());
    cases
}

// ---------------------------------------------------------------------------
// HTTP — curl. The reference case: an independent client, an unambiguous
// status line, and four instructions an operator would plausibly type.
// ---------------------------------------------------------------------------

#[cfg(feature = "http")]
fn curl(path: &str) -> Probe {
    let url = format!("http://127.0.0.1:{{PORT}}{}", path);
    Probe::client("curl", &["-sS", "-i", "--max-time", "230", url.as_str()])
}

#[cfg(feature = "http")]
fn http() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "http/hello-page",
            "http",
            "Serve a page that says hello.",
            curl("/"),
            Expect::contains(&["HTTP/1.1 200", "hello"]),
        ),
        EvalCase::new(
            "http/admin-404",
            "http",
            "Return 404 Not Found for anything under /admin. Serve every other path \
             with a short ordinary page.",
            curl("/admin/secrets"),
            Expect::contains(&["HTTP/1.1 404"]),
        ),
        EvalCase::new(
            "http/json-status",
            "http",
            "Answer /status with the JSON body {\"status\":\"ok\"} and say it is JSON.",
            curl("/status"),
            Expect::contains(&["application/json", "\"status\"", "ok"]),
        ),
        EvalCase::new(
            "http/permanent-redirect",
            "http",
            "Redirect / permanently to https://example.com/.",
            curl("/"),
            Expect::contains(&["HTTP/1.1 301", "example.com"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// DNS — dig. The strongest evidence in the suite: dig parses the whole message
// and will not print an answer it could not decode, so a pass here means the
// model built a wire-correct packet with the right transaction id.
// ---------------------------------------------------------------------------

#[cfg(feature = "dns")]
fn dig(name: &str, rtype: &str) -> Probe {
    Probe::client(
        "dig",
        &[
            "@127.0.0.1",
            "-p",
            "{PORT}",
            "+time=230",
            "+tries=1",
            name,
            rtype,
        ],
    )
}

#[cfg(feature = "dns")]
fn dns() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "dns/a-record",
            "dns",
            "Answer example.com with 1.2.3.4.",
            dig("example.com", "A"),
            Expect::contains(&["1.2.3.4"]),
        ),
        EvalCase::new(
            "dns/wildcard-a",
            "dns",
            "Whatever name is asked for, answer 10.0.0.1.",
            dig("anything.test", "A"),
            Expect::contains(&["10.0.0.1"]),
        ),
        EvalCase::new(
            "dns/txt-record",
            "dns",
            "Answer text queries for hello.test with the text netget-eval-ok.",
            dig("hello.test", "TXT"),
            Expect::contains(&["netget-eval-ok"]),
        ),
        EvalCase::new(
            "dns/nxdomain",
            "dns",
            "Say the name does not exist for anything under blocked.test. Answer \
             everything else with 127.0.0.1.",
            dig("evil.blocked.test", "A"),
            Expect::contains(&["NXDOMAIN"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// WHOIS — `nc`, not the system `whois`, and the reason is worth recording
// because `whois` is counted among the "67 of 100 real clients already
// installed" in PROTOCOL_QUALITY.md.
//
// **macOS `whois` segfaults when `-p` follows `-h`.** Measured against a bare
// listener: `whois -h 127.0.0.1 -p N netget.example` exits 139 (SIGSEGV) having
// sent zero bytes, while `whois -p N -h 127.0.0.1 …` exits 71 and also sends
// nothing, and `-p` without `-h` goes looking for the real registry. There is no
// argument order that reaches a loopback port, so this client cannot evaluate
// this protocol on this machine at all — the three whois cases in the first
// smoke run all reported `event_never_reached_model` because the client died
// 1.2ms after connecting.
//
// WHOIS is one line of text in and free text out, so `nc` loses little here
// beyond the label, which is why these rows say `generic-transport`.
// ---------------------------------------------------------------------------

#[cfg(feature = "whois")]
fn whois_probe(query: &str) -> Probe {
    let line = format!("{}\r\n", query);
    Probe::generic("nc", &["-w", "235", "127.0.0.1", "{PORT}"]).stdin(line.as_str())
}

#[cfg(feature = "whois")]
fn whois() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "whois/registrar-line",
            "whois",
            "Answer every query with a whois record whose registrar is \
             NETGET-EVAL-REGISTRAR.",
            whois_probe("netget.example"),
            Expect::contains(&["NETGET-EVAL-REGISTRAR"]),
        )
        .note("macOS whois segfaults with -p; driven with nc."),
        EvalCase::new(
            "whois/registrant-and-status",
            "whois",
            "For netget.example, report the registrant organisation as Example \
             Holdings Ltd and the domain status as clientTransferProhibited.",
            whois_probe("netget.example"),
            Expect::contains(&["Example Holdings", "clientTransferProhibited"]),
        ),
        EvalCase::new(
            "whois/no-match",
            "whois",
            "Report that no match was found for any domain ending in .invalid. \
             Answer anything else with an ordinary record.",
            whois_probe("nothing.invalid"),
            Expect::default().matching(r"(?i)no match|not found|no entries|no data found"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Gopher — curl, which speaks gopher natively (see `curl --version`).
// ---------------------------------------------------------------------------

#[cfg(feature = "gopher")]
fn gopher_probe(selector: &str) -> Probe {
    let url = format!("gopher://127.0.0.1:{{PORT}}{}", selector);
    Probe::client("curl", &["-sS", "--max-time", "230", url.as_str()])
}

#[cfg(feature = "gopher")]
fn gopher() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "gopher/welcome-menu",
            "gopher",
            "Serve a menu whose first item is labelled Welcome to NetGet.",
            gopher_probe("/"),
            Expect::contains(&["Welcome to NetGet"]),
        ),
        EvalCase::new(
            "gopher/text-selector",
            "gopher",
            "When the selector /about is asked for, return the text \
             NetGet eval gopher server.",
            gopher_probe("/0/about"),
            Expect::contains(&["NetGet eval gopher server"]),
        ),
        EvalCase::new(
            "gopher/unknown-selector",
            "gopher",
            "Serve a menu at the root. For any other selector, say it was not found.",
            gopher_probe("/1/nowhere"),
            Expect::default().matching(r"(?i)not found|no such|does not exist|error"),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Finger — no installed finger client accepts a port (BSD `finger` hard-wires
// 79, which needs root to bind). Driven with `nc`, and labelled
// `generic-transport` so the weaker evidence is visible in the table.
// ---------------------------------------------------------------------------

#[cfg(feature = "finger")]
fn finger_probe(query: &str) -> Probe {
    Probe::generic("nc", &["-w", "235", "127.0.0.1", "{PORT}"]).stdin(query)
}

#[cfg(feature = "finger")]
fn finger() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "finger/user-record",
            "finger",
            "When someone asks about the user alice, report her real name as \
             Alice Liddell and that she is logged in.",
            finger_probe("alice\r\n"),
            Expect::contains(&["Alice Liddell"]),
        )
        .note("BSD finger cannot target a non-default port; driven with nc."),
        EvalCase::new(
            "finger/unknown-user",
            "finger",
            "Only the user alice exists. Say so for anyone else.",
            finger_probe("bob\r\n"),
            Expect::default().matching(r"(?i)no such user|not found|does not exist|no one|unknown"),
        ),
        EvalCase::new(
            "finger/user-list",
            "finger",
            "When no user is named, list the two users alice and bob.",
            finger_probe("\r\n"),
            Expect::contains(&["alice", "bob"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Redis — redis-cli, the reference client, which decodes RESP for itself.
// ---------------------------------------------------------------------------

#[cfg(feature = "redis")]
fn redis_cli(args: &[&str]) -> Probe {
    let mut full = vec!["-h", "127.0.0.1", "-p", "{PORT}", "-t", "230"];
    full.extend_from_slice(args);
    Probe::client("redis-cli", &full)
}

#[cfg(feature = "redis")]
fn redis() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "redis/get-string",
            "redis",
            "Answer a read of the key greeting with the text hello-netget.",
            redis_cli(&["GET", "greeting"]),
            Expect::contains(&["hello-netget"]),
        ),
        EvalCase::new(
            "redis/ping",
            "redis",
            "Answer a ping with PONG.",
            redis_cli(&["PING"]),
            Expect::contains(&["PONG"]),
        ),
        EvalCase::new(
            "redis/key-list",
            "redis",
            "When asked to list all keys, report exactly three: alpha, beta and gamma.",
            redis_cli(&["KEYS", "*"]),
            Expect::contains(&["alpha", "beta", "gamma"]),
        ),
        EvalCase::new(
            "redis/counter",
            "redis",
            "When the key counter is incremented, report the new value as 42.",
            redis_cli(&["INCR", "counter"]),
            Expect::contains(&["42"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// PostgreSQL — psql. Note this exercises far more than one answer: the model
// has to get through the startup packet and authentication before any query
// arrives, so a failure here may be at the handshake rather than at the query.
// ---------------------------------------------------------------------------

#[cfg(feature = "postgresql")]
fn psql(sql: &str) -> Probe {
    Probe::client(
        "psql",
        &[
            "-h",
            "127.0.0.1",
            "-p",
            "{PORT}",
            "-U",
            "evaluser",
            "-d",
            "evaldb",
            "-w",
            "-t",
            "-A",
            "-c",
            sql,
        ],
    )
    .env("PGCONNECT_TIMEOUT", "230")
    .env("PGSSLMODE", "disable")
}

#[cfg(feature = "postgresql")]
fn postgresql() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "postgresql/select-literal",
            "postgresql",
            "Let anyone log in without a password, and answer the query SELECT 1 with \
             one row holding the number 1.",
            psql("SELECT 1"),
            Expect::contains(&["1"]),
        ),
        EvalCase::new(
            "postgresql/current-user",
            "postgresql",
            "Let anyone log in without a password. Report the current user as \
             netget_eval.",
            psql("SELECT current_user"),
            Expect::contains(&["netget_eval"]),
        ),
        EvalCase::new(
            "postgresql/users-table",
            "postgresql",
            "Let anyone log in without a password. There is a table called users with \
             a name column holding alice, bob and carol.",
            psql("SELECT name FROM users"),
            Expect::contains(&["alice", "bob", "carol"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// MySQL — the **8.0** client, by absolute path, and the reason is a finding the
// first sweep produced from the wire.
//
// NetGet's MySQL server offers `mysql_native_password`, which the 9.x client no
// longer ships. All nine runs against Homebrew's `mysql` 9.3.0 died before a
// query existed:
//
//     ERROR 2059 (HY000): Authentication plugin 'mysql_native_password'
//     cannot be loaded: dlopen(…/mysql/9.3.0/lib/plugin/mysql_native_password.so)
//
// — recorded as `event_never_reached_model`, which is exactly right: the model
// was never asked. `src/server/mysql/CLAUDE.md` already says to use an 8.0
// client, so this is confirmation rather than news; what it *does* show is that
// MySQL's Beta rating rests on `mysql_async`, which still supports the old
// plugin and is therefore more permissive than the shipping client. That is the
// "one client can agree with one bug" case PROTOCOL_QUALITY Tier 1 names.
//
// An absolute path rather than `mysql` on PATH: whichever version is linked is
// not something an eval should be at the mercy of, and when this path is absent
// the case is recorded `client-missing` rather than silently passing.
// ---------------------------------------------------------------------------

#[cfg(feature = "mysql")]
const MYSQL_8_CLIENT: &str = "/opt/homebrew/opt/mysql@8.0/bin/mysql";

#[cfg(feature = "mysql")]
fn mysql_cli(sql: &str) -> Probe {
    Probe::client(
        MYSQL_8_CLIENT,
        &[
            "-h",
            "127.0.0.1",
            "-P",
            "{PORT}",
            "-u",
            "evaluser",
            "--protocol=TCP",
            "--connect-timeout=230",
            "-N",
            "-B",
            "-e",
            sql,
        ],
    )
}

#[cfg(feature = "mysql")]
fn mysql() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "mysql/select-literal",
            "mysql",
            "Let anyone log in without a password, and answer SELECT 1 with one row \
             holding the number 1.",
            mysql_cli("SELECT 1"),
            Expect::contains(&["1"]),
        ),
        EvalCase::new(
            "mysql/server-version",
            "mysql",
            "Let anyone log in without a password. Report the server version as \
             8.0.36-netget-eval.",
            mysql_cli("SELECT VERSION()"),
            Expect::contains(&["8.0.36-netget-eval"]),
        ),
        EvalCase::new(
            "mysql/users-table",
            "mysql",
            "Let anyone log in without a password. There is a table called users with \
             a name column holding alice, bob and carol.",
            mysql_cli("SELECT name FROM users"),
            Expect::contains(&["alice", "bob", "carol"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// LDAP — ldapsearch, OpenLDAP's own client.
// ---------------------------------------------------------------------------

#[cfg(feature = "ldap")]
fn ldapsearch(base: &str, filter: &str) -> Probe {
    // Bind, then search, one model call each. A bind answered with a
    // diagnostic message makes ldapsearch print `ldap_bind: Success (0)` and
    // carry on — so it talks between the two calls, and the idle settle used to
    // kill it there, before the search was ever sent.
    Probe::client(
        "ldapsearch",
        &[
            "-x",
            "-H",
            "ldap://127.0.0.1:{PORT}",
            "-b",
            base,
            "-s",
            "sub",
            "-o",
            "nettimeout=230",
            "-l",
            "230",
            filter,
        ],
    )
    .until_exit()
}

#[cfg(feature = "ldap")]
fn ldap() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "ldap/single-person",
            "ldap",
            "Accept anonymous connections. A search under dc=example,dc=com finds one \
             person, Alice Liddell, whose mail address is alice@example.com.",
            ldapsearch("dc=example,dc=com", "(objectClass=*)"),
            Expect::contains(&["Alice Liddell", "alice@example.com"]),
        ),
        EvalCase::new(
            "ldap/two-people",
            "ldap",
            "Accept anonymous connections. Under ou=people,dc=example,dc=com there are \
             two users, alice and bob.",
            ldapsearch("ou=people,dc=example,dc=com", "(objectClass=*)"),
            Expect::contains(&["alice", "bob"]),
        ),
        EvalCase::new(
            "ldap/empty-result",
            "ldap",
            "Accept anonymous connections. There is nothing at all under \
             dc=other,dc=com — searches there succeed and find no one.",
            ldapsearch("dc=other,dc=com", "(objectClass=*)"),
            Expect::default()
                .matching(r"(?i)result:\s*0\s+success")
                .not_containing(&["Alice", "cn="]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// IPP — ipptool, shipped with CUPS, with CUPS's own Get-Printer-Attributes
// test file. It validates the IPP encoding itself, so a pass is strong.
// ---------------------------------------------------------------------------

#[cfg(feature = "ipp")]
const IPPTOOL_GET_ATTRS: &str = "/usr/share/cups/ipptool/get-printer-attributes.test";

#[cfg(feature = "ipp")]
fn ipptool() -> Probe {
    // `-v` echoes the request before it is sent and the answer only once it
    // arrives, with the whole model call in between — so this client is done
    // when it exits, never when it goes quiet.
    Probe::client(
        "ipptool",
        &[
            "-tv",
            "-T",
            "230",
            "ipp://127.0.0.1:{PORT}/printers/eval",
            IPPTOOL_GET_ATTRS,
        ],
    )
    .until_exit()
}

#[cfg(feature = "ipp")]
fn ipp() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "ipp/printer-name",
            "ipp",
            "There is one printer called NetGet-Eval-Printer. It is idle and accepting \
             jobs.",
            ipptool(),
            Expect::contains(&["NetGet-Eval-Printer"]),
        ),
        EvalCase::new(
            "ipp/printer-stopped",
            "ipp",
            "The printer is called Eval-Stopped-Printer, it is stopped, and it is not \
             accepting jobs.",
            ipptool(),
            Expect::contains(&["Eval-Stopped-Printer"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// Syslog — one-way, so the observable is netget's own log rather than the wire.
// macOS `logger` is BSD and has no -n/-P, so it cannot target a remote port;
// `nc -u` carries the datagram instead.
// ---------------------------------------------------------------------------

#[cfg(feature = "syslog")]
fn syslog_probe(message: &str) -> Probe {
    Probe::generic("nc", &["-u", "-w", "1", "127.0.0.1", "{PORT}"]).stdin(message)
}

#[cfg(feature = "syslog")]
fn syslog() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "syslog/store-message",
            "syslog",
            "Keep every message that arrives.",
            syslog_probe("<14>Oct 11 22:14:15 evalhost netget-eval: disk almost full\n"),
            Expect::executed_action(&["store_syslog_message"]),
        )
        .note("BSD logger cannot target a remote port; driven with nc -u."),
        EvalCase::new(
            "syslog/drop-healthchecks",
            "syslog",
            "Throw away any message whose text mentions healthcheck. Keep everything \
             else.",
            syslog_probe("<14>Oct 11 22:14:15 evalhost netget-eval: healthcheck ok\n"),
            Expect::executed_action(&["ignore_syslog_message"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// NTP — recorded, never run. Neither `sntp` nor `ntpdate` accepts a
// destination port (sntp's -r only *sources* from 123), and binding 123 needs
// root. Left in the suite with the reason so the day a client appears the case
// is already written — and so the results file says "no client", not "passed".
// ---------------------------------------------------------------------------

#[cfg(feature = "ntp")]
fn ntp() -> Vec<EvalCase> {
    vec![EvalCase::unavailable(
        "ntp/current-time",
        "ntp",
        "Answer time requests with the correct current time, stratum 2.",
        "no installed NTP client accepts a destination port: sntp's -r only selects \
         the source port and ntpdate has no port option, so an unprivileged \
         loopback port is unreachable. Binding 123 needs root.",
    )]
}

// ---------------------------------------------------------------------------
// Telnet — curl speaks telnet:// and performs real option negotiation.
// ---------------------------------------------------------------------------

#[cfg(feature = "telnet")]
fn telnet_probe(input: Option<&str>) -> Probe {
    let probe = Probe::client(
        "curl",
        &["-sS", "--max-time", "230", "telnet://127.0.0.1:{PORT}"],
    );
    match input {
        Some(text) => probe.stdin(text),
        None => probe,
    }
}

#[cfg(feature = "telnet")]
fn telnet() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "telnet/banner",
            "telnet",
            "Greet everyone who connects with the banner NETGET EVAL TELNET.",
            telnet_probe(None),
            Expect::contains(&["NETGET EVAL TELNET"]),
        ),
        EvalCase::new(
            "telnet/login-prompt",
            "telnet",
            "Ask for a login name as soon as somebody connects.",
            telnet_probe(None),
            Expect::default().matching(r"(?i)login|username|user name"),
        ),
        EvalCase::new(
            "telnet/answer-command",
            "telnet",
            "If somebody types the word time, answer with the line \
             It is always noon here.",
            telnet_probe(Some("time\r\n")),
            Expect::contains(&["It is always noon here"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// TCP — nc. Generic transport by construction: raw TCP has no protocol for a
// third-party client to implement.
// ---------------------------------------------------------------------------

#[cfg(feature = "tcp")]
fn nc_tcp(input: &str) -> Probe {
    Probe::generic("nc", &["-w", "235", "127.0.0.1", "{PORT}"]).stdin(input)
}

#[cfg(feature = "tcp")]
fn tcp() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "tcp/fixed-reply",
            "tcp",
            "Reply to anything a client sends with the single line PONG-EVAL.",
            nc_tcp("hello\n"),
            Expect::contains(&["PONG-EVAL"]),
        ),
        EvalCase::new(
            "tcp/echo",
            "tcp",
            "Echo back exactly what the client sent.",
            nc_tcp("netget-eval-ping\n"),
            Expect::contains(&["netget-eval-ping"]),
        ),
        EvalCase::new(
            "tcp/uppercase",
            "tcp",
            "Send back the client's text in upper case.",
            nc_tcp("hello eval\n"),
            Expect::contains(&["HELLO EVAL"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// FTP — inetutils `ftp`, which takes the port as a positional argument.
// ---------------------------------------------------------------------------

#[cfg(feature = "ftp")]
fn ftp_probe(script: &str) -> Probe {
    Probe::client("ftp", &["-n", "-v", "127.0.0.1", "{PORT}"]).stdin(script)
}

#[cfg(feature = "ftp")]
fn ftp() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "ftp/banner",
            "ftp",
            "Greet every connection with the banner NetGet Eval FTP, and let anyone log \
             in anonymously.",
            ftp_probe("quit\r\n"),
            Expect::contains(&["NetGet Eval FTP"]),
        ),
        EvalCase::new(
            "ftp/anonymous-login",
            "ftp",
            "Let anyone log in anonymously and tell them the login succeeded.",
            ftp_probe("user anonymous eval@example.com\r\nquit\r\n"),
            Expect::default().matching(r"(?i)230|logged in|login successful"),
        ),
        EvalCase::new(
            "ftp/working-directory",
            "ftp",
            "Let anyone log in anonymously. The current directory is /eval.",
            ftp_probe("user anonymous eval@example.com\r\npwd\r\nquit\r\n"),
            Expect::contains(&["/eval"]),
        ),
    ]
}

// ---------------------------------------------------------------------------
// UDP — nc -u. Same caveat as TCP.
// ---------------------------------------------------------------------------

#[cfg(feature = "udp")]
fn nc_udp(input: &str) -> Probe {
    Probe::generic("nc", &["-u", "-w", "235", "127.0.0.1", "{PORT}"]).stdin(input)
}

#[cfg(feature = "udp")]
fn udp() -> Vec<EvalCase> {
    vec![
        EvalCase::new(
            "udp/fixed-reply",
            "udp",
            "Answer every datagram with the text UDP-EVAL-OK.",
            nc_udp("hello\n"),
            Expect::contains(&["UDP-EVAL-OK"]),
        ),
        EvalCase::new(
            "udp/echo",
            "udp",
            "Send every datagram straight back to whoever sent it, unchanged.",
            nc_udp("netget-eval-datagram\n"),
            Expect::contains(&["netget-eval-datagram"]),
        ),
    ]
}
