"""python ocpp 2.1.0 peers (schema-validating) over websockets 15, public APIs only.

    peer.py cp HOST PORT VERSION     charge point CP-PY: boot, heartbeat, status, authorize,
                                     start/stop (1.6) or TransactionEvent (2.0.1), meter values;
                                     answers Reset and remote start; one JSON line per step;
                                     stays connected until stdin closes
    peer.py csms VERSION             central system: prints {"port"}, answers every core call,
                                     sends Reset after boot; one JSON line per call; until stdin closes
"""
import asyncio
import json
import sys
from dataclasses import asdict
from datetime import datetime, timezone

import websockets
from ocpp.routing import on

NOW = lambda: datetime.now(timezone.utc).isoformat()


def emit(obj):
    print(json.dumps(obj, default=str), flush=True)


def stdin_closed():
    return asyncio.get_running_loop().run_in_executor(None, sys.stdin.read)


def v16_classes():
    from ocpp.v16 import ChargePoint, call, call_result
    from ocpp.v16.enums import Action
    return ChargePoint, call, call_result, Action


def v201_classes():
    from ocpp.v201 import ChargePoint, call, call_result
    from ocpp.v201.enums import Action
    return ChargePoint, call, call_result, Action


async def charge_point(host, port, version):
    sub = "ocpp1.6" if version == "1.6" else "ocpp2.0.1"
    CP, call, result, Action = v16_classes() if version == "1.6" else v201_classes()

    class Station(CP):
        @on(Action.reset)
        def on_reset(self, **kwargs):
            emit({"server_call": "Reset", "payload": kwargs})
            return result.Reset(status="Accepted")

        if version == "1.6":
            @on(Action.remote_start_transaction)
            def on_remote(self, **kwargs):
                emit({"server_call": "RemoteStartTransaction", "payload": kwargs})
                return result.RemoteStartTransaction(status="Accepted")
        else:
            @on(Action.request_start_transaction)
            def on_remote(self, **kwargs):
                emit({"server_call": "RequestStartTransaction", "payload": kwargs})
                return result.RequestStartTransaction(status="Accepted")

    async with websockets.connect(f"ws://{host}:{port}/CP-PY", subprotocols=[sub]) as ws:
        emit({"step": "connected", "subprotocol": ws.subprotocol})
        station = Station("CP-PY", ws)
        runner = asyncio.create_task(station.start())
        if version == "1.6":
            r = await station.call(call.BootNotification(charge_point_model="PySim", charge_point_vendor="OCPP-PY"))
            emit({"step": "boot", "status": r.status, "interval": r.interval})
            r = await station.call(call.Heartbeat())
            emit({"step": "heartbeat", "current_time": r.current_time})
            await station.call(call.StatusNotification(connector_id=1, error_code="NoError", status="Available"))
            emit({"step": "status"})
            r = await station.call(call.Authorize(id_tag="ABC123"))
            emit({"step": "authorize", "status": r.id_tag_info["status"]})
            r = await station.call(call.StartTransaction(connector_id=1, id_tag="ABC123", meter_start=1000, timestamp=NOW()))
            emit({"step": "start", "transaction_id": r.transaction_id, "status": r.id_tag_info["status"]})
            await station.call(call.MeterValues(connector_id=1, transaction_id=r.transaction_id, meter_value=[{"timestamp": NOW(), "sampledValue": [{"value": "1500"}]}]))
            emit({"step": "meter"})
            await station.call(call.StopTransaction(transaction_id=r.transaction_id, meter_stop=2000, timestamp=NOW()))
            emit({"step": "stop"})
            try:
                await station.call(call.DataTransfer(vendor_id="netget", message_id="x"), suppress=False)
                emit({"step": "datatransfer", "error": None})
            except Exception as e:  # noqa: BLE001 - the CALLERROR surfaces as an exception
                emit({"step": "datatransfer", "error": type(e).__name__})
        else:
            r = await station.call(call.BootNotification(charging_station={"model": "PySim", "vendor_name": "OCPP-PY"}, reason="PowerUp"))
            emit({"step": "boot", "status": r.status, "interval": r.interval})
            r = await station.call(call.Heartbeat())
            emit({"step": "heartbeat", "current_time": r.current_time})
            await station.call(call.StatusNotification(timestamp=NOW(), connector_status="Available", evse_id=1, connector_id=1))
            emit({"step": "status"})
            r = await station.call(call.Authorize(id_token={"id_token": "ABC123", "type": "ISO14443"}))
            emit({"step": "authorize", "status": r.id_token_info["status"]})
            await station.call(call.TransactionEvent(event_type="Started", timestamp=NOW(), trigger_reason="Authorized", seq_no=0, transaction_info={"transaction_id": "T1"}))
            emit({"step": "transaction_started"})
            await station.call(call.MeterValues(evse_id=1, meter_value=[{"timestamp": NOW(), "sampled_value": [{"value": 1500.0}]}]))
            emit({"step": "meter"})
            await station.call(call.TransactionEvent(event_type="Ended", timestamp=NOW(), trigger_reason="StopAuthorized", seq_no=1, transaction_info={"transaction_id": "T1"}))
            emit({"step": "transaction_ended"})
        emit({"step": "done"})
        await stdin_closed()
        runner.cancel()


async def central_system(version):
    sub = "ocpp1.6" if version == "1.6" else "ocpp2.0.1"
    CP, call, result, Action = v16_classes() if version == "1.6" else v201_classes()

    class Central(CP):
        def seen(self, action, payload):
            emit({"call": action, "payload": payload})

        @on(Action.boot_notification)
        def on_boot(self, **kw):
            self.seen("BootNotification", kw)
            asyncio.get_running_loop().call_later(0.2, lambda: asyncio.ensure_future(self.after_boot()))
            return result.BootNotification(current_time=NOW(), interval=10, status="Accepted")

        async def after_boot(self):
            try:
                r = await self.call(call.Reset(type="Soft" if version == "1.6" else "Immediate"))
                emit({"reset_status": r.status})
            except Exception as e:  # noqa: BLE001
                emit({"reset_error": repr(e)})

        @on(Action.heartbeat)
        def on_heartbeat(self, **kw):
            self.seen("Heartbeat", kw)
            return result.Heartbeat(current_time=NOW())

        @on(Action.status_notification)
        def on_status(self, **kw):
            self.seen("StatusNotification", kw)
            return result.StatusNotification()

        @on(Action.authorize)
        def on_authorize(self, **kw):
            self.seen("Authorize", kw)
            if version == "1.6":
                return result.Authorize(id_tag_info={"status": "Accepted"})
            return result.Authorize(id_token_info={"status": "Accepted"})

        if version == "1.6":
            @on(Action.start_transaction)
            def on_start(self, **kw):
                self.seen("StartTransaction", kw)
                return result.StartTransaction(transaction_id=42, id_tag_info={"status": "Accepted"})

            @on(Action.stop_transaction)
            def on_stop(self, **kw):
                self.seen("StopTransaction", kw)
                return result.StopTransaction()
        else:
            @on(Action.transaction_event)
            def on_tx(self, **kw):
                self.seen("TransactionEvent", kw)
                return result.TransactionEvent()

    async def handler(ws):
        cp_id = ws.request.path.strip("/").split("/")[-1]
        emit({"connected": cp_id, "subprotocol": ws.subprotocol})
        try:
            await Central(cp_id, ws).start()
        except websockets.ConnectionClosed:
            pass

    async with websockets.serve(handler, "127.0.0.1", 0, subprotocols=[sub]) as server:
        emit({"port": server.sockets[0].getsockname()[1]})
        await stdin_closed()


if __name__ == "__main__":
    if sys.argv[1] == "cp":
        asyncio.run(charge_point(sys.argv[2], sys.argv[3], sys.argv[4]))
    else:
        asyncio.run(central_system(sys.argv[2]))
