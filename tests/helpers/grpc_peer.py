"""Independent grpcio 1.75.1 wire peer; generated protobuf, no local framing."""
import argparse
from concurrent import futures
import importlib.metadata
import json
import pathlib
import sys
import tempfile
import time

import grpc
from grpc_tools import protoc
from grpc_reflection.v1alpha import reflection, reflection_pb2, reflection_pb2_grpc


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--server", action="store_true")
    parser.add_argument("--target")
    parser.add_argument("--cert")
    parser.add_argument("--key")
    parser.add_argument("--schema", required=True)
    parser.add_argument("--scenario", default="streaming")
    parser.add_argument("--reflection-mode")
    args = parser.parse_args()
    for package in ("grpcio", "grpcio-tools", "grpcio-reflection"):
        assert importlib.metadata.version(package) == "1.75.1", package
    schema = pathlib.Path(args.schema)
    with tempfile.TemporaryDirectory(prefix="netget-grpcio-") as generated:
        assert protoc.main([
            "grpc_tools.protoc", f"-I{schema.parent}",
            f"--python_out={generated}", f"--grpc_python_out={generated}", str(schema),
        ]) == 0
        sys.path.insert(0, generated)
        import grpc_streams_pb2 as pb
        import grpc_streams_pb2_grpc as services

        class Session(services.SessionServicer):
            def Echo(self, request, context):
                return request

            def Watch(self, request, context):
                if request.name == "response-bound":
                    yield pb.Message(name="r" * (4 * 1024 * 1024 - 5 + request.value))
                elif request.name == "response-count":
                    for index in range(257): yield pb.Message(value=index)
                elif list(request.tags) == ["request-bound"]:
                    yield pb.Message(value=len(request.name))
                elif request.name == "subscription":
                    index = 0
                    while context.is_active():
                        yield pb.Message(name="subscription", value=index)
                        index += 1
                        time.sleep(0.02)
                else:
                    for index in range(3):
                        yield pb.Message(name=f"{request.name}-{index}", value=index,
                                         tags=list(request.tags), counts=dict(request.counts))

            def Collect(self, requests, context):
                count = 0
                total = 0
                for request in requests:
                    count += 1
                    total += request.value
                return pb.Message(name="collected", value=total, counts={"messages": count})

            def Chat(self, requests, context):
                for request in requests:
                    yield pb.Message(name=request.name, value=request.value + 1,
                                     tags=list(request.tags), counts=dict(request.counts))

        class BoundedProbe(reflection.ReflectionServicer):
            def ServerReflectionInfo(self, requests, context):
                from google.protobuf import descriptor_pb2
                for request in requests:
                    if request.HasField("list_services"):
                        names=["streams.Session"]
                        if args.reflection_mode == "changed-duplicate": names.append("streams.Other")
                        yield reflection_pb2.ServerReflectionResponse(original_request=request,
                            list_services_response=reflection_pb2.ListServiceResponse(service=[reflection_pb2.ServiceResponse(name=name) for name in names]))
                    else:
                        file=descriptor_pb2.FileDescriptorProto.FromString(pb.DESCRIPTOR.serialized_pb)
                        if args.reflection_mode == "oversized-descriptor": file.source_code_info.location.add(leading_comments="x"*(4*1024*1024))
                        elif args.reflection_mode == "name-expansion": file.package="p"*257
                        elif args.reflection_mode == "changed-duplicate" and request.file_containing_symbol == "streams.Other": file.source_code_info.location.add(leading_comments="changed")
                        files=[file.SerializeToString()]
                        if args.reflection_mode == "too-many-files": files.extend(descriptor_pb2.FileDescriptorProto(name=f"empty{i}.proto").SerializeToString() for i in range(128))
                        yield reflection_pb2.ServerReflectionResponse(original_request=request,
                            file_descriptor_response=reflection_pb2.FileDescriptorResponse(file_descriptor_proto=files))

        if args.server:
            server = grpc.server(futures.ThreadPoolExecutor(max_workers=16), options=[
                ("grpc.max_receive_message_length", 4 * 1024 * 1024),
                ("grpc.max_send_message_length", 5 * 1024 * 1024 if args.reflection_mode else 4 * 1024 * 1024 + 1),
                ("grpc.max_concurrent_streams", 64),
            ])
            services.add_SessionServicer_to_server(Session(), server)
            if args.reflection_mode:
                reflection_pb2_grpc.add_ServerReflectionServicer_to_server(BoundedProbe(("streams.Session",)), server)
            else:
                reflection.enable_server_reflection(("streams.Session", reflection.SERVICE_NAME), server)
            if args.cert:
                credentials = grpc.ssl_server_credentials([(pathlib.Path(args.key).read_bytes(), pathlib.Path(args.cert).read_bytes())])
                port = server.add_secure_port("127.0.0.1:0", credentials)
            else:
                port = server.add_insecure_port("127.0.0.1:0")
            assert port > 0
            server.start()
            print(json.dumps({"port": port, "grpcio": grpc.__version__}), flush=True)
            server.wait_for_termination()
            return

        assert args.target
        with grpc.insecure_channel(args.target, options=[
            ("grpc.max_receive_message_length", 5 * 1024 * 1024),
            ("grpc.max_send_message_length", 4 * 1024 * 1024 + 1),
        ]) as channel:
            grpc.channel_ready_future(channel).result(timeout=5)
            client = services.SessionStub(channel)
            if args.scenario == "request-bounds":
                for compression in (grpc.Compression.NoCompression, grpc.Compression.Gzip):
                    exact = pb.Message(name="r" * (4 * 1024 * 1024 - 5))
                    assert exact.ByteSize() == 4 * 1024 * 1024
                    response = list(client.Watch(exact, timeout=10, compression=compression))
                    assert response[0].value == len(exact.name)
                    exact.name += "r"
                    assert exact.ByteSize() == 4 * 1024 * 1024 + 1
                    try:
                        list(client.Watch(exact, timeout=10, compression=compression))
                        raise AssertionError("oversized request accepted")
                    except grpc.RpcError as error:
                        assert error.code() == grpc.StatusCode.RESOURCE_EXHAUSTED, error
                response = list(client.Watch(pb.Message(name="small"), timeout=10))
                assert response[0].value == 5
                print(json.dumps({"exact": 4194304, "overflow": 4194305, "gzip": True, "recovered": True}), flush=True)
            elif args.scenario == "reflection":
                queries = [
                    reflection_pb2.ServerReflectionRequest(list_services=""),
                    reflection_pb2.ServerReflectionRequest(file_containing_symbol="streams.Session.Watch"),
                    reflection_pb2.ServerReflectionRequest(file_by_filename="missing.proto"),
                    reflection_pb2.ServerReflectionRequest(file_containing_symbol="streams.Message"),
                ]
                replies = list(reflection_pb2_grpc.ServerReflectionStub(channel)
                               .ServerReflectionInfo(iter(queries), timeout=10))
                assert len(replies) == 4
                assert "streams.Session" in [s.name for s in replies[0].list_services_response.service]
                assert replies[1].file_descriptor_response.file_descriptor_proto
                assert replies[2].error_response.error_code == 5
                assert replies[3].file_descriptor_response.file_descriptor_proto
                print(json.dumps({"queries": len(replies), "unknown_code": 5}), flush=True)
            elif args.scenario == "cancel":
                call = client.Watch(pb.Message(name="subscription"), timeout=10)
                first = next(call)
                assert call.cancel()
                print(json.dumps({"name": first.name, "cancelled": call.cancelled()}), flush=True)
            else:
                request = pb.Message(name="watch", tags=["blue", "green"], counts={"copies": 2})
                watch = list(client.Watch(request, timeout=10, compression=grpc.Compression.Gzip))
                assert [(item.name, item.value) for item in watch] == [(f"watch-{i}", i) for i in range(3)]
                collected = client.Collect(iter([pb.Message(value=2), pb.Message(value=3)]), timeout=10)
                assert collected.value == 5 and collected.counts["messages"] == 2
                chat = list(client.Chat(iter([pb.Message(name="one", value=4), pb.Message(name="two", value=8)]), timeout=10))
                assert [(item.name, item.value) for item in chat] == [("one", 5), ("two", 9)]
                print(json.dumps({"watch": len(watch), "collected": collected.value, "chat": len(chat)}), flush=True)


if __name__ == "__main__":
    main()
