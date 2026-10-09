//! Real v1/v1alpha reflection using generated services and connection-owned stream futures.
use anyhow::{ensure, Result};
use prost::Message;
use prost_reflect::{DescriptorPool, FileDescriptor};
use std::{collections::HashSet, sync::Arc};

const MAX_FILES: usize = 128;
const MAX_QUERIES: usize = 128;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_DESCRIPTOR_BYTES: usize = 4 * 1024 * 1024;
// Descriptor bytes plus repeated-field framing and the original request/host echo.
const MAX_RESPONSE_BYTES: usize = 5 * 1024 * 1024;

pub(super) struct Reflection(DescriptorPool);
impl Reflection {
    pub fn new(pool: &DescriptorPool) -> Result<Arc<Self>> {
        let mut pool = pool.clone();
        for bytes in [
            tonic_reflection::pb::v1::FILE_DESCRIPTOR_SET,
            tonic_reflection::pb::v1alpha::FILE_DESCRIPTOR_SET,
        ] {
            let descriptors = prost_types::FileDescriptorSet::decode(bytes)?;
            for file in descriptors.file {
                if let Some(existing) = pool.get_file_by_name(file.name()) {
                    ensure!(
                        existing.file_descriptor_proto() == &file,
                        "startup schema replaces a reserved reflection descriptor"
                    );
                } else {
                    pool.add_file_descriptor_proto(file)?;
                }
            }
        }
        ensure!(
            pool.files().len() <= MAX_FILES,
            "reflection schema exceeds 128 files"
        );
        let bytes = pool
            .file_descriptor_protos()
            .map(Message::encoded_len)
            .sum::<usize>();
        ensure!(
            bytes <= MAX_DESCRIPTOR_BYTES,
            "reflection schema exceeds 4 MiB"
        );
        ensure!(
            pool.services().len() <= 128,
            "reflection schema exceeds 128 services"
        );
        Ok(Arc::new(Self(pool)))
    }
    fn file_for_symbol(&self, name: &str) -> Option<FileDescriptor> {
        if let Some(message) = self.0.get_message_by_name(name) {
            return Some(message.parent_file());
        }
        if let Some(service) = self.0.get_service_by_name(name) {
            return Some(service.parent_file());
        }
        if let Some(enumeration) = self.0.get_enum_by_name(name) {
            return Some(enumeration.parent_file());
        }
        if let Some(extension) = self.0.get_extension_by_name(name) {
            return Some(extension.parent_file());
        }
        // Protobuf enum values are siblings of their enum in the symbol namespace.
        for enumeration in self.0.all_enums() {
            if enumeration.values().any(|value| value.full_name() == name) {
                return Some(enumeration.parent_file());
            }
        }
        // grpcurl also asks about fully qualified method and field symbols.
        let (parent, child) = name.rsplit_once('.')?;
        if let Some(service) = self.0.get_service_by_name(parent) {
            if service.methods().any(|method| method.name() == child) {
                return Some(service.parent_file());
            }
        }
        if let Some(message) = self.0.get_message_by_name(parent) {
            if message.get_field_by_name(child).is_some()
                || message.oneofs().any(|oneof| oneof.name() == child)
            {
                return Some(message.parent_file());
            }
        }
        None
    }
    fn files(&self, root: FileDescriptor) -> Vec<Vec<u8>> {
        let mut seen = HashSet::new();
        let mut pending = vec![root];
        let mut files = Vec::new();
        while let Some(file) = pending.pop() {
            if seen.insert(file.name().to_owned()) {
                files.push(file.file_descriptor_proto().encode_to_vec());
                pending.extend(file.dependencies());
            }
        }
        files
    }
}

// Both generated protocols have the same query semantics. The stream itself polls
// the request; there is no spawned library worker to outlive a cancelled response.
macro_rules! reflection_version {
    ($module:ident, $version:ident) => {
        mod $module {
            use super::*;
            use futures::{Stream, StreamExt};
            use hyper::{body::Incoming, Request};
            use std::pin::Pin;
            use tonic::{codec::CompressionEncoding, Response, Status, Streaming};
            use tonic_reflection::pb::$version::{
                server_reflection_request::MessageRequest,
                server_reflection_response::MessageResponse,
                server_reflection_server::{ServerReflection, ServerReflectionServer},
                ErrorResponse, ExtensionNumberResponse, FileDescriptorResponse,
                ListServiceResponse, ServerReflectionRequest, ServerReflectionResponse,
                ServiceResponse,
            };
            use tower::Service;
            struct Handler(Arc<Reflection>, tokio::time::Instant);
            type Replies =
                Pin<Box<dyn Stream<Item = Result<ServerReflectionResponse, Status>> + Send>>;
            #[tonic::async_trait]
            impl ServerReflection for Handler {
                type ServerReflectionInfoStream = Replies;
                async fn server_reflection_info(
                    &self,
                    request: tonic::Request<Streaming<ServerReflectionRequest>>,
                ) -> Result<Response<Replies>, Status> {
                    let state = self.0.clone();
                    let deadline = self.1;
                    let replies = futures::stream::unfold(
                        (request.into_inner(), 0usize, false),
                        move |(mut requests, count, finished)| {
                            let state = state.clone();
                            async move {
                                if finished {
                                    return None;
                                }
                                let request = match tokio::time::timeout_at(
                                    deadline,
                                    requests.next(),
                                )
                                .await
                                {
                                    Ok(Some(Ok(request))) => request,
                                    Ok(None) => return None,
                                    Ok(Some(Err(error))) => {
                                        return Some((Err(error), (requests, count, true)))
                                    }
                                    Err(_) => {
                                        return Some((
                                            Err(Status::deadline_exceeded(
                                                "reflection deadline exceeded",
                                            )),
                                            (requests, count, true),
                                        ))
                                    }
                                };
                                if count >= MAX_QUERIES {
                                    return Some((
                                        Err(Status::resource_exhausted(
                                            "reflection exceeds 128 queries",
                                        )),
                                        (requests, count, true),
                                    ));
                                }
                                let files = |file: Option<FileDescriptor>| {
                                    file.map(|file| {
                                        MessageResponse::FileDescriptorResponse(
                                            FileDescriptorResponse {
                                                file_descriptor_proto: state.files(file),
                                            },
                                        )
                                    })
                                };
                                let reply = match &request.message_request {
                                    Some(MessageRequest::FileByFilename(name)) => {
                                        files(state.0.get_file_by_name(name))
                                    }
                                    Some(MessageRequest::FileContainingSymbol(name)) => {
                                        files(state.file_for_symbol(name))
                                    }
                                    Some(MessageRequest::ListServices(_)) => {
                                        Some(MessageResponse::ListServicesResponse(
                                            ListServiceResponse {
                                                service: state
                                                    .0
                                                    .services()
                                                    .map(|service| ServiceResponse {
                                                        name: service.full_name().to_owned(),
                                                    })
                                                    .collect(),
                                            },
                                        ))
                                    }
                                    Some(MessageRequest::AllExtensionNumbersOfType(name)) => {
                                        state.0.get_message_by_name(name).and_then(|message| {
                                            let numbers = message
                                                .extensions()
                                                .map(|extension| i32::try_from(extension.number()))
                                                .collect::<Result<Vec<_>, _>>()
                                                .ok()?;
                                            Some(MessageResponse::AllExtensionNumbersResponse(
                                                ExtensionNumberResponse {
                                                    base_type_name: name.clone(),
                                                    extension_number: numbers,
                                                },
                                            ))
                                        })
                                    }
                                    Some(MessageRequest::FileContainingExtension(extension)) => {
                                        files(
                                            state
                                                .0
                                                .get_message_by_name(&extension.containing_type)
                                                .and_then(|message| {
                                                    let number =
                                                        u32::try_from(extension.extension_number)
                                                            .ok()?;
                                                    message
                                                        .extensions()
                                                        .find(|candidate| {
                                                            candidate.number() == number
                                                        })
                                                        .map(|candidate| candidate.parent_file())
                                                }),
                                        )
                                    }
                                    None => None,
                                }
                                .unwrap_or_else(|| {
                                    MessageResponse::ErrorResponse(ErrorResponse {
                                        error_code: 5,
                                        error_message: "reflection symbol or file not found".into(),
                                    })
                                });
                                let response = ServerReflectionResponse {
                                    valid_host: request.host.clone(),
                                    original_request: Some(request),
                                    message_response: Some(reply),
                                };
                                Some((Ok(response), (requests, count + 1, false)))
                            }
                        },
                    );
                    Ok(Response::new(Box::pin(replies)))
                }
            }
            pub(super) async fn dispatch(
                request: Request<Incoming>,
                state: Arc<Reflection>,
                deadline: tokio::time::Instant,
            ) -> hyper::Response<tonic::body::BoxBody> {
                let mut service = ServerReflectionServer::new(Handler(state, deadline))
                    .max_decoding_message_size(MAX_REQUEST_BYTES)
                    .max_encoding_message_size(MAX_RESPONSE_BYTES)
                    .accept_compressed(CompressionEncoding::Gzip);
                match service.call(request).await {
                    Ok(response) => response,
                    Err(never) => match never {},
                }
            }
        }
    };
}
reflection_version!(v1, v1);
reflection_version!(v1alpha, v1alpha);

pub(super) async fn dispatch(
    request: hyper::Request<hyper::body::Incoming>,
    state: Arc<Reflection>,
    deadline: tokio::time::Instant,
) -> hyper::Response<tonic::body::BoxBody> {
    if request.uri().path() == "/grpc.reflection.v1.ServerReflection/ServerReflectionInfo" {
        v1::dispatch(request, state, deadline).await
    } else {
        v1alpha::dispatch(request, state, deadline).await
    }
}
