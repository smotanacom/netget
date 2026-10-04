//! Check protobuf structure before prost allocates repeated fields or nested messages.
use bytes::{Buf, BufMut};
use prost::Message;
use prost_reflect::{DescriptorPool, Kind, MessageDescriptor};
use std::{collections::HashMap, marker::PhantomData, sync::LazyLock};
use tonic::{
    codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder},
    Status,
};
pub const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
pub const MAX_VALUE_BYTES: usize = 64 * 1024;
pub const MAX_NODES: usize = 10_000;
pub const MAX_DEPTH: usize = 32;
static DESCRIPTORS: LazyLock<DescriptorPool> = LazyLock::new(|| {
    DescriptorPool::decode(
        include_bytes!(concat!(env!("OUT_DIR"), "/gnmi-descriptors.pb")).as_slice(),
    )
    .expect("immutable compiled OpenConfig descriptors")
});
pub trait WireName {
    const NAME: &'static str;
}
macro_rules! names {
    ($($name:ident),*) => { $(impl WireName for super::proto::gnmi::$name {
        const NAME: &'static str = concat!("gnmi.", stringify!($name));
    })* };
}
names!(
    CapabilityRequest,
    CapabilityResponse,
    GetRequest,
    GetResponse,
    SetRequest,
    SetResponse,
    SubscribeRequest,
    SubscribeResponse
);
pub struct BoundedProstCodec<T, U>(PhantomData<(T, U)>);
impl<T, U> Default for BoundedProstCodec<T, U> {
    fn default() -> Self {
        Self(PhantomData)
    }
}
pub struct BoundedEncoder<T>(PhantomData<T>);
pub struct BoundedDecoder<T>(PhantomData<T>);
impl<T, U> Codec for BoundedProstCodec<T, U>
where
    T: Message + WireName + Send + 'static,
    U: Message + WireName + Default + Send + 'static,
{
    type Encode = T;
    type Decode = U;
    type Encoder = BoundedEncoder<T>;
    type Decoder = BoundedDecoder<U>;
    fn encoder(&mut self) -> Self::Encoder {
        BoundedEncoder(PhantomData)
    }
    fn decoder(&mut self) -> Self::Decoder {
        BoundedDecoder(PhantomData)
    }
}
impl<T: Message + WireName> Encoder for BoundedEncoder<T> {
    type Item = T;
    type Error = Status;
    fn encode(&mut self, item: T, output: &mut EncodeBuf<'_>) -> Result<(), Status> {
        if item.encoded_len() > MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted("gNMI message exceeds 1 MiB"));
        }
        let bytes = item.encode_to_vec();
        validate_wire(T::NAME, &bytes)?;
        output.put_slice(&bytes);
        Ok(())
    }
}
impl<T: Message + Default + WireName> Decoder for BoundedDecoder<T> {
    type Item = T;
    type Error = Status;
    fn decode(&mut self, input: &mut DecodeBuf<'_>) -> Result<Option<T>, Status> {
        if input.remaining() > MAX_MESSAGE_BYTES {
            return Err(Status::resource_exhausted("gNMI message exceeds 1 MiB"));
        }
        let bytes = input.copy_to_bytes(input.remaining());
        validate_wire(T::NAME, &bytes)?;
        T::decode(bytes)
            .map(Some)
            .map_err(|_| Status::invalid_argument("invalid gNMI protobuf"))
    }
}
pub fn validate_wire(name: &str, bytes: &[u8]) -> Result<(), Status> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Status::resource_exhausted("gNMI message exceeds 1 MiB"));
    }
    let descriptor = DESCRIPTORS
        .get_message_by_name(name)
        .ok_or_else(|| Status::internal("unknown immutable gNMI descriptor"))?;
    scan(bytes, descriptor, 0, &mut 0)
}
fn varint(input: &mut &[u8]) -> Result<u64, Status> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = input
            .split_first()
            .ok_or_else(|| Status::invalid_argument("truncated protobuf varint"))?;
        *input = rest;
        if shift == 63 && byte > 1 {
            return Err(Status::invalid_argument("overflowing protobuf varint"));
        }
        value |= u64::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Ok(value);
        }
    }
    Err(Status::invalid_argument("invalid protobuf varint"))
}
fn take<'a>(input: &mut &'a [u8], length: usize) -> Result<&'a [u8], Status> {
    if length > input.len() {
        return Err(Status::invalid_argument("truncated protobuf field"));
    }
    let (value, rest) = input.split_at(length);
    *input = rest;
    Ok(value)
}
fn repeated_limit(message: &str, field: &str) -> usize {
    match (message, field) {
        ("gnmi.Path", "elem" | "element") => 32,
        ("gnmi.PathElem", "key") => 8,
        ("gnmi.ScalarArray", "element") => 64,
        ("gnmi.Notification", "update" | "delete") => 256,
        ("gnmi.GetResponse", "notification") => 128,
        ("gnmi.SetResponse", "response") => 256,
        _ => 128,
    }
}
fn field_limit(message: &str, field: &str) -> usize {
    match (message, field) {
        ("gnmi.Path", "origin" | "target") => 256,
        ("gnmi.Path", "element") | ("gnmi.PathElem", "name") => 128,
        ("gnmi.PathElem.KeyEntry", "key") => 128,
        ("gnmi.PathElem.KeyEntry", "value") => 1024,
        ("gnmi.ModelData", _) => 256,
        ("gnmi.Error", "message") => 4096,
        _ => MAX_VALUE_BYTES,
    }
}
fn scan(
    mut input: &[u8],
    descriptor: MessageDescriptor,
    depth: usize,
    nodes: &mut usize,
) -> Result<(), Status> {
    if depth > MAX_DEPTH {
        return Err(Status::resource_exhausted("protobuf nesting exceeds 32"));
    }
    let name = descriptor.full_name();
    let mut repeated = HashMap::<u32, usize>::new();
    while !input.is_empty() {
        *nodes += 1;
        if *nodes > MAX_NODES {
            return Err(Status::resource_exhausted(
                "protobuf/JSON nodes exceed 10000",
            ));
        }
        let key = varint(&mut input)?;
        let number = u32::try_from(key >> 3)
            .map_err(|_| Status::invalid_argument("protobuf field number overflow"))?;
        if number == 0 || number > 0x1fff_ffff {
            return Err(Status::invalid_argument("invalid protobuf field number"));
        }
        let wire = (key & 7) as u8;
        let field = descriptor.get_field(number);
        if let Some(field) = &field {
            if field.is_list() || field.is_map() {
                let count = repeated.entry(number).or_default();
                *count += 1;
                if *count > repeated_limit(name, field.name()) {
                    return Err(Status::resource_exhausted(
                        "protobuf repeated field exceeds bound",
                    ));
                }
            }
            if name == "gnmi.TypedValue" && matches!(number, 5 | 9 | 13)
                || name == "gnmi.Update" && number == 2
                || matches!(field.kind(),Kind::Message(ref child) if child.full_name()=="gnmi_ext.Extension" || child.full_name()=="google.protobuf.Any")
            {
                return Err(Status::unimplemented(
                    "opaque bytes, Any, legacy values and extensions are excluded",
                ));
            }
            let expected = match field.kind() {
                Kind::Message(_) | Kind::String | Kind::Bytes => 2,
                Kind::Double | Kind::Fixed64 | Kind::Sfixed64 => 1,
                Kind::Float | Kind::Fixed32 | Kind::Sfixed32 => 5,
                _ => 0,
            };
            if wire != expected && !(field.is_list() && field.is_packed() && wire == 2) {
                return Err(Status::invalid_argument(
                    "protobuf field has wrong wire type",
                ));
            }
        }
        match wire {
            0 => {
                varint(&mut input)?;
            }
            1 => {
                take(&mut input, 8)?;
            }
            5 => {
                take(&mut input, 4)?;
            }
            2 => {
                let length = usize::try_from(varint(&mut input)?)
                    .map_err(|_| Status::resource_exhausted("protobuf field length overflow"))?;
                let payload = take(&mut input, length)?;
                if let Some(field) = field {
                    match field.kind() {
                        Kind::Message(child) => scan(payload, child, depth + 1, nodes)?,
                        Kind::String | Kind::Bytes => {
                            if payload.len() > field_limit(name, field.name()) {
                                return Err(Status::resource_exhausted(
                                    "protobuf text/value field exceeds bound",
                                ));
                            }
                            if name == "gnmi.TypedValue" && matches!(number, 10 | 11) {
                                super::json::parse_with_budget(payload, nodes)?;
                            }
                        }
                        kind if field.is_list() && field.is_packed() => {
                            let mut packed = payload;
                            let mut count = 0usize;
                            while !packed.is_empty() {
                                match kind {
                                    Kind::Double | Kind::Fixed64 | Kind::Sfixed64 => {
                                        take(&mut packed, 8)?;
                                    }
                                    Kind::Float | Kind::Fixed32 | Kind::Sfixed32 => {
                                        take(&mut packed, 4)?;
                                    }
                                    _ => {
                                        varint(&mut packed)?;
                                    }
                                }
                                count += 1;
                                if count > repeated_limit(name, field.name()) {
                                    return Err(Status::resource_exhausted(
                                        "packed protobuf field exceeds bound",
                                    ));
                                }
                            }
                            let entries = repeated.entry(number).or_default();
                            *entries += count.saturating_sub(1);
                            *nodes += count;
                            if *entries > repeated_limit(name, field.name()) || *nodes > MAX_NODES {
                                return Err(Status::resource_exhausted(
                                    "packed protobuf count/node bound",
                                ));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {
                return Err(Status::invalid_argument(
                    "protobuf group or reserved wire type excluded",
                ))
            }
        }
    }
    Ok(())
}
