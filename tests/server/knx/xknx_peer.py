"""xknx, unchanged, as a KNXnet/IP tunnelling client of a gateway: switch 1/2/3 on, read the
temperature at 1/2/4 and the text at 1/2/5, read 1/2/9 (which nothing answers), and collect
every telegram the bus delivers. Prints one JSON line.

Usage: xknx_peer.py HOST PORT            the client run above
       xknx_peer.py HOST PORT respond    stay on the bus answering reads of 1/2/4 with 19.25 °C
                                         (DPT 9) and printing READY, until killed
"""
import asyncio, json, sys
from xknx import XKNX
from xknx.core import ValueReader
from xknx.dpt import DPTArray, DPTBinary
from xknx.dpt.dpt_9 import DPTTemperature
from xknx.dpt.dpt_16 import DPTString
from xknx.io import ConnectionConfig, ConnectionType
from xknx.telegram import GroupAddress, Telegram
from xknx.telegram.apci import GroupValueRead, GroupValueResponse, GroupValueWrite


async def main(host, port):
    seen = []

    def heard(t):
        p = t.payload
        value = getattr(p, "value", None)
        seen.append({"source": str(t.source_address), "destination": str(t.destination_address),
                     "kind": type(p).__name__,
                     "value": list(value.value) if isinstance(value, DPTArray) else (value.value if value is not None else None)})

    x = XKNX(connection_config=ConnectionConfig(connection_type=ConnectionType.TUNNELING, gateway_ip=host,
                                                gateway_port=int(port), local_ip="127.0.0.1"))
    x.telegram_queue.register_telegram_received_cb(heard)
    await x.start()
    out = {"address": str(x.current_address)}
    await x.telegrams.put(Telegram(destination_address=GroupAddress("1/2/3"), payload=GroupValueWrite(DPTBinary(1))))
    t = await ValueReader(x, GroupAddress("1/2/4"), timeout_in_seconds=5).read()
    out["temperature"] = DPTTemperature.from_knx(t.payload.value) if t else None
    t = await ValueReader(x, GroupAddress("1/2/5"), timeout_in_seconds=5).read()
    out["text"] = DPTString.from_knx(t.payload.value) if t else None
    t = await ValueReader(x, GroupAddress("1/2/9"), timeout_in_seconds=2).read()
    out["unanswered"] = t is None
    await asyncio.sleep(0.5)
    out["heard"] = seen
    await x.stop()
    print(json.dumps(out), flush=True)


async def respond(host, port):
    x = XKNX(connection_config=ConnectionConfig(connection_type=ConnectionType.TUNNELING, gateway_ip=host,
                                                gateway_port=int(port), local_ip="127.0.0.1"))

    def answer(t):
        if isinstance(t.payload, GroupValueRead) and str(t.destination_address) == "1/2/4":
            x.telegrams.put_nowait(Telegram(destination_address=t.destination_address,
                                            payload=GroupValueResponse(DPTTemperature.to_knx(19.25))))

    x.telegram_queue.register_telegram_received_cb(answer)
    await x.start()
    print("READY", flush=True)
    while True:
        await asyncio.sleep(3600)


if len(sys.argv) > 3 and sys.argv[3] == "respond":
    asyncio.run(respond(sys.argv[1], sys.argv[2]))
else:
    asyncio.run(main(sys.argv[1], sys.argv[2]))
