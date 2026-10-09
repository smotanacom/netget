"""Pinned OpenConfig v0.14.1 generated public grpcio SDK peer, both wire roles.

The deterministic fixture is a public SDK service, not a device/YANG datastore.
The OpenConfig fake Agent only implements Subscribe and cannot prove other RPCs.
"""
import argparse
from concurrent import futures
import hashlib
import importlib.metadata
import json
import pathlib
import queue
import sys
import tempfile
import threading
import time

import grpc
import grpc_tools
from grpc_tools import protoc


def varint(value):
    result = bytearray()
    while value > 127:
        result.append((value & 127) | 128)
        value >>= 7
    result.append(value)
    return bytes(result)


def sized_message(length):
    # Unknown length-delimited field 999, retained by the independent protobuf runtime.
    for payload in range(length - 8, length):
        value = varint(999 << 3 | 2) + varint(payload) + b'x' * payload
        if len(value) == length:
            return value
    raise AssertionError("unrepresentable fixture size")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--server', action='store_true')
    parser.add_argument('--target')
    parser.add_argument('--proto-root', required=True)
    parser.add_argument('--scenario', default='normal')
    parser.add_argument('--cert')
    parser.add_argument('--key')
    args = parser.parse_args()
    for package, version in [('grpcio', '1.75.1'), ('grpcio-tools', '1.75.1'), ('protobuf', '6.32.1')]:
        assert importlib.metadata.version(package) == version, package
    root = pathlib.Path(args.proto_root)
    schemas = [('gnmi', '45abf90bfee289544e2430c8ce1b5b10e4f851736a508ddba54d3cf12e82f7b7'),
               ('gnmi_ext', 'b0e96bd0c540cf249512783ee5abeb3f0d05805115f18c7d714ac90316981e2e')]
    inputs = []
    for name, digest in schemas:
        file = root / f'github.com/openconfig/gnmi/proto/{name}/{name}.proto'
        assert hashlib.sha256(file.read_bytes()).hexdigest() == digest
        inputs.append(str(file))
    with tempfile.TemporaryDirectory(prefix='netget-gnmi-generated-') as generated:
        assert protoc.main(['grpc_tools.protoc', f'-I{root}',
                            f'-I{pathlib.Path(grpc_tools.__file__).parent / "_proto"}',
                            f'--python_out={generated}', f'--grpc_python_out={generated}', *inputs]) == 0
        sys.path.insert(0, generated)
        from github.com.openconfig.gnmi.proto.gnmi import gnmi_pb2 as pb
        from github.com.openconfig.gnmi.proto.gnmi import gnmi_pb2_grpc as service

        def path(name='system'):
            return pb.Path(origin='openconfig', elem=[pb.PathElem(name=name, key={'name': 'eth0'})])

        def notification(encoding=pb.PROTO, index=42):
            if encoding == pb.JSON:
                value = pb.TypedValue(json_val=json.dumps({'counter': index}).encode())
            elif encoding == pb.JSON_IETF:
                value = pb.TypedValue(json_ietf_val=json.dumps({'counter': index}).encode())
            elif encoding == pb.ASCII:
                value = pb.TypedValue(ascii_val=f'value-{index}')
            else:
                value = pb.TypedValue(uint_val=index)
            return pb.Notification(timestamp=123456789, prefix=pb.Path(target='fixture'),
                                   update=[pb.Update(path=path(), val=value)])

        class Target(service.gNMIServicer):
            def Capabilities(self, request, context):
                return pb.CapabilityResponse(supported_models=[pb.ModelData(name='fixture', organization='OpenConfig', version='1')],
                                             supported_encodings=[pb.PROTO, pb.JSON, pb.JSON_IETF, pb.ASCII], gNMI_version='0.10.0')

            def Get(self, request, context):
                name = request.path[0].elem[0].name if request.path and request.path[0].elem else ''
                if name == 'denied':
                    context.abort(grpc.StatusCode.PERMISSION_DENIED, 'denied: fixture')
                if name == 'parked':
                    while context.is_active():
                        time.sleep(0.01)
                    return pb.GetResponse()
                if name == 'message-bound':
                    length = int(request.path[0].elem[0].key['size'])
                    response = pb.GetResponse()
                    response.ParseFromString(sized_message(length))
                    return response
                if name == 'json-bound':
                    length = int(request.path[0].elem[0].key['size'])
                    n = pb.Notification(update=[pb.Update(val=pb.TypedValue(json_val=b'"' + b'x' * (length - 2) + b'"'))])
                    return pb.GetResponse(notification=[n])
                if name == 'opaque':
                    return pb.GetResponse(notification=[pb.Notification(update=[pb.Update(val=pb.TypedValue(bytes_val=b'opaque'))])])
                if name == 'nonfinite':
                    return pb.GetResponse(notification=[pb.Notification(update=[pb.Update(val=pb.TypedValue(double_val=float('inf')))])])
                return pb.GetResponse(notification=[notification(request.encoding)])

            def Set(self, request, context):
                responses = []
                for paths, operation in [(request.delete, pb.UpdateResult.DELETE),
                                         ([u.path for u in request.replace], pb.UpdateResult.REPLACE),
                                         ([u.path for u in request.update], pb.UpdateResult.UPDATE)]:
                    responses.extend(pb.UpdateResult(path=p, op=operation) for p in paths)
                return pb.SetResponse(prefix=request.prefix, response=responses, timestamp=123456789)

            def Subscribe(self, requests, context):
                first = next(requests)
                if not first.HasField('subscribe'):
                    context.abort(grpc.StatusCode.INVALID_ARGUMENT, 'list required')
                selected = first.subscribe
                name = selected.subscription[0].path.elem[0].name
                def cycle(index):
                    if not selected.updates_only:
                        yield pb.SubscribeResponse(update=notification(selected.encoding, index))
                    if name != 'missing-sync':
                        yield pb.SubscribeResponse(sync_response=True)
                yield from cycle(42)
                if name == 'duplicate-sync':
                    yield pb.SubscribeResponse(sync_response=True)
                    return
                if selected.mode == pb.SubscriptionList.ONCE:
                    return
                if selected.mode == pb.SubscriptionList.POLL:
                    for request in requests:
                        if not request.HasField('poll'):
                            context.abort(grpc.StatusCode.INVALID_ARGUMENT, 'poll required')
                        yield from cycle(43)
                    return
                index = 43
                while context.is_active():
                    yield pb.SubscribeResponse(update=notification(selected.encoding, index))
                    if name != 'live' and (name != 'response-count' and index >= 45 or name == 'response-count' and index >= 300):
                        return
                    index += 1
                    time.sleep(0.005)

        options = [('grpc.max_receive_message_length', 2 * 1024 * 1024),
                   ('grpc.max_send_message_length', 2 * 1024 * 1024), ('grpc.max_concurrent_streams', 16)]
        if args.server:
            server = grpc.server(futures.ThreadPoolExecutor(max_workers=16), options=options)
            service.add_gNMIServicer_to_server(Target(), server)
            if args.cert:
                credentials = grpc.ssl_server_credentials([(pathlib.Path(args.key).read_bytes(), pathlib.Path(args.cert).read_bytes())])
                port = server.add_secure_port('127.0.0.1:0', credentials)
            else:
                port = server.add_insecure_port('127.0.0.1:0')
            assert port
            server.start()
            print(json.dumps({'port': port, 'grpcio': grpc.__version__, 'schema': 'v0.14.1'}), flush=True)
            server.wait_for_termination()
            return
        options += [('grpc.ssl_target_name_override', 'localhost')]
        channel = grpc.secure_channel(args.target, grpc.ssl_channel_credentials(root_certificates=pathlib.Path(args.cert).read_bytes()), options=options) if args.cert else grpc.insecure_channel(args.target, options=options)
        grpc.channel_ready_future(channel).result(timeout=5)
        stub = service.gNMIStub(channel)
        if args.scenario == 'normal':
            cap = stub.Capabilities(pb.CapabilityRequest(), timeout=5, compression=grpc.Compression.Gzip)
            assert cap.gNMI_version == '0.10.0' and pb.PROTO in cap.supported_encodings
            get = stub.Get(pb.GetRequest(path=[path()], encoding=pb.PROTO, type=pb.GetRequest.STATE), timeout=5)
            assert get.notification[0].update[0].val.uint_val == 42
            changes = pb.SetRequest(prefix=pb.Path(target='fixture'), delete=[path('old')],
                                    replace=[pb.Update(path=path('new'), val=pb.TypedValue(string_val='updated'))],
                                    update=[pb.Update(path=path('counter'), val=pb.TypedValue(uint_val=2**64-1))])
            result = stub.Set(changes, timeout=5, compression=grpc.Compression.Gzip)
            assert [r.op for r in result.response] == [1, 2, 3]
            assert [r.path for r in result.response] == [changes.delete[0], changes.replace[0].path, changes.update[0].path]
            print(json.dumps({'version': cap.gNMI_version, 'counter': 42, 'operations': [r.op for r in result.response]}))
        elif args.scenario in ['once', 'poll', 'stream', 'updates-only']:
            mode = {'once': 1, 'poll': 2, 'stream': 0, 'updates-only': 1}[args.scenario]
            inputs = queue.Queue(maxsize=1)
            stop = threading.Event()
            inputs.put(pb.SubscribeRequest(subscribe=pb.SubscriptionList(mode=mode, encoding=pb.PROTO,
                updates_only=args.scenario == 'updates-only', subscription=[pb.Subscription(path=path())])))
            def requests():
                while not stop.is_set():
                    try:
                        yield inputs.get(timeout=0.05)
                    except queue.Empty:
                        continue
            call = stub.Subscribe(requests(), timeout=8, compression=grpc.Compression.Gzip)
            kinds = []
            try:
                for response in call:
                    kinds.append(response.WhichOneof('response'))
                    if mode == 2 and kinds.count('sync_response') == 1:
                        inputs.put(pb.SubscribeRequest(poll=pb.Poll()), timeout=1)
                    if mode == 2 and kinds.count('sync_response') == 2:
                        call.cancel()
                        break
            finally:
                stop.set()
                call.cancel()
            assert kinds.count('sync_response') == (2 if mode == 2 else 1), kinds
            print(json.dumps({'kinds': kinds}))
        elif args.scenario == 'error':
            try:
                stub.Get(pb.GetRequest(path=[path('denied')], encoding=pb.PROTO), timeout=5)
                raise AssertionError('expected refusal')
            except grpc.RpcError as error:
                assert error.code() == grpc.StatusCode.PERMISSION_DENIED
                print(json.dumps({'code': error.code().value[0], 'message': error.details()}))
        else:
            raise AssertionError('unknown scenario')
        channel.close()


if __name__ == '__main__':
    main()
