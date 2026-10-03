use serde_json::Value;
use std::{process::Stdio, time::Duration};
pub(crate) fn read_records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter_map(|s| serde_json::from_str(s).ok())
        .collect()
}

pub(crate) struct Collector {
    root: tempfile::TempDir,
    pub(crate) port: u16,
    child: tokio::process::Child,
}
impl Collector {
    pub(crate) async fn start() -> Self {
        let binary = std::env::var("NETGET_NETFLOW_V9_COLLECTOR")
            .expect("pinned official GoFlow2 collector required; no missing-service skip");
        let version = tokio::process::Command::new(&binary)
            .arg("-v")
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(version.status.success());
        assert!(String::from_utf8_lossy(&version.stdout).contains("GoFlow2 v2.2.7 "));
        let root = tempfile::Builder::new()
            .prefix("netget-netflow-v9-goflow-")
            .tempdir()
            .unwrap();
        let reservation = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let output = std::fs::File::create(root.path().join("records.json")).unwrap();
        let error = std::fs::File::create(root.path().join("daemon.log")).unwrap();
        let child = tokio::process::Command::new(binary)
            .args([
                "-listen",
                &format!("netflow://127.0.0.1:{port}"),
                "-addr",
                "127.0.0.1:0",
                "-produce",
                "raw",
                "-format",
                "json",
                "-transport",
                "file",
            ])
            .stdout(Stdio::from(output))
            .stderr(Stdio::from(error))
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let mut service = Self { root, port, child };
        let probe = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // RFC3954 literal one-template packet with reserved fixture Source-ID999.
        let header = [
            0, 9, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3, 231, 0, 0, 0, 12, 0xfc, 0, 0,
            1, 0, 8, 0, 4,
        ];
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                assert!(
                    service.child.try_wait().unwrap().is_none(),
                    "{}",
                    std::fs::read_to_string(service.root.path().join("daemon.log")).unwrap()
                );
                probe
                    .send_to(&header, format!("127.0.0.1:{port}"))
                    .await
                    .unwrap();
                if service
                    .records()
                    .iter()
                    .any(|r| r["message"]["source-id"] == 999)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("official collector must receive a readiness datagram");
        service
    }
    fn records(&self) -> Vec<Value> {
        read_records(&self.root.path().join("records.json"))
    }
    pub(crate) async fn messages(&mut self, source: u32, count: usize) -> Vec<Value> {
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let messages = self
                    .records()
                    .into_iter()
                    .filter(|r| r["message"]["source-id"] == source)
                    .collect::<Vec<_>>();
                if messages.len() >= count {
                    break messages;
                }
                assert!(self.child.try_wait().unwrap().is_none(), "collector exited");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap()
    }
    pub(crate) async fn stop(mut self) {
        self.child.kill().await.unwrap();
        self.child.wait().await.unwrap();
    }
}

pub(crate) fn peer_golden() -> Vec<u8> {
    include_str!("../server/netflow_v9/softflowd_ipv4.hex")
        .split_whitespace()
        .map(|v| u8::from_str_radix(v, 16).unwrap())
        .collect()
}
pub(crate) async fn export(addr: std::net::SocketAddr, sampling: bool) -> Vec<u8> {
    let binary = std::env::var("NETGET_NETFLOW_V9_EXPORTER")
        .expect("pinned unmodified softflowd1.1.1 upstream legacy exporter required; no skip");
    let version = tokio::process::Command::new(&binary)
        .arg("-h")
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        String::from_utf8_lossy(&version.stderr).contains("This is softflowd version 1.1.1.")
            || String::from_utf8_lossy(&version.stdout)
                .contains("This is softflowd version 1.1.1.")
    );
    let root = tempfile::Builder::new()
        .prefix("netget-netflow-v9-pcap-")
        .tempdir()
        .unwrap();
    let mut pcap = vec![
        0xd4, 0xc3, 0xb2, 0xa1, 2, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 0, 0, 1, 0, 0, 0,
    ];
    // Literal valid Ethernet/IPv4/UDP packet; length28 bytes, no application payload.
    let frame = [
        2, 0, 0, 0, 0, 2, 2, 0, 0, 0, 0, 1, 8, 0, 0x45, 0, 0, 28, 0, 1, 0, 0, 64, 17, 0x8e, 0x99,
        192, 0, 2, 1, 198, 51, 100, 2, 0x30, 0x39, 8, 7, 0, 8, 0, 0,
    ];
    for micros in [0u32, 100000, 200000] {
        for v in [
            1700000000u32,
            micros,
            frame.len() as u32,
            frame.len() as u32,
        ] {
            pcap.extend(v.to_le_bytes());
        }
        pcap.extend(frame);
    }
    let file = root.path().join("flows.pcap");
    std::fs::write(&file, pcap).unwrap();
    // Proxy captures the actual datagram and forwards it unchanged to the target.
    let proxy = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let destination = proxy.local_addr().unwrap().to_string();
    let mut command = tokio::process::Command::new(binary);
    command.args([
        "-a",
        "-d",
        "-r",
        file.to_str().unwrap(),
        "-n",
        &destination,
        "-v",
        "9",
        "-P",
        "udp",
        "-c",
        "none",
    ]);
    if sampling {
        command.args(["-s", "2"]);
    }
    let out = tokio::time::timeout(Duration::from_secs(15), command.kill_on_drop(true).output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut bytes = vec![0; 8193];
    let n = tokio::time::timeout(Duration::from_secs(5), proxy.recv(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    bytes.truncate(n);
    proxy.send_to(&bytes, addr).await.unwrap();
    bytes
}
pub(crate) fn decoded_values(record: &Value) -> Vec<(u16, Vec<u8>)> {
    use base64::Engine;
    record["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| {
            (
                v["type"].as_u64().unwrap() as u16,
                base64::engine::general_purpose::STANDARD
                    .decode(v["value"].as_str().unwrap())
                    .unwrap(),
            )
        })
        .collect()
}
