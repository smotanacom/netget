"""Independent Thrift peers for NetGet's tests, unchanged libraries driven from here:

  thriftpy2-call IDL PORT TRANSPORT PROTOCOL  thriftpy2 client, IDL-driven calls; JSON per line
  apache-call PORT TRANSPORT PROTOCOL         Apache Thrift's own codecs, no generated code;
                                              results read generically; JSON per line
  serve IDL PORT TRANSPORT PROTOCOL           thriftpy2 server for the Users service

TRANSPORT is framed or buffered; PROTOCOL is binary or compact.
"""
import json, sys


def out(**kw):
    print(json.dumps(kw), flush=True)


def tp2_factories(transport, protocol):
    from thriftpy2.protocol import TBinaryProtocolFactory, TCompactProtocolFactory
    from thriftpy2.transport import TBufferedTransportFactory, TFramedTransportFactory
    proto = TBinaryProtocolFactory() if protocol == "binary" else TCompactProtocolFactory()
    trans = TFramedTransportFactory() if transport == "framed" else TBufferedTransportFactory()
    return proto, trans


def user_json(u):
    return {"id": u.id, "name": u.name, "role": u.role, "tags": u.tags}


def thriftpy2_call(idl, port, transport, protocol):
    import thriftpy2
    from thriftpy2.rpc import make_client
    from thriftpy2.thrift import TApplicationException
    users = thriftpy2.load(idl, module_name="users_thrift")
    proto, trans = tp2_factories(transport, protocol)
    c = make_client(users.Users, "127.0.0.1", int(port), proto_factory=proto, trans_factory=trans, timeout=20000)
    out(call="add", result=c.add(40, 2))
    out(call="get_user", result=user_json(c.get_user(7)))
    try:
        c.get_user(404)
        out(call="get_user_missing", result="no exception")
    except users.NotFound as e:
        out(call="get_user_missing", exception="NotFound", message=e.message, id=e.id)
    found = c.find("A", {users.Role.ADMIN, users.Role.USER})
    out(call="find", result=[user_json(u) for u in found])
    out(call="touch", result=c.touch(users.User(id=9, name="Grace", role=users.Role.USER, tags=["navy"])))
    c.ping("hello")
    try:
        c.add(1, 1000)
        out(call="add_refused", result="no exception")
    except TApplicationException as e:
        out(call="add_refused", app_error=e.type, message=e.message)
    out(call="add_after", result=c.add(1, 2))
    c.close()


def apache_call(port, transport, protocol):
    from thrift.protocol import TBinaryProtocol, TCompactProtocol
    from thrift.Thrift import TApplicationException, TMessageType, TType
    from thrift.transport import TSocket, TTransport
    sock = TSocket.TSocket("127.0.0.1", int(port))
    sock.setTimeout(20000)
    t = TTransport.TFramedTransport(sock) if transport == "framed" else TTransport.TBufferedTransport(sock)
    p = TBinaryProtocol.TBinaryProtocol(t) if protocol == "binary" else TCompactProtocol.TCompactProtocol(t)
    t.open()

    def read(ttype):
        if ttype == TType.STRUCT:
            p.readStructBegin(); fields = {}
            while True:
                _, ft, fid = p.readFieldBegin()
                if ft == TType.STOP:
                    break
                fields[str(fid)] = read(ft); p.readFieldEnd()
            p.readStructEnd(); return fields
        if ttype in (TType.LIST, TType.SET):
            et, n = p.readListBegin() if ttype == TType.LIST else p.readSetBegin()
            v = [read(et) for _ in range(n)]
            p.readListEnd() if ttype == TType.LIST else p.readSetEnd(); return v
        if ttype == TType.MAP:
            kt, vt, n = p.readMapBegin(); v = [[read(kt), read(vt)] for _ in range(n)]; p.readMapEnd(); return v
        return {TType.BOOL: p.readBool, TType.BYTE: p.readByte, TType.I16: p.readI16, TType.I32: p.readI32,
                TType.I64: p.readI64, TType.DOUBLE: p.readDouble, TType.STRING: p.readString}[ttype]()

    seq = [0]

    def call(name, args, kind=TMessageType.CALL):
        seq[0] += 1
        p.writeMessageBegin(name, kind, seq[0]); p.writeStructBegin("args")
        for fid, ft, write in args:
            p.writeFieldBegin("f", ft, fid); write(); p.writeFieldEnd()
        p.writeFieldStop(); p.writeStructEnd(); p.writeMessageEnd(); t.flush()
        if kind == TMessageType.ONEWAY:
            return
        rname, rkind, rseq = p.readMessageBegin()
        assert (rname, rseq) == (name, seq[0]), (rname, rseq, name, seq[0])
        if rkind == TMessageType.EXCEPTION:
            x = TApplicationException(); x.read(p); p.readMessageEnd()
            out(call=name, app_error=x.type, message=x.message); return
        result = read(TType.STRUCT); p.readMessageEnd()
        out(call=name, result=result)

    call("add", [(1, TType.I32, lambda: p.writeI32(40)), (2, TType.I32, lambda: p.writeI32(2))])
    call("get_user", [(1, TType.I64, lambda: p.writeI64(7))])
    call("get_user", [(1, TType.I64, lambda: p.writeI64(404))])

    def roles():
        p.writeSetBegin(TType.I32, 1); p.writeI32(2); p.writeSetEnd()
    call("find", [(1, TType.STRING, lambda: p.writeString("B")), (2, TType.SET, roles)])
    call("ping", [(1, TType.STRING, lambda: p.writeString("apache"))], TMessageType.ONEWAY)
    call("delete_everything", [(1, TType.I64, lambda: p.writeI64(1))])
    call("add", [(1, TType.I32, lambda: p.writeI32(1)), (2, TType.I32, lambda: p.writeI32(2))])
    t.close()


def serve(idl, port, transport, protocol):
    import thriftpy2
    from thriftpy2.rpc import make_server
    users = thriftpy2.load(idl, module_name="users_thrift")

    class Handler:
        def add(self, a, b):
            out(served="add", a=a, b=b); return a + b

        def get_user(self, id):
            out(served="get_user", id=id)
            if id == 404:
                raise users.NotFound(message="no user 404", id=404)
            return users.User(id=id, name="Ada", role=users.Role.ADMIN, tags=["x", "y"])

        def find(self, prefix, roles):
            out(served="find", prefix=prefix, roles=sorted(roles))
            return [users.User(id=i, name=f"{prefix}{i}", role=r, tags=[]) for i, r in enumerate(sorted(roles), 1)]

        def touch(self, u):
            out(served="touch", user=user_json(u))

        def ping(self, note):
            out(served="ping", note=note)

    proto, trans = tp2_factories(transport, protocol)
    server = make_server(users.Users, Handler(), "127.0.0.1", int(port), proto_factory=proto, trans_factory=trans)
    print(f"thriftpy2 listening on {port}", flush=True)
    server.serve()


if __name__ == "__main__":
    mode, *rest = sys.argv[1:]
    {"thriftpy2-call": thriftpy2_call, "apache-call": apache_call, "serve": serve}[mode](*rest)
