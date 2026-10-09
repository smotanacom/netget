"""Python smpplib 2.2.4, unchanged, as an ESME against NetGet's SMSC.

  smpplib_peer.py PORT SYSTEM_ID PASSWORD
Binds as a transceiver, submits an ASCII message asking for a receipt, a UCS-2 one and one the
SMSC should refuse, reads until quiet, and prints one JSON line: each submit_sm_resp (status
and message id) and each deliver_sm (addresses, esm_class, text).
"""
import json, socket, sys
import smpplib.client, smpplib.exceptions

port, user, password = int(sys.argv[1]), sys.argv[2], sys.argv[3]
out = {"responses": [], "delivered": [], "errors": []}
client = smpplib.client.Client("127.0.0.1", port, timeout=3, allow_unknown_opt_params=True)
client.set_message_sent_handler(lambda pdu: out["responses"].append({"status": pdu.status, "message_id": (pdu.message_id or b"").decode()}))

def received(pdu):
    text = pdu.short_message or b""
    if not text and getattr(pdu, "message_payload", None):
        text = pdu.message_payload
    text = text.decode("utf-16-be") if pdu.data_coding == 8 else text.decode()
    out["delivered"].append({"from": pdu.source_addr.decode(), "to": pdu.destination_addr.decode(), "esm_class": pdu.esm_class, "text": text})

client.set_message_received_handler(received)
client.connect()
client.bind_transceiver(system_id=user, password=password)
client.send_message(source_addr="NetGetTest", dest_addr_ton=1, dest_addr_npi=1, destination_addr="15551230001", short_message=b"hello from smpplib", data_coding=0, registered_delivery=True)
client.send_message(source_addr="NetGetTest", dest_addr_ton=1, dest_addr_npi=1, destination_addr="15551230002", short_message="Привет".encode("utf-16-be"), data_coding=8)
client.send_message(source_addr="NetGetTest", dest_addr_ton=1, dest_addr_npi=1, destination_addr="44990000000", short_message=b"rejected", data_coding=0)
while True:
    try:
        client.read_once(auto_send_enquire_link=False)
    except smpplib.exceptions.PDUError as e:
        out["errors"].append(e.args[1] if len(e.args) > 1 else str(e))
    except (socket.timeout, TimeoutError):
        break
    except Exception as e:  # smpplib wraps a read timeout in its own ConnectionError
        if "timed out" in str(e) or "timeout" in str(e).lower():
            break
        raise
client.unbind()
client.disconnect()
print(json.dumps(out))
