//! Library-owned framing with the audited field-name JSON conversion for streaming RPCs.
use anyhow::{bail, ensure, Result};
use bytes::Buf;
use prost::Message;
use prost_reflect::{DynamicMessage, MessageDescriptor, ReflectMessage, Value};
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::Status;

pub const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

/// Exclude bytes fields, including nested/map values, before JSON conversion.
pub fn check_descriptor(descriptor: &MessageDescriptor) -> Result<()> {
    let mut pending = vec![descriptor.clone()];
    let mut visited = std::collections::HashSet::new();
    while let Some(message) = pending.pop() {
        if !visited.insert(message.full_name().to_owned()) {
            continue;
        }
        ensure!(visited.len() <= 100_000, "stream schema exceeds type limit");
        ensure!(
            message.full_name().len() <= 1024 && message.fields().len() <= 128,
            "stream message schema exceeds name/field bound"
        );
        for field in message.fields() {
            ensure!(
                field.name().len() <= 256,
                "stream field name exceeds 256 bytes"
            );
            match field.kind() {
                prost_reflect::Kind::Bytes => {
                    bail!("bytes fields are outside the typed streaming subset")
                }
                prost_reflect::Kind::Message(child) => pending.push(child),
                _ => {}
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct DynamicCodec {
    pub encode: MessageDescriptor,
    pub decode: MessageDescriptor,
}
pub struct DynamicEncoder(MessageDescriptor);
pub struct DynamicDecoder(MessageDescriptor);
impl Codec for DynamicCodec {
    type Encode = DynamicMessage;
    type Decode = DynamicMessage;
    type Encoder = DynamicEncoder;
    type Decoder = DynamicDecoder;
    fn encoder(&mut self) -> Self::Encoder {
        DynamicEncoder(self.encode.clone())
    }
    fn decoder(&mut self) -> Self::Decoder {
        DynamicDecoder(self.decode.clone())
    }
}
impl Encoder for DynamicEncoder {
    type Item = DynamicMessage;
    type Error = Status;
    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        if item.descriptor() != self.0 {
            return Err(Status::invalid_argument(
                "stream message type does not match method",
            ));
        }
        if item.encoded_len() > MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted("stream message exceeds 4 MiB"));
        }
        item.encode(dst)
            .map_err(|_| Status::internal("stream message encoding failed"))
    }
}
impl Decoder for DynamicDecoder {
    type Item = DynamicMessage;
    type Error = Status;
    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        if src.remaining() > MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted("stream message exceeds 4 MiB"));
        }
        DynamicMessage::decode(self.0.clone(), src)
            .map(Some)
            .map_err(|_| Status::invalid_argument("stream message does not decode"))
    }
}

/// New streaming actions never accept or expose encoded protobuf bytes fields.
/// Legacy unary conversion remains unchanged. Walk borrowed values with the same
/// depth/node guards before converting them to model-visible JSON.
fn check_value(value: &Value, depth: usize, nodes: &mut usize) -> Result<()> {
    use super::value_codec::{ValueLimitExceeded, MAX_VALUE_DEPTH, MAX_VALUE_NODES};
    if depth > MAX_VALUE_DEPTH {
        return Err(ValueLimitExceeded {
            dimension: "depth",
            limit: MAX_VALUE_DEPTH,
        }
        .into());
    }
    if *nodes >= MAX_VALUE_NODES {
        return Err(ValueLimitExceeded {
            dimension: "nodes",
            limit: MAX_VALUE_NODES,
        }
        .into());
    }
    *nodes += 1;
    match value {
        Value::Bytes(_) => bail!("bytes fields are outside the typed streaming subset"),
        Value::Message(message) => {
            for (_, value) in message.fields() {
                check_value(value, depth + 1, nodes)?;
            }
        }
        Value::List(values) => {
            for value in values {
                check_value(value, depth + 1, nodes)?;
            }
        }
        Value::Map(values) => {
            for value in values.values() {
                check_value(value, depth + 1, nodes)?;
            }
        }
        _ => {}
    }
    Ok(())
}
fn check_message(message: &DynamicMessage) -> Result<()> {
    let mut nodes = 1;
    for (_, value) in message.fields() {
        check_value(value, 1, &mut nodes)?;
    }
    Ok(())
}
pub fn to_json(message: &DynamicMessage) -> Result<serde_json::Value> {
    check_message(message)?;
    super::value_codec::dynamic_message_to_json(message)
}
pub fn from_json(
    json: &serde_json::Value,
    descriptor: &MessageDescriptor,
) -> Result<DynamicMessage> {
    check_descriptor(descriptor)?;
    let message = super::value_codec::json_to_dynamic_message(json, descriptor)?;
    ensure!(
        message.encoded_len() <= MAX_MESSAGE_BYTES,
        "stream message exceeds 4 MiB"
    );
    check_message(&message)?;
    Ok(message)
}
