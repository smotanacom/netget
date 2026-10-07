#![cfg(feature = "webtransport")]
//! Pinned transport corrections exercised through public wire/library APIs.
use std::{borrow::Cow, sync::Arc, time::Duration};
use wtransport_proto::{
    bytes::BytesWriter,
    frame::{Frame, IoReadError, ParseError},
    headers::Headers,
    qpack::{Decoder, DecodingError, Encoder},
    settings::Settings,
    VarInt,
};

#[test]
fn qpack_overlong_and_truncated_integer_bits_return_errors_without_panicking() {
    for continuation in [vec![0x80; 20], {
        let mut value = vec![0x80; (usize::BITS as usize - 1) / 7];
        value.push(0x7f);
        value
    }] {
        let mut input = vec![0, 0, 0xff];
        input.extend(continuation);
        input.push(0);
        let result = std::panic::catch_unwind(|| Decoder::decode(&input));
        assert!(matches!(result, Ok(Err(DecodingError::IntegerOverflow))), "{result:?}");
    }
}

#[test]
fn qpack_requires_zero_dynamic_table_prefix() {
    for prefix in [vec![1, 0], vec![0, 1], vec![0, 0x80, 0]] {
        assert!(matches!(Decoder::decode(prefix), Err(DecodingError::DynamicNotSupported)));
    }
}

#[test]
fn qpack_field_count_and_decoded_size_exact_limits() {
    let fields = (0..64).map(|index| (format!("x-{index}"), "v".to_owned())).collect::<Vec<_>>();
    assert_eq!(Decoder::decode(Encoder::encode(&fields)).unwrap().len(), 64);
    let extra = fields.iter().cloned().chain([("x-extra".into(), "v".into())]).collect::<Vec<_>>();
    assert!(matches!(Decoder::decode(Encoder::encode(extra)), Err(DecodingError::FieldSectionTooLarge)));
    for size in [16 * 1024 - 33, 16 * 1024 - 32] {
        let result = Decoder::decode(Encoder::encode([("x", "a".repeat(size).as_str())]));
        if size == 16 * 1024 - 33 { assert!(result.is_ok()); }
        else { assert!(matches!(result, Err(DecodingError::FieldSectionTooLarge))); }
    }
}

#[test]
fn qpack_refuses_lost_duplicates_invalid_fields_and_pseudo_order() {
    assert!(matches!(Decoder::decode(Encoder::encode([("x", "a"), ("x", "b")])), Err(DecodingError::DuplicateField)));
    for fields in [vec![("Upper", "value")], vec![("x", "\r\n")], vec![("x", "ok"), (":path", "/")]] {
        assert!(matches!(Decoder::decode(Encoder::encode(fields)), Err(DecodingError::InvalidField)));
    }
}

#[test]
fn settings_detect_duplicate_unknown_ids() {
    let mut payload = Vec::new();
    for _ in 0..2 {
        payload.put_varint(VarInt::from_u32(0x4242)).unwrap();
        payload.put_varint(VarInt::from_u32(1)).unwrap();
    }
    assert!(Settings::with_frame(&Frame::new_settings(Cow::Owned(payload))).is_err());
}

#[tokio::test]
async fn frame_length_is_bounded_before_payload_read() {
    for length in [4096, 4097, 1024 * 1024] {
        let mut bytes = Vec::new();
        bytes.put_varint(VarInt::from_u32(0)).unwrap();
        bytes.put_varint(VarInt::from_u32(length)).unwrap();
        if length == 4096 { bytes.extend(vec![b'x'; 4096]); }
        let result = Frame::read_async(&mut bytes.as_slice()).await;
        if length == 4096 { assert_eq!(result.unwrap().payload().len(), 4096); }
        else { assert!(matches!(result, Err(IoReadError::Parse(ParseError::PayloadTooBig)))); }
    }
}

fn configs() -> (quinn::ServerConfig, quinn::ClientConfig, rustls::ServerConfig, rustls::ClientConfig) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
        .with_no_client_auth().with_single_cert(vec![cert.cert.der().clone()], key).unwrap();
    server_tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut roots = rustls::RootCertStore::empty(); roots.add(cert.cert.der().clone()).unwrap();
    let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13]).unwrap()
        .with_root_certificates(roots).with_no_client_auth();
    client_tls.alpn_protocols = vec![b"h3".to_vec()];
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(Duration::from_secs(20).try_into().unwrap()));
    transport.max_concurrent_bidi_streams(16u32.into()).max_concurrent_uni_streams(16u32.into());
    transport.datagram_receive_buffer_size(Some(8192));
    let transport = Arc::new(transport);
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(quinn::crypto::rustls::QuicServerConfig::try_from(server_tls.clone()).unwrap()));
    server.transport_config(transport.clone());
    let mut client = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(client_tls.clone()).unwrap()));
    client.transport_config(transport);
    (server, client, server_tls, client_tls)
}

struct RawPair {
    server_endpoint: quinn::Endpoint,
    client_endpoint: quinn::Endpoint,
    client: quinn::Connection,
    server: Option<quinn::Connecting>,
}
impl Drop for RawPair {
    fn drop(&mut self) {
        self.server_endpoint.close(0u32.into(), b"");
        self.client_endpoint.close(0u32.into(), b"");
    }
}
async fn raw_pair() -> RawPair {
    let (server_config, client_config, _, _) = configs();
    let server_endpoint = quinn::Endpoint::server(server_config, "127.0.0.1:0".parse().unwrap()).unwrap();
    let mut client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    client_endpoint.set_default_client_config(client_config);
    let (client, server) = tokio::join!(client_endpoint.connect(server_endpoint.local_addr().unwrap(), "localhost").unwrap(), async {server_endpoint.accept().await.unwrap().accept().unwrap()});
    RawPair {server_endpoint,client_endpoint,client:client.unwrap(),server:Some(server)}
}
fn settings_bytes(valid: bool) -> Vec<u8> {
    let mut settings = Settings::builder().enable_connect_protocol().enable_h3_datagrams();
    if valid { settings = settings.enable_webtransport(); }
    let mut output = vec![0];
    settings.build().generate_frame().write(&mut output).unwrap();
    output
}
fn request_bytes() -> Vec<u8> {
    let headers = [(":method", "CONNECT"), (":scheme", "https"), (":protocol", "webtransport"), (":authority", "localhost"), (":path", "/probe")].into_iter().collect::<Headers>();
    let mut output = Vec::new(); headers.generate_frame().write(&mut output).unwrap(); output
}

#[tokio::test]
async fn pending_session_drop_closes_socket_and_owned_partial_header_readers() {
    let mut pair = raw_pair().await;
    let mut session = Box::pin(wtransport::endpoint::IncomingSessionFuture::with_quic_connecting(pair.server.take().unwrap()));
    let wire = async {
        // Reading server settings proves its driver has started, before we park stream headers.
        let mut settings = pair.client.accept_uni().await.unwrap();
        let mut byte = [0]; settings.read_exact(&mut byte).await.unwrap();
        let mut uni = pair.client.open_uni().await.unwrap(); uni.write_all(&[0x40, 0x54]).await.unwrap();
        let (mut bi, _recv) = pair.client.open_bi().await.unwrap(); bi.write_all(&[0x40, 0x41]).await.unwrap();
        let mut pacing = tokio::time::interval(Duration::from_millis(20));
        pacing.tick().await; pacing.tick().await;
        (uni, bi)
    };
    let partials = tokio::select! {
        value = wire => value,
        result = &mut session => panic!("session should remain pending: {result:?}"),
        _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("driver did not start"),
    };
    drop(session);
    let close = tokio::time::timeout(Duration::from_secs(1), pair.client.closed()).await.expect("dropping the incoming future must close the owned QUIC socket");
    assert!(matches!(close, quinn::ConnectionError::ApplicationClosed(_)));
    drop(partials);
}

#[tokio::test]
async fn invalid_peer_settings_reject_session_before_handshake_headers() {
    let mut pair = raw_pair().await;
    let session = wtransport::endpoint::IncomingSessionFuture::with_quic_connecting(pair.server.take().unwrap());
    let wire = async {
        let mut control = pair.client.open_uni().await.unwrap();
        control.write_all(&settings_bytes(false)).await.unwrap();
        let (mut send, recv) = pair.client.open_bi().await.unwrap();
        send.write_all(&request_bytes()).await.unwrap();
        (control,send,recv)
    };
    let (result, streams) = tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(session, wire) }).await.unwrap();
    assert!(result.is_err(), "missing WebTransport opt-in must fail");
    let close = tokio::time::timeout(Duration::from_secs(1), pair.client.closed()).await.unwrap();
    match close { quinn::ConnectionError::ApplicationClosed(error) => assert_eq!(error.error_code.into_inner(), 0x109), error => panic!("{error}") }
    drop(streams);
}

#[tokio::test]
async fn fragmented_settings_survive_unrelated_stream_and_datagram_activity() {
    let mut pair = raw_pair().await;
    let session = wtransport::endpoint::IncomingSessionFuture::with_quic_connecting(pair.server.take().unwrap());
    let wire = async {
        let bytes = settings_bytes(true);
        let mut control = pair.client.open_uni().await.unwrap();
        control.write_all(&bytes[..2]).await.unwrap();
        let mut streams = Vec::new();
        let mut pacing = tokio::time::interval(Duration::from_millis(20));
        for byte in &bytes[2..] {
            // A genuinely trickled control frame crosses ready branches in the driver's select.
            if streams.len() < 3 {
                let mut uni = pair.client.open_uni().await.unwrap();
                uni.write_all(&[0x40, 0x54, 0]).await.unwrap(); streams.push(uni);
            }
            pair.client.send_datagram(bytes::Bytes::from_static(&[0, b'x'])).unwrap();
            pacing.tick().await;
            control.write_all(&[*byte]).await.unwrap();
        }
        let (mut send, recv) = pair.client.open_bi().await.unwrap();
        send.write_all(&request_bytes()).await.unwrap();
        (control, streams, send, recv)
    };
    let (result, streams) = tokio::time::timeout(Duration::from_secs(5), async {tokio::join!(session,wire)}).await.unwrap();
    assert_eq!(result.unwrap().path(), "/probe");
    drop(streams);
}

#[tokio::test]
async fn drop_last_connected_session_handle_closes_peer_even_with_partial_streams() {
    let (_, _, server_tls, client_tls) = configs();
    let server_config = wtransport::ServerConfig::builder().with_bind_address("127.0.0.1:0".parse().unwrap()).with_custom_tls(server_tls).build();
    let server = wtransport::Endpoint::server(server_config).unwrap();
    let client_config = wtransport::ClientConfig::builder().with_bind_address("127.0.0.1:0".parse().unwrap()).with_custom_tls(client_tls).build();
    let client = wtransport::Endpoint::client(client_config).unwrap();
    let (connected, accepted) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(client.connect(format!("https://localhost:{}/probe",server.local_addr().unwrap().port())), async {server.accept().await.await.unwrap().accept().await})
    }).await.unwrap();
    let connected = connected.unwrap(); let accepted = accepted.unwrap();
    let mut partial = connected.quic_connection().open_uni().await.unwrap();
    partial.write_all(&[0x40, 0x54]).await.unwrap();
    drop(accepted);
    tokio::time::timeout(Duration::from_secs(1), connected.closed()).await.expect("last session owner must close peer");
    drop(partial);
    client.close(VarInt::from_u32(0), b""); server.close(VarInt::from_u32(0), b"");
}
