use crate::helpers::grpc_peer as peer;
use base64::Engine;
use prost::Message;
use prost_types::{
    field_descriptor_proto::{Label, Type},
    DescriptorProto, FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet,
    MethodDescriptorProto, ServiceDescriptorProto,
};
use serde_json::json;
const LIMIT: usize = 4 * 1024 * 1024;
fn set() -> FileDescriptorSet {
    FileDescriptorSet {
        file: vec![FileDescriptorProto {
            name: Some("bounded.proto".into()),
            package: Some("bounded".into()),
            syntax: Some("proto3".into()),
            message_type: vec![DescriptorProto {
                name: Some("Message".into()),
                ..Default::default()
            }],
            service: vec![ServiceDescriptorProto {
                name: Some("Session".into()),
                method: vec![MethodDescriptorProto {
                    name: Some("Watch".into()),
                    input_type: Some(".bounded.Message".into()),
                    output_type: Some(".bounded.Message".into()),
                    server_streaming: Some(true),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}
async fn start(
    state: &netget::state::AppState,
    id: netget::state::ServerId,
    set: &FileDescriptorSet,
    reflection: bool,
) -> anyhow::Result<std::net::SocketAddr> {
    use netget::llm::actions::protocol_trait::Protocol;
    let parameters=netget::protocol::StartupParams::new(json!({"proto_schema":base64::engine::general_purpose::STANDARD.encode(set.encode_to_vec()),"enable_reflection":reflection}),netget::server::grpc::actions::GrpcProtocol::new().get_startup_parameters()).unwrap();
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    netget::server::grpc::GrpcServer::spawn_with_llm_actions(
        "127.0.0.1:0".parse().unwrap(),
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        std::sync::Arc::new(state.clone()),
        tx,
        id,
        Some(parameters),
    )
    .await
}
#[tokio::test]
async fn startup_descriptor_bytes_files_and_reserved_reflection_are_bounded() {
    let state = peer::state().await;
    let (id, _) = peer::netget_server(&state, vec![], json!({}))
        .await
        .unwrap();
    let mut exact = set();
    exact.file[0].source_code_info = Some(prost_types::SourceCodeInfo {
        location: vec![prost_types::source_code_info::Location {
            leading_comments: Some("x".repeat(LIMIT - 1024)),
            ..Default::default()
        }],
    });
    let remaining = LIMIT - exact.encoded_len();
    exact.file[0].source_code_info.as_mut().unwrap().location[0]
        .leading_comments
        .as_mut()
        .unwrap()
        .extend(std::iter::repeat_n('x', remaining));
    assert_eq!(exact.encoded_len(), LIMIT);
    assert!(start(&state, id, &exact, false).await.is_ok());
    exact.file[0].source_code_info.as_mut().unwrap().location[0]
        .leading_comments
        .as_mut()
        .unwrap()
        .push('x');
    assert_eq!(exact.encoded_len(), LIMIT + 1);
    assert!(format!("{:#}", start(&state, id, &exact, false).await.unwrap_err()).contains("4 MiB"));
    let mut files = set();
    for index in 1..128 {
        files.file.push(FileDescriptorProto {
            name: Some(format!("empty{index}.proto")),
            syntax: Some("proto3".into()),
            ..Default::default()
        });
    }
    assert!(start(&state, id, &files, false).await.is_ok());
    files.file.push(FileDescriptorProto {
        name: Some("overflow.proto".into()),
        ..Default::default()
    });
    assert!(
        format!("{:#}", start(&state, id, &files, false).await.unwrap_err()).contains("128 files")
    );
    files.file.truncate(126);
    assert!(start(&state, id, &files, true).await.is_ok());
    files.file.push(FileDescriptorProto {
        name: Some("extra.proto".into()),
        ..Default::default()
    });
    assert!(
        format!("{:#}", start(&state, id, &files, true).await.unwrap_err())
            .contains("reflection schema exceeds 128 files")
    );
    let mut reserved = set();
    let mut canonical = FileDescriptorSet::decode(tonic_reflection::pb::v1::FILE_DESCRIPTOR_SET)
        .unwrap()
        .file
        .remove(0);
    canonical.source_code_info = Some(prost_types::SourceCodeInfo {
        location: vec![prost_types::source_code_info::Location {
            leading_comments: Some("modified reserved descriptor".into()),
            ..Default::default()
        }],
    });
    reserved.file.push(canonical);
    assert!(format!(
        "{:#}",
        start(&state, id, &reserved, true).await.unwrap_err()
    )
    .contains("reserved reflection descriptor"));
    state.remove_server(id).await.unwrap();
}
#[tokio::test]
async fn startup_schema_name_expansion_and_nesting_are_checked_before_pool_creation() {
    let state = peer::state().await;
    let (id, _) = peer::netget_server(&state, vec![], json!({}))
        .await
        .unwrap();
    let mut nested = set();
    let mut current = DescriptorProto {
        name: Some("N".into()),
        ..Default::default()
    };
    for _ in 0..32 {
        current = DescriptorProto {
            name: Some("N".into()),
            nested_type: vec![current],
            ..Default::default()
        };
    }
    nested.file[0].message_type[0].nested_type = current.nested_type.clone();
    assert!(start(&state, id, &nested, false).await.is_ok());
    nested.file[0].message_type[0].nested_type = vec![current];
    assert!(
        format!("{:#}", start(&state, id, &nested, false).await.unwrap_err()).contains("32 levels")
    );
    let mut amplification = set();
    amplification.file[0].package = Some("p".repeat(257));
    assert!(format!(
        "{:#}",
        start(&state, id, &amplification, false).await.unwrap_err()
    )
    .contains("filename/package too long"));
    let mut fields = set();
    fields.file[0].message_type[0].field = (1..=100_000)
        .map(|number| FieldDescriptorProto {
            name: Some(format!("f{number}")),
            number: Some(number),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Int32 as i32),
            ..Default::default()
        })
        .collect();
    assert!(
        format!("{:#}", start(&state, id, &fields, false).await.unwrap_err())
            .contains("100000 nodes/8 MiB")
    );
    state.remove_server(id).await.unwrap();
}
#[test]
fn typed_stream_schema_excludes_nested_bytes_and_caps_message_fields() {
    let mut descriptors = set();
    descriptors.file[0].message_type[0].field = (1..=128)
        .map(|number| FieldDescriptorProto {
            name: Some(format!("f{number}")),
            number: Some(number),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Int32 as i32),
            ..Default::default()
        })
        .collect();
    let pool =
        prost_reflect::DescriptorPool::from_file_descriptor_set(descriptors.clone()).unwrap();
    assert!(netget::server::grpc::stream_codec::check_descriptor(
        &pool.get_message_by_name("bounded.Message").unwrap()
    )
    .is_ok());
    descriptors.file[0].message_type[0]
        .field
        .push(FieldDescriptorProto {
            name: Some("extra".into()),
            number: Some(129),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Int32 as i32),
            ..Default::default()
        });
    let pool = prost_reflect::DescriptorPool::from_file_descriptor_set(descriptors).unwrap();
    assert!(netget::server::grpc::stream_codec::check_descriptor(
        &pool.get_message_by_name("bounded.Message").unwrap()
    )
    .is_err());
    let mut bytes = set();
    bytes.file[0].message_type.push(DescriptorProto {
        name: Some("Child".into()),
        field: vec![FieldDescriptorProto {
            name: Some("encoded".into()),
            number: Some(1),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Bytes as i32),
            ..Default::default()
        }],
        ..Default::default()
    });
    bytes.file[0].message_type[0]
        .field
        .push(FieldDescriptorProto {
            name: Some("child".into()),
            number: Some(1),
            label: Some(Label::Optional as i32),
            r#type: Some(Type::Message as i32),
            type_name: Some(".bounded.Child".into()),
            ..Default::default()
        });
    let pool = prost_reflect::DescriptorPool::from_file_descriptor_set(bytes).unwrap();
    let descriptor = pool.get_message_by_name("bounded.Message").unwrap();
    assert!(
        netget::server::grpc::stream_codec::from_json(&json!({}), &descriptor)
            .unwrap_err()
            .to_string()
            .contains("bytes fields")
    );
}
