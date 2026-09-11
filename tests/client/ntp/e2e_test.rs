//! E2E tests for the NTP client, against an NTP server running inside the test.
//!
//! **These tests used to query `time.google.com:123` and `pool.ntp.org:123`.** That breaks the
//! repository rule that tests bind to localhost only and never contact external endpoints, and
//! it made the suite depend on the public NTP pool being reachable and unloaded. It also hid a
//! real defect for as long as it stood: the client resolved its target with
//! `str::parse::<SocketAddr>()`, which does no name lookup at all, so `time.google.com:123`
//! failed at connect and no packet was ever sent. The tests passed anyway, because their
//! assertions were `output_contains("ntp") || output_contains("time")` — satisfied by the
//! instruction text echoing back — and their `ntp_response_received` rule was
//! `expect_at_most(1)`, which zero exchanges satisfies. A test that cannot tell "the client
//! worked" from "the client never sent anything" is not evidence of either.
//!
//! The peer here is a plain `tokio::net::UdpSocket` speaking RFC 5905 by hand: a real NTP
//! server would be a third-party dependency and a privileged port, and what needs proving is
//! that NetGet's client puts a well-formed request on the wire and understands the reply.
//! `expect_calls(1)` on the response event is what makes that assertion bite.

#[cfg(all(test, feature = "ntp"))]
mod ntp_client_tests {
    use crate::helpers::*;
    use std::time::Duration;
    use tokio::net::UdpSocket;

    /// Seconds between the NTP epoch (1900-01-01) and the Unix epoch (1970-01-01).
    const NTP_UNIX_OFFSET: u64 = 2_208_988_800;

    /// A one-shot NTP server. Answers the first request it receives with a well-formed
    /// stratum-2 reply and hands back the bytes the client sent, so the test can assert on
    /// the request as well as on what the client made of the response.
    ///
    /// Echoing the client's transmit timestamp into the origin field is not optional: a
    /// client that cannot match the reply to its own request discards it, which would show up
    /// as the response event simply never firing.
    ///
    /// `bind` selects which loopback address the responder listens on. The hostname test binds
    /// whatever `localhost` resolves to first, because that is the address the client will pick
    /// — on most systems `::1`, not `127.0.0.1`.
    async fn spawn_ntp_responder_on(
        bind: &str,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<Vec<u8>>) {
        let socket = UdpSocket::bind(bind).await.expect("bind responder");
        let addr = socket.local_addr().expect("responder addr");

        let handle = tokio::spawn(async move {
            let mut buf = vec![0u8; 128];
            let (n, peer) = socket.recv_from(&mut buf).await.expect("recv request");
            let request = buf[..n].to_vec();

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock before 1970")
                .as_secs()
                + NTP_UNIX_OFFSET;

            let mut reply = vec![0u8; 48];
            // LI 0, VN 4, mode 4 (server).
            reply[0] = 0b00_100_100;
            reply[1] = 2; // stratum 2
            reply[2] = 4; // poll interval
            reply[3] = 0xEC; // precision, -20 as a signed byte
            reply[12..16].copy_from_slice(b"GPS\0"); // reference identifier
            reply[16..20].copy_from_slice(&(now as u32).to_be_bytes()); // reference ts
            if request.len() >= 48 {
                // The client's transmit timestamp becomes our origin timestamp.
                reply[24..32].copy_from_slice(&request[40..48]);
            }
            reply[32..36].copy_from_slice(&(now as u32).to_be_bytes()); // receive ts
            reply[40..44].copy_from_slice(&(now as u32).to_be_bytes()); // transmit ts

            socket.send_to(&reply, peer).await.expect("send reply");
            request
        });

        (addr, handle)
    }

    /// The client queries a real NTP server and the reply reaches the model.
    ///
    /// LLM calls: 3 — the startup instruction, the `ntp_connected` event (which is what
    /// actually triggers the query), and the `ntp_response_received` event.
    #[tokio::test]
    async fn test_ntp_client_queries_a_server_and_reports_the_stratum() -> E2EResult<()> {
        let (server_addr, responder) = spawn_ntp_responder_on("127.0.0.1:0").await;
        let target = server_addr.to_string();

        let client_config = NetGetConfig::new(format!("Query {target} for the current time."))
            .with_mock({
                let target = target.clone();
                move |mock| {
                    mock.on_instruction_containing("for the current time")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "open_client",
                                "remote_addr": target,
                                "protocol": "NTP",
                                "instruction": "Query the time server"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // The client asks the model what to do the moment it is connected,
                        // and only queries if the answer says `query_time`. Neither of the
                        // three tests this file replaced had a rule for `ntp_connected`, so
                        // that call fell through to an unmatched mock, the connect task
                        // logged an LLM error and returned, and **no NTP packet was ever
                        // sent**. `expect_at_most(1)` on the response rule then made zero
                        // exchanges a pass.
                        .on_event("ntp_connected")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "query_time"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                        // The stratum match is the assertion: the rule only fires if the
                        // client actually decoded our reply and put stratum 2 in the event.
                        // A rule that matched on the event id alone would fire on a response
                        // the client had misparsed just as readily.
                        .on_event("ntp_response_received")
                        .and_event_data_contains("stratum", "2")
                        .respond_with_actions(serde_json::json!([
                            {
                                "type": "analyze_response"
                            }
                        ]))
                        .expect_calls(1)
                        .and()
                }
            });

        let mut client = start_netget_client(client_config).await?;

        // The responder returns the client's own request once it has answered it.
        let request = tokio::time::timeout(Duration::from_secs(30), responder)
            .await
            .map_err(|_| "the NTP client never sent a request to the local server")?
            .map_err(|e| format!("responder task panicked: {e}"))?;

        assert_eq!(
            request.len(),
            48,
            "an NTP client request is exactly 48 bytes, got {}",
            request.len()
        );
        assert_eq!(
            request[0] & 0b0000_0111,
            3,
            "mode must be 3 (client), got {:#04x} in the first octet",
            request[0]
        );
        assert_ne!(
            &request[40..48],
            &[0u8; 8],
            "the client must set its own transmit timestamp; without it the reply cannot be \
             matched to the request"
        );

        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;
        Ok(())
    }

    /// The target may be a hostname, not only a literal `IP:port`.
    ///
    /// This is the regression guard for the `str::parse::<SocketAddr>()` defect described in
    /// this file's header. `localhost` is a hostname as far as the resolver is concerned — it
    /// does not parse as a `SocketAddr` — while still keeping the test entirely on loopback.
    #[tokio::test]
    async fn test_ntp_client_resolves_a_hostname_target() -> E2EResult<()> {
        // Bind whatever `localhost` resolves to first — the same address the client's
        // `lookup_host` will hand back. Assuming 127.0.0.1 would fail on every system that
        // prefers `::1`, which is most of them, and would look like the resolution fix not
        // working rather than the test binding the wrong socket.
        let localhost = tokio::net::lookup_host("localhost:0")
            .await
            .expect("resolve localhost")
            .next()
            .expect("localhost resolves to at least one address");
        let (server_addr, responder) = spawn_ntp_responder_on(&localhost.to_string()).await;
        let target = format!("localhost:{}", server_addr.port());

        let client_config = NetGetConfig::new(format!("Query {target} by name.")).with_mock({
            let target = target.clone();
            move |mock| {
                mock.on_instruction_containing("by name")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_client",
                            "remote_addr": target,
                            "protocol": "NTP",
                            "instruction": "Query the time server by hostname"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    .on_event("ntp_connected")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "query_time"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
                    .on_event("ntp_response_received")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "analyze_response"
                        }
                    ]))
                    .expect_calls(1)
                    .and()
            }
        });

        let mut client = start_netget_client(client_config).await?;

        let request = tokio::time::timeout(Duration::from_secs(30), responder)
            .await
            .map_err(|_| {
                "the NTP client never reached localhost by name - `str::parse::<SocketAddr>()` \
                 does no DNS lookup, which is the defect this test guards"
            })?
            .map_err(|e| format!("responder task panicked: {e}"))?;
        assert_eq!(
            request.len(),
            48,
            "a hostname target must send a real query"
        );

        client.wait_for_mocks(30).await;
        client.verify_mocks().await?;
        client.stop().await?;
        Ok(())
    }
}
