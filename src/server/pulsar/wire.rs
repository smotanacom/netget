//! The Pulsar binary protocol for both roles: the protobuf commands NetGet speaks (a subset of
//! `PulsarApi.proto`, field numbers and proto2 `required` labels as upstream defines them, so
//! a zero-valued required field is still written), frame encoding, the payload section of
//! SEND and MESSAGE frames (magic, CRC32C, metadata), and batch entries.
use anyhow::{bail, ensure, Context, Result};
use prost::Message;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};

/// A frame (all of it, after the 4-byte total size) at most; Pulsar's own default
/// `maxMessageSize` is 5 MiB, and NetGet advertises this number in CONNECTED.
pub const MAX_FRAME: usize = 5 * 1024 * 1024;
/// A command's protobuf at most.
pub const MAX_COMMAND: usize = 64 * 1024;
/// Messages one batch may carry.
pub const MAX_BATCH: usize = 1000;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// The protocol version NetGet speaks (v19: transaction-coordinator connect; NetGet uses none
/// of the newer features but nothing in them changes the framing).
pub const PROTOCOL_VERSION: i32 = 19;
const MAGIC_CRC32C: u16 = 0x0e01;

pub mod command_type {
    pub const CONNECT: i32 = 2;
    pub const CONNECTED: i32 = 3;
    pub const SUBSCRIBE: i32 = 4;
    pub const PRODUCER: i32 = 5;
    pub const SEND: i32 = 6;
    pub const SEND_RECEIPT: i32 = 7;
    pub const SEND_ERROR: i32 = 8;
    pub const MESSAGE: i32 = 9;
    pub const ACK: i32 = 10;
    pub const FLOW: i32 = 11;
    pub const UNSUBSCRIBE: i32 = 12;
    pub const SUCCESS: i32 = 13;
    pub const ERROR: i32 = 14;
    pub const CLOSE_PRODUCER: i32 = 15;
    pub const CLOSE_CONSUMER: i32 = 16;
    pub const PRODUCER_SUCCESS: i32 = 17;
    pub const PING: i32 = 18;
    pub const PONG: i32 = 19;
    pub const REDELIVER_UNACKNOWLEDGED_MESSAGES: i32 = 20;
    pub const PARTITIONED_METADATA: i32 = 21;
    pub const PARTITIONED_METADATA_RESPONSE: i32 = 22;
    pub const LOOKUP: i32 = 23;
    pub const LOOKUP_RESPONSE: i32 = 24;
    pub const GET_LAST_MESSAGE_ID: i32 = 29;
    pub const GET_LAST_MESSAGE_ID_RESPONSE: i32 = 30;
    pub const GET_SCHEMA: i32 = 34;
    pub const GET_SCHEMA_RESPONSE: i32 = 35;
}

/// `ServerError` values NetGet answers with.
pub mod server_error {
    pub const UNKNOWN_ERROR: i32 = 0;
    pub const AUTHORIZATION_ERROR: i32 = 4;
    pub const SERVICE_NOT_READY: i32 = 6;
    pub const CHECKSUM_ERROR: i32 = 9;
    pub const UNSUPPORTED_VERSION_ERROR: i32 = 10;
    pub const TOPIC_NOT_FOUND: i32 = 11;
    pub const SUBSCRIPTION_NOT_FOUND: i32 = 12;
    pub const CONSUMER_NOT_FOUND: i32 = 13;
    pub const TOO_MANY_REQUESTS: i32 = 14;
    pub const CONSUMER_BUSY: i32 = 5;
    pub const INVALID_TOPIC_NAME: i32 = 17;
    pub const NOT_ALLOWED_ERROR: i32 = 22;
}

pub const SUB_TYPES: [&str; 4] = ["Exclusive", "Shared", "Failover", "Key_Shared"];

#[derive(Clone, PartialEq, Message)]
pub struct KeyValue {
    #[prost(string, required, tag = "1")]
    pub key: String,
    #[prost(string, required, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct MessageIdData {
    #[prost(uint64, required, tag = "1")]
    pub ledger_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub entry_id: u64,
    #[prost(int32, optional, tag = "3")]
    pub partition: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub batch_index: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct MessageMetadata {
    #[prost(string, required, tag = "1")]
    pub producer_name: String,
    #[prost(uint64, required, tag = "2")]
    pub sequence_id: u64,
    #[prost(uint64, required, tag = "3")]
    pub publish_time: u64,
    #[prost(message, repeated, tag = "4")]
    pub properties: Vec<KeyValue>,
    #[prost(string, optional, tag = "6")]
    pub partition_key: Option<String>,
    #[prost(int32, optional, tag = "8")]
    pub compression: Option<i32>,
    #[prost(uint32, optional, tag = "9")]
    pub uncompressed_size: Option<u32>,
    #[prost(int32, optional, tag = "11")]
    pub num_messages_in_batch: Option<i32>,
    #[prost(uint64, optional, tag = "12")]
    pub event_time: Option<u64>,
    #[prost(string, optional, tag = "14")]
    pub encryption_algo: Option<String>,
    #[prost(bool, optional, tag = "25")]
    pub null_value: Option<bool>,
    #[prost(int32, optional, tag = "27")]
    pub num_chunks_from_msg: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct SingleMessageMetadata {
    #[prost(message, repeated, tag = "1")]
    pub properties: Vec<KeyValue>,
    #[prost(string, optional, tag = "2")]
    pub partition_key: Option<String>,
    #[prost(int32, required, tag = "3")]
    pub payload_size: i32,
    #[prost(bool, optional, tag = "4")]
    pub compacted_out: Option<bool>,
    #[prost(uint64, optional, tag = "5")]
    pub event_time: Option<u64>,
    #[prost(uint64, optional, tag = "8")]
    pub sequence_id: Option<u64>,
    #[prost(bool, optional, tag = "9")]
    pub null_value: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandConnect {
    #[prost(string, required, tag = "1")]
    pub client_version: String,
    #[prost(bytes = "vec", optional, tag = "3")]
    pub auth_data: Option<Vec<u8>>,
    #[prost(int32, optional, tag = "4")]
    pub protocol_version: Option<i32>,
    #[prost(string, optional, tag = "5")]
    pub auth_method_name: Option<String>,
    #[prost(string, optional, tag = "6")]
    pub proxy_to_broker_url: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandConnected {
    #[prost(string, required, tag = "1")]
    pub server_version: String,
    #[prost(int32, optional, tag = "2")]
    pub protocol_version: Option<i32>,
    #[prost(int32, optional, tag = "3")]
    pub max_message_size: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandSubscribe {
    #[prost(string, required, tag = "1")]
    pub topic: String,
    #[prost(string, required, tag = "2")]
    pub subscription: String,
    #[prost(int32, required, tag = "3")]
    pub sub_type: i32,
    #[prost(uint64, required, tag = "4")]
    pub consumer_id: u64,
    #[prost(uint64, required, tag = "5")]
    pub request_id: u64,
    #[prost(string, optional, tag = "6")]
    pub consumer_name: Option<String>,
    #[prost(bool, optional, tag = "8")]
    pub durable: Option<bool>,
    #[prost(int32, optional, tag = "13")]
    pub initial_position: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandPartitionedTopicMetadata {
    #[prost(string, required, tag = "1")]
    pub topic: String,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandPartitionedTopicMetadataResponse {
    #[prost(uint32, optional, tag = "1")]
    pub partitions: Option<u32>,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
    #[prost(int32, optional, tag = "3")]
    pub response: Option<i32>,
    #[prost(int32, optional, tag = "4")]
    pub error: Option<i32>,
    #[prost(string, optional, tag = "5")]
    pub message: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandLookupTopic {
    #[prost(string, required, tag = "1")]
    pub topic: String,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
    #[prost(bool, optional, tag = "3")]
    pub authoritative: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandLookupTopicResponse {
    #[prost(string, optional, tag = "1")]
    pub broker_service_url: Option<String>,
    #[prost(int32, optional, tag = "3")]
    pub response: Option<i32>,
    #[prost(uint64, required, tag = "4")]
    pub request_id: u64,
    #[prost(bool, optional, tag = "5")]
    pub authoritative: Option<bool>,
    #[prost(int32, optional, tag = "6")]
    pub error: Option<i32>,
    #[prost(string, optional, tag = "7")]
    pub message: Option<String>,
    #[prost(bool, optional, tag = "8")]
    pub proxy_through_service_url: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandProducer {
    #[prost(string, required, tag = "1")]
    pub topic: String,
    #[prost(uint64, required, tag = "2")]
    pub producer_id: u64,
    #[prost(uint64, required, tag = "3")]
    pub request_id: u64,
    #[prost(string, optional, tag = "4")]
    pub producer_name: Option<String>,
    #[prost(bool, optional, tag = "5")]
    pub encrypted: Option<bool>,
    #[prost(int32, optional, tag = "10")]
    pub producer_access_mode: Option<i32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandSend {
    #[prost(uint64, required, tag = "1")]
    pub producer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub sequence_id: u64,
    #[prost(int32, optional, tag = "3")]
    pub num_messages: Option<i32>,
    #[prost(uint64, optional, tag = "4")]
    pub txnid_least_bits: Option<u64>,
    #[prost(uint64, optional, tag = "6")]
    pub highest_sequence_id: Option<u64>,
    #[prost(bool, optional, tag = "7")]
    pub is_chunk: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandSendReceipt {
    #[prost(uint64, required, tag = "1")]
    pub producer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub sequence_id: u64,
    #[prost(message, optional, tag = "3")]
    pub message_id: Option<MessageIdData>,
    #[prost(uint64, optional, tag = "4")]
    pub highest_sequence_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandSendError {
    #[prost(uint64, required, tag = "1")]
    pub producer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub sequence_id: u64,
    #[prost(int32, required, tag = "3")]
    pub error: i32,
    #[prost(string, required, tag = "4")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandMessage {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(message, required, tag = "2")]
    pub message_id: MessageIdData,
    #[prost(uint32, optional, tag = "3")]
    pub redelivery_count: Option<u32>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandAck {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(int32, required, tag = "2")]
    pub ack_type: i32,
    #[prost(message, repeated, tag = "3")]
    pub message_id: Vec<MessageIdData>,
    #[prost(uint64, optional, tag = "8")]
    pub request_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandAckResponse {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(uint64, optional, tag = "6")]
    pub request_id: Option<u64>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandFlow {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(uint32, required, tag = "2")]
    pub message_permits: u32,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandUnsubscribe {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandCloseProducer {
    #[prost(uint64, required, tag = "1")]
    pub producer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandCloseConsumer {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandRedeliverUnacknowledgedMessages {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(message, repeated, tag = "2")]
    pub message_ids: Vec<MessageIdData>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandSuccess {
    #[prost(uint64, required, tag = "1")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandProducerSuccess {
    #[prost(uint64, required, tag = "1")]
    pub request_id: u64,
    #[prost(string, required, tag = "2")]
    pub producer_name: String,
    #[prost(int64, optional, tag = "3")]
    pub last_sequence_id: Option<i64>,
    /// The Java client reads this unconditionally; a broker without schemas sends it empty.
    #[prost(bytes = "vec", optional, tag = "4")]
    pub schema_version: Option<Vec<u8>>,
    #[prost(bool, optional, tag = "6")]
    pub producer_ready: Option<bool>,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandError {
    #[prost(uint64, required, tag = "1")]
    pub request_id: u64,
    #[prost(int32, required, tag = "2")]
    pub error: i32,
    #[prost(string, required, tag = "3")]
    pub message: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandPing {}

#[derive(Clone, PartialEq, Message)]
pub struct CommandPong {}

#[derive(Clone, PartialEq, Message)]
pub struct CommandGetLastMessageId {
    #[prost(uint64, required, tag = "1")]
    pub consumer_id: u64,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandGetLastMessageIdResponse {
    #[prost(message, required, tag = "1")]
    pub last_message_id: MessageIdData,
    #[prost(uint64, required, tag = "2")]
    pub request_id: u64,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandGetSchema {
    #[prost(uint64, required, tag = "1")]
    pub request_id: u64,
    #[prost(string, required, tag = "2")]
    pub topic: String,
}

#[derive(Clone, PartialEq, Message)]
pub struct CommandGetSchemaResponse {
    #[prost(uint64, required, tag = "1")]
    pub request_id: u64,
    #[prost(int32, optional, tag = "2")]
    pub error_code: Option<i32>,
    #[prost(string, optional, tag = "3")]
    pub error_message: Option<String>,
}

#[derive(Clone, PartialEq, Message)]
pub struct BaseCommand {
    #[prost(int32, required, tag = "1")]
    pub r#type: i32,
    #[prost(message, optional, tag = "2")]
    pub connect: Option<CommandConnect>,
    #[prost(message, optional, tag = "3")]
    pub connected: Option<CommandConnected>,
    #[prost(message, optional, tag = "4")]
    pub subscribe: Option<CommandSubscribe>,
    #[prost(message, optional, tag = "5")]
    pub producer: Option<CommandProducer>,
    #[prost(message, optional, tag = "6")]
    pub send: Option<CommandSend>,
    #[prost(message, optional, tag = "7")]
    pub send_receipt: Option<CommandSendReceipt>,
    #[prost(message, optional, tag = "8")]
    pub send_error: Option<CommandSendError>,
    #[prost(message, optional, tag = "9")]
    pub message: Option<CommandMessage>,
    #[prost(message, optional, tag = "10")]
    pub ack: Option<CommandAck>,
    #[prost(message, optional, tag = "11")]
    pub flow: Option<CommandFlow>,
    #[prost(message, optional, tag = "12")]
    pub unsubscribe: Option<CommandUnsubscribe>,
    #[prost(message, optional, tag = "13")]
    pub success: Option<CommandSuccess>,
    #[prost(message, optional, tag = "14")]
    pub error: Option<CommandError>,
    #[prost(message, optional, tag = "15")]
    pub close_producer: Option<CommandCloseProducer>,
    #[prost(message, optional, tag = "16")]
    pub close_consumer: Option<CommandCloseConsumer>,
    #[prost(message, optional, tag = "17")]
    pub producer_success: Option<CommandProducerSuccess>,
    #[prost(message, optional, tag = "18")]
    pub ping: Option<CommandPing>,
    #[prost(message, optional, tag = "19")]
    pub pong: Option<CommandPong>,
    #[prost(message, optional, tag = "20")]
    pub redeliver_unacknowledged_messages: Option<CommandRedeliverUnacknowledgedMessages>,
    #[prost(message, optional, tag = "21")]
    pub partition_metadata: Option<CommandPartitionedTopicMetadata>,
    #[prost(message, optional, tag = "22")]
    pub partition_metadata_response: Option<CommandPartitionedTopicMetadataResponse>,
    #[prost(message, optional, tag = "23")]
    pub lookup_topic: Option<CommandLookupTopic>,
    #[prost(message, optional, tag = "24")]
    pub lookup_topic_response: Option<CommandLookupTopicResponse>,
    #[prost(message, optional, tag = "29")]
    pub get_last_message_id: Option<CommandGetLastMessageId>,
    #[prost(message, optional, tag = "30")]
    pub get_last_message_id_response: Option<CommandGetLastMessageIdResponse>,
    #[prost(message, optional, tag = "34")]
    pub get_schema: Option<CommandGetSchema>,
    #[prost(message, optional, tag = "35")]
    pub get_schema_response: Option<CommandGetSchemaResponse>,
    #[prost(message, optional, tag = "38")]
    pub ack_response: Option<CommandAckResponse>,
}

impl BaseCommand {
    pub fn of(kind: i32) -> Self {
        BaseCommand {
            r#type: kind,
            ..Default::default()
        }
    }
}

/// CRC-32C (Castagnoli), as Pulsar checksums metadata and payload.
pub fn crc32c(data: &[u8]) -> u32 {
    const TABLE: [u32; 256] = {
        let mut t = [0u32; 256];
        let mut i = 0;
        while i < 256 {
            let mut c = i as u32;
            let mut k = 0;
            while k < 8 {
                c = if c & 1 != 0 {
                    0x82F6_3B78 ^ (c >> 1)
                } else {
                    c >> 1
                };
                k += 1;
            }
            t[i] = c;
            i += 1;
        }
        t
    };
    let mut crc = !0u32;
    for b in data {
        crc = TABLE[((crc ^ u32::from(*b)) & 0xff) as usize] ^ (crc >> 8);
    }
    !crc
}

/// A frame carrying only a command.
pub fn simple(cmd: &BaseCommand) -> Vec<u8> {
    let body = cmd.encode_to_vec();
    let mut out = Vec::with_capacity(8 + body.len());
    out.extend_from_slice(&((4 + body.len()) as u32).to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out
}

/// A frame carrying a command, then the checksummed metadata and payload (SEND, MESSAGE).
pub fn with_payload(cmd: &BaseCommand, metadata: &MessageMetadata, payload: &[u8]) -> Vec<u8> {
    let body = cmd.encode_to_vec();
    let meta = metadata.encode_to_vec();
    let mut checked = Vec::with_capacity(4 + meta.len() + payload.len());
    checked.extend_from_slice(&(meta.len() as u32).to_be_bytes());
    checked.extend_from_slice(&meta);
    checked.extend_from_slice(payload);
    let total = 4 + body.len() + 2 + 4 + checked.len();
    let mut out = Vec::with_capacity(4 + total);
    out.extend_from_slice(&(total as u32).to_be_bytes());
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&MAGIC_CRC32C.to_be_bytes());
    out.extend_from_slice(&crc32c(&checked).to_be_bytes());
    out.extend_from_slice(&checked);
    out
}

/// One frame read off the wire.
pub struct Frame {
    pub command: BaseCommand,
    /// For SEND and MESSAGE: the metadata and the (possibly batched) payload.
    pub payload: Option<(MessageMetadata, Vec<u8>)>,
}

/// A frame's payload section: none, a decoded one, or why it was refused.
pub type Payload = Result<Option<(MessageMetadata, Vec<u8>)>, PayloadError>;

/// Why a payload section was refused; the frame itself was well-formed.
#[derive(Debug)]
pub enum PayloadError {
    Checksum,
    Malformed(String),
}

/// Read one frame. A frame over `MAX_FRAME` or a command that does not decode is an error:
/// the peer is not speaking Pulsar. A bad checksum or metadata is returned beside the command,
/// so a SEND can be answered with a SendError rather than dropping the connection.
pub async fn read_frame<R: AsyncRead + Unpin>(
    r: &mut R,
    idle: Duration,
) -> Result<Option<(BaseCommand, Payload)>> {
    let mut size = [0u8; 4];
    match tokio::time::timeout(idle, r.read_exact(&mut size)).await {
        Err(_) => bail!("Pulsar peer idle"),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Ok(r) => {
            r?;
        }
    }
    let total = u32::from_be_bytes(size) as usize;
    ensure!(
        (4..=MAX_FRAME).contains(&total),
        "Pulsar frame of {total} bytes (at most {MAX_FRAME})"
    );
    let mut frame = vec![0u8; total];
    tokio::time::timeout(IO_TIMEOUT, r.read_exact(&mut frame))
        .await
        .context("Pulsar frame read deadline")??;
    let cmd_len = u32::from_be_bytes(frame[0..4].try_into().unwrap_or_default()) as usize;
    ensure!(
        cmd_len <= MAX_COMMAND && 4 + cmd_len <= total,
        "Pulsar command of {cmd_len} bytes does not fit"
    );
    let command = BaseCommand::decode(&frame[4..4 + cmd_len])
        .context("Pulsar command is not a BaseCommand")?;
    let rest = &frame[4 + cmd_len..];
    if rest.is_empty() {
        return Ok(Some((command, Ok(None))));
    }
    Ok(Some((command, parse_payload(rest))))
}

fn parse_payload(rest: &[u8]) -> Payload {
    let bad = |m: &str| PayloadError::Malformed(m.to_string());
    let checked = if rest.len() >= 6 && u16::from_be_bytes([rest[0], rest[1]]) == MAGIC_CRC32C {
        let want = u32::from_be_bytes([rest[2], rest[3], rest[4], rest[5]]);
        let checked = &rest[6..];
        if crc32c(checked) != want {
            return Err(PayloadError::Checksum);
        }
        checked
    } else {
        rest
    };
    if checked.len() < 4 {
        return Err(bad("payload section too short"));
    }
    let meta_len = u32::from_be_bytes([checked[0], checked[1], checked[2], checked[3]]) as usize;
    if 4 + meta_len > checked.len() {
        return Err(bad("metadata size past the frame"));
    }
    let metadata = MessageMetadata::decode(&checked[4..4 + meta_len])
        .map_err(|e| bad(&format!("metadata: {e}")))?;
    Ok(Some((metadata, checked[4 + meta_len..].to_vec())))
}

/// One message out of a payload: what a single message or one batch entry carries.
#[derive(Clone, Debug)]
pub struct Entry {
    pub properties: Vec<KeyValue>,
    pub key: Option<String>,
    pub event_time: Option<u64>,
    pub sequence_id: u64,
    pub payload: Vec<u8>,
}

/// The messages a SEND carries: one, or each entry of a batch. Compressed and encrypted
/// payloads are refused (NetGet decompresses nothing it could not show the model).
pub fn entries(meta: &MessageMetadata, payload: &[u8]) -> Result<Vec<Entry>> {
    ensure!(
        meta.compression.unwrap_or(0) == 0,
        "compressed payloads are not supported"
    );
    ensure!(
        meta.encryption_algo.is_none(),
        "encrypted payloads are not supported"
    );
    ensure!(
        meta.num_chunks_from_msg.unwrap_or(1) <= 1,
        "chunked messages are not supported"
    );
    let Some(n) = meta.num_messages_in_batch.filter(|_| batched(meta)) else {
        return Ok(vec![Entry {
            properties: meta.properties.clone(),
            key: meta.partition_key.clone(),
            event_time: meta.event_time.filter(|t| *t > 0),
            sequence_id: meta.sequence_id,
            payload: payload.to_vec(),
        }]);
    };
    ensure!(
        (1..=MAX_BATCH as i32).contains(&n),
        "a batch of {n} messages"
    );
    let mut out = Vec::new();
    let mut rest = payload;
    for i in 0..n as u64 {
        ensure!(rest.len() >= 4, "batch entry {i} is cut short");
        let len = u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]) as usize;
        ensure!(
            4 + len <= rest.len(),
            "batch entry {i} metadata past the payload"
        );
        let single =
            SingleMessageMetadata::decode(&rest[4..4 + len]).context("batch entry metadata")?;
        rest = &rest[4 + len..];
        let size = usize::try_from(single.payload_size).context("negative payload size")?;
        ensure!(
            size <= rest.len(),
            "batch entry {i} payload past the payload"
        );
        let (body, tail) = rest.split_at(size);
        rest = tail;
        if single.compacted_out == Some(true) {
            continue;
        }
        out.push(Entry {
            properties: single.properties,
            key: single.partition_key,
            event_time: single.event_time.filter(|t| *t > 0),
            sequence_id: single.sequence_id.unwrap_or(meta.sequence_id + i),
            payload: body.to_vec(),
        });
    }
    Ok(out)
}

/// Whether the producer batched: clients that batch always set num_messages_in_batch.
fn batched(meta: &MessageMetadata) -> bool {
    meta.num_messages_in_batch.is_some()
}

/// A topic name in its full form: `persistent://public/default/t` from `t`.
pub fn full_topic(t: &str) -> Result<String> {
    ensure!(
        !t.is_empty() && t.len() <= 512,
        "topic name must be 1-512 bytes"
    );
    if t.starts_with("persistent://") || t.starts_with("non-persistent://") {
        let rest = t.split_once("://").map(|(_, r)| r).unwrap_or_default();
        ensure!(
            rest.split('/').count() == 3 && rest.split('/').all(|p| !p.is_empty()),
            "topic must be tenant/namespace/name"
        );
        return Ok(t.to_string());
    }
    ensure!(!t.contains("://"), "unknown topic domain");
    Ok(match t.split('/').count() {
        1 => format!("persistent://public/default/{t}"),
        3 => format!("persistent://{t}"),
        _ => bail!("topic must be a name or tenant/namespace/name"),
    })
}

/// Text for the model: UTF-8 as is, anything else as hex, and which it is.
pub fn show(payload: &[u8]) -> (String, &'static str) {
    match std::str::from_utf8(payload) {
        Ok(s)
            if !s
                .chars()
                .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) =>
        {
            (s.to_string(), "utf8")
        }
        _ => (hex::encode(payload), "hex"),
    }
}

/// Bytes from the model: `encoding` utf8 (default) or hex.
pub fn bytes(payload: &str, encoding: Option<&str>) -> Result<Vec<u8>> {
    match encoding.unwrap_or("utf8") {
        "utf8" => Ok(payload.as_bytes().to_vec()),
        "hex" => hex::decode(payload).context("payload is not hex"),
        other => bail!("encoding must be utf8 or hex, not {other}"),
    }
}

pub fn now_millis() -> u64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
