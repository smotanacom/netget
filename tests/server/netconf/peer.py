"""Independent NETCONF peers driven through their public APIs only.

    peer.py client HOST PORT USER PASSWORD HOSTKEY_B64 VERSIONS   (ncclient against NetGet)
    peer.py server USER PASSWORD                                   (netconf 2.1.0 for NetGet)

The client prints one JSON object per step. The server prints {"port", "host_key_b64"} on
its first line, then one JSON line per RPC it handled, and serves until stdin closes.
HOME is pointed at an owned temporary directory so no ambient keys or SSH config are read.
"""
import json
import os
import socket
import sys
import tempfile

os.environ["HOME"] = tempfile.mkdtemp(prefix="netget-netconf-peer-")
os.environ.pop("SSH_AUTH_SOCK", None)

NC = "urn:ietf:params:xml:ns:netconf:base:1.0"
BASE = "urn:ietf:params:netconf:base:"
DEMO = "urn:netget:netconf-peer"
WRITABLE = "urn:ietf:params:netconf:capability:writable-running:1.0"


def emit(obj):
    print(json.dumps(obj), flush=True)


def client(host, port, user, password, hostkey_b64, versions):
    from ncclient import manager
    from ncclient.devices.default import DefaultDeviceHandler
    from ncclient.operations import RPCError

    caps = [BASE + v for v in versions.split(",")]

    class Handler(DefaultDeviceHandler):
        def get_capabilities(self):
            return caps

    connection = dict(
        host=host, port=int(port), username=user, password=password, allow_agent=False,
        look_for_keys=False, ssh_config=None, hostkey_verify=True, hostkey_b64=hostkey_b64,
        timeout=10, device_params={"handler": Handler},
    )
    # Not a context manager: its __exit__ sends close-session again after ours.
    m = manager.connect(**connection)
    m.timeout = 10
    emit({"step": "hello", "session_id": m.session_id, "server_capabilities": sorted(m.server_capabilities)})
    reply = m.get(filter=("subtree", '<demo xmlns="%s"><label/></demo>' % DEMO))
    emit({"step": "get", "data_xml": reply.data_xml})
    reply = m.get_config(source="running")
    emit({"step": "get-config", "data_xml": reply.data_xml})
    config = '<config xmlns="%s"><demo xmlns="%s"><label>changed &amp; saved</label></demo></config>' % (NC, DEMO)
    reply = m.edit_config(target="running", config=config)
    emit({"step": "edit-config", "ok": reply.ok})
    try:
        m.lock(target="running")
        emit({"step": "lock", "error": None})
    except RPCError as e:
        emit({"step": "lock", "error": e.tag, "message": e.message})
    try:
        m.get_config(source="candidate")
        emit({"step": "candidate", "error": None})
    except RPCError as e:
        emit({"step": "candidate", "error": e.tag})
    m.close_session()
    emit({"step": "closed"})


def server(user, password):
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    from lxml import etree
    from netconf import error
    from netconf.server import NetconfMethods, NetconfSSHServer, SSHUserPassController

    home = os.environ["HOME"]
    key_file = os.path.join(home, "host_key")
    private = Ed25519PrivateKey.generate().private_bytes(
        serialization.Encoding.PEM, serialization.PrivateFormat.OpenSSH, serialization.NoEncryption())
    fd = os.open(key_file, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
    with os.fdopen(fd, "wb") as stream:
        stream.write(private)

    class Methods(NetconfMethods):
        def __init__(self):
            self.label = "fixture α"

        def nc_append_capabilities(self, capabilities):
            etree.SubElement(capabilities, "{" + NC + "}capability").text = WRITABLE

        def record(self, session, operation):
            emit({"rpc": operation, "base": "1.1" if session.new_framing else "1.0"})

        def data(self):
            data = etree.Element("{" + NC + "}data")
            demo = etree.SubElement(data, "{" + DEMO + "}demo")
            etree.SubElement(demo, "{" + DEMO + "}label", {"owner": "fixture"}).text = self.label
            return data

        def rpc_get(self, session, rpc, filter_or_none):
            self.record(session, "get")
            return self.data()

        def rpc_get_config(self, session, rpc, source, filter_or_none):
            self.record(session, "get-config")
            if len(source) != 1 or source[0].tag != "{" + NC + "}running":
                raise error.OperationNotSupportedProtoError(rpc, message="only running")
            return self.data()

        def rpc_edit_config(self, session, rpc, *parameters):
            self.record(session, "edit-config")
            operation = rpc[0]
            config = operation.find("{" + NC + "}config")
            label = config.find("{" + DEMO + "}demo/{" + DEMO + "}label")
            if label is None:
                raise error.InvalidValueAppError(rpc, message="label required")
            self.label = label.text
            return etree.Element("{" + NC + "}ok")

    # The package's macOS dual-family port=0 path opens two listeners and loses one; pick a
    # free dual-stack port first and use its normal explicit-port path.
    with socket.socket(socket.AF_INET6, socket.SOCK_STREAM) as reservation:
        reservation.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        reservation.bind(("::", 0))
        port = reservation.getsockname()[1]
    server = NetconfSSHServer(SSHUserPassController(user, password), Methods(), port=port, host_key=key_file, debug=False)
    emit({"port": port, "host_key_b64": server.host_key.get_base64()})
    sys.stdin.read()
    server.close()


if __name__ == "__main__":
    mode = sys.argv[1]
    if mode == "client":
        client(*sys.argv[2:8])
    elif mode == "server":
        server(*sys.argv[2:4])
    else:
        raise SystemExit("mode must be client or server")
