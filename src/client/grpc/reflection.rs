//! Bounded schema discovery; descriptors stay internal, never enter model events.
use anyhow::{ensure, Context, Result};
use prost::Message;
use prost_reflect::DescriptorPool;
use prost_types::{FileDescriptorProto, FileDescriptorSet};
use std::{collections::HashMap, time::Duration};
use tonic::transport::Channel;

const MAX_FILES: usize = 128;
const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_RESPONSE: usize = 5 * 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(10);

macro_rules! discover_version {
    ($name:ident, $version:ident) => {
        async fn $name(channel: Channel) -> Result<DescriptorPool> {
            use tonic_reflection::pb::$version::{
                server_reflection_client::ServerReflectionClient,
                server_reflection_request::MessageRequest,
                server_reflection_response::MessageResponse, ServerReflectionRequest,
            };
            let (sender, requests) = tokio::sync::mpsc::channel(1);
            sender
                .send(ServerReflectionRequest {
                    host: String::new(),
                    message_request: Some(MessageRequest::ListServices(String::new())),
                })
                .await?;
            let mut request =
                tonic::Request::new(tokio_stream::wrappers::ReceiverStream::new(requests));
            request.set_timeout(DEADLINE);
            let mut client = ServerReflectionClient::new(channel)
                .max_decoding_message_size(MAX_RESPONSE)
                .max_encoding_message_size(64 * 1024)
                .accept_compressed(tonic::codec::CompressionEncoding::Gzip);
            let mut replies = client.server_reflection_info(request).await?.into_inner();
            let response = replies
                .message()
                .await?
                .context("reflection ended before service list")?;
            let Some(MessageResponse::ListServicesResponse(list)) = response.message_response
            else {
                anyhow::bail!("reflection did not return service list");
            };
            ensure!(
                list.service.len() <= MAX_FILES,
                "reflection exceeds 128 services"
            );
            let mut files: HashMap<String, FileDescriptorProto> = HashMap::new();
            let mut total = 0usize;
            for service in list
                .service
                .into_iter()
                .filter(|service| !service.name.starts_with("grpc.reflection."))
            {
                ensure!(
                    service.name.len() <= 1024,
                    "reflection service name too long"
                );
                sender
                    .send(ServerReflectionRequest {
                        host: String::new(),
                        message_request: Some(MessageRequest::FileContainingSymbol(service.name)),
                    })
                    .await?;
                let response = replies
                    .message()
                    .await?
                    .context("reflection ended before descriptor response")?;
                let Some(MessageResponse::FileDescriptorResponse(response)) =
                    response.message_response
                else {
                    anyhow::bail!("reflection did not return descriptors");
                };
                ensure!(
                    response.file_descriptor_proto.len() <= MAX_FILES,
                    "reflection exceeds 128 files"
                );
                for bytes in response.file_descriptor_proto {
                    ensure!(
                        bytes.len() <= MAX_BYTES,
                        "reflection descriptor exceeds 4 MiB"
                    );
                    let file = FileDescriptorProto::decode(bytes.as_slice())?;
                    let name = file.name().to_owned();
                    if let Some(previous) = files.get(&name) {
                        ensure!(
                            previous == &file,
                            "reflection changed an immutable descriptor"
                        );
                    } else {
                        ensure!(files.len() < MAX_FILES, "reflection exceeds 128 files");
                        total = total
                            .checked_add(bytes.len())
                            .context("reflection size overflow")?;
                        ensure!(total <= MAX_BYTES, "reflection schema exceeds 4 MiB");
                        files.insert(name, file);
                    }
                }
            }
            drop(sender);
            ensure!(
                !files.is_empty(),
                "reflection returned no application schema"
            );
            crate::server::grpc::schema::pool_from_set(FileDescriptorSet {
                file: files.into_values().collect(),
            })
            .context("reflection descriptors do not form a valid schema")
        }
    };
}
discover_version!(v1, v1);
discover_version!(v1alpha, v1alpha);

pub(super) async fn discover(channel: Channel) -> Result<DescriptorPool> {
    tokio::time::timeout(DEADLINE, async {
        match v1(channel.clone()).await {
            Ok(pool) => Ok(pool),
            Err(error)
                if error
                    .downcast_ref::<tonic::Status>()
                    .is_some_and(|status| status.code() == tonic::Code::Unimplemented) =>
            {
                v1alpha(channel).await
            }
            Err(error) => Err(error),
        }
    })
    .await
    .context("reflection discovery deadline exceeded")?
}
