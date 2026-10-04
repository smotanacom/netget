"""python-hl7 0.4.5 MLLP peers driven through public APIs only.

    peer.py client HOST PORT   send ADT^A01, ORU^R01 and QRY^A19; print one JSON line per ACK
    peer.py server             print {"port"}; answer ADT with AA, ORU with AE (+ERR), else AR;
                               print one JSON line per received message until stdin closes
"""
import asyncio
import json
import sys

import hl7
from hl7.mllp import open_hl7_connection, start_hl7_server


def emit(obj):
    print(json.dumps(obj), flush=True)


MESSAGES = [
    "MSH|^~\\&|LAB|NORTH|EHR|HOSP|20260101120000||ADT^A01^ADT_A01|PEER-1|P|2.5\r"
    "EVN|A01|20260101120000\r"
    "PID|1||12345^^^HOSP^MR||Doe^John||19700101|M\r"
    "PV1|1|I|W^389^1",
    "MSH|^~\\&|LAB|NORTH|EHR|HOSP|20260101120100||ORU^R01^ORU_R01|PEER-2|P|2.5\r"
    "PID|1||12345^^^HOSP^MR||Doe^John\r"
    "OBR|1||9876|GLU^Glucose\r"
    "OBX|1|NM|GLU^Glucose||5.4|mmol/L|3.9-5.5|N|||F",
    "MSH|^~\\&|LAB|NORTH|EHR|HOSP|20260101120200||QRY^A19|PEER-3|P|2.3\r"
    "QRD|20260101120200|R|I|Q1|||1^RD|12345|DEM",
]


async def client(host, port):
    reader, writer = await open_hl7_connection(host, int(port))
    for text in MESSAGES:
        sent = hl7.parse(text)
        writer.writemessage(sent)
        await writer.drain()
        ack = await asyncio.wait_for(reader.readmessage(), 10)
        msa = ack.segment("MSA")
        err = [str(s) for s in ack if str(s[0]) == "ERR"]
        emit({"sent_control": str(sent.segment("MSH")[10]), "ack_code": str(msa[1]), "msa_control": str(msa[2]),
              "msa_text": str(msa[3]) if len(msa) > 3 else "", "ack_type": str(ack.segment("MSH")[9]),
              "ack_receiver": str(ack.segment("MSH")[5]), "err": err, "extra": [str(s[0]) for s in ack][2:]})
    writer.close()
    await writer.wait_closed()


async def server():
    async def handle(reader, writer):
        try:
            while not reader.at_eof():
                message = await reader.readmessage()
                kind = str(message.segment("MSH")[9])
                pid = [s for s in message if str(s[0]) == "PID"]
                emit({"type": kind, "control_id": str(message.segment("MSH")[10]),
                      "sender": str(message.segment("MSH")[3]), "patient": str(pid[0][5]) if pid else None,
                      "segments": [str(s[0]) for s in message]})
                if kind.startswith("ADT"):
                    ack = message.create_ack("AA")
                elif kind.startswith("ORU"):
                    ack = message.create_ack("AE", message_id="SRV-AE")
                    carrier = hl7.parse("MSH|^~\\&|X\rERR|||207^Application internal error^HL70357|E||||glucose out of range")
                    ack.append(carrier.segment("ERR"))
                else:
                    ack = message.create_ack("AR")
                writer.writemessage(ack)
                await writer.drain()
        except asyncio.IncompleteReadError:
            pass
        except Exception as e:  # noqa: BLE001
            emit({"server_error": repr(e)})

    srv = await start_hl7_server(handle, "127.0.0.1", 0)
    emit({"port": srv.sockets[0].getsockname()[1]})
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, sys.stdin.read)
    srv.close()


if __name__ == "__main__":
    if sys.argv[1] == "client":
        asyncio.run(client(sys.argv[2], sys.argv[3]))
    else:
        asyncio.run(server())
