"""a2a-sdk 1.2.1 peers (A2A protocol 1.0, JSON-RPC binding), public APIs only.

    peer.py server            print {"port"}; an echo agent: text containing "task" becomes a
                              Task (working -> artifact -> completed), "slow" a Task left
                              working until canceled, anything else a direct Message; until
                              stdin closes
    peer.py client URL        resolve the agent card, send a message, a streamed task, get and
                              cancel a task; one JSON line per step
"""
import asyncio
import json
import socket
import sys
import uuid

from google.protobuf.json_format import MessageToDict


def emit(obj):
    print(json.dumps(obj, default=str), flush=True)


def as_dict(m):
    return MessageToDict(m)


async def server():
    import uvicorn
    from starlette.applications import Starlette
    from a2a.server.agent_execution import AgentExecutor, RequestContext
    from a2a.server.events import EventQueue
    from a2a.server.request_handlers import DefaultRequestHandler
    from a2a.server.routes import create_agent_card_routes, create_jsonrpc_routes
    from a2a.server.tasks import InMemoryTaskStore, TaskUpdater
    from a2a.types import AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Message, Part, Role, TaskState

    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]

    class Echo(AgentExecutor):
        async def execute(self, context: RequestContext, event_queue: EventQueue):
            text = context.get_user_input()
            if "task" in text or "slow" in text:
                from a2a.helpers.proto_helpers import new_task
                await event_queue.enqueue_event(new_task(context.task_id, context.context_id, TaskState.TASK_STATE_SUBMITTED))
                updater = TaskUpdater(event_queue, context.task_id, context.context_id)
                await updater.start_work()
                if "slow" in text:
                    return
                await updater.add_artifact([Part(text=f"echo: {text}")], name="echo")
                await updater.complete()
            else:
                await event_queue.enqueue_event(Message(message_id=str(uuid.uuid4()), role=Role.ROLE_AGENT, parts=[Part(text=f"echo: {text}")], context_id=context.context_id))

        async def cancel(self, context: RequestContext, event_queue: EventQueue):
            updater = TaskUpdater(event_queue, context.task_id, context.context_id)
            await updater.cancel()

    card = AgentCard(
        name="Python Echo",
        description="a2a-sdk echo agent",
        version="1.0.0",
        supported_interfaces=[AgentInterface(url=f"http://127.0.0.1:{port}/", protocol_binding="JSONRPC", protocol_version="1.0")],
        capabilities=AgentCapabilities(streaming=True),
        default_input_modes=["text/plain"],
        default_output_modes=["text/plain"],
        skills=[AgentSkill(id="echo", name="Echo", description="Echoes text", tags=["echo"])],
    )
    handler = DefaultRequestHandler(agent_executor=Echo(), task_store=InMemoryTaskStore(), agent_card=card)
    app = Starlette(routes=create_agent_card_routes(card) + create_jsonrpc_routes(handler, "/"))
    srv = uvicorn.Server(uvicorn.Config(app, log_level="warning"))
    task = asyncio.create_task(srv.serve(sockets=[sock]))
    emit({"port": port})
    await asyncio.get_running_loop().run_in_executor(None, sys.stdin.read)
    srv.should_exit = True
    await task


async def client(url):
    from a2a.client import ClientConfig, create_client
    from a2a.types import CancelTaskRequest, GetTaskRequest, Message, Part, Role, SendMessageRequest

    def message(text, task_id=None):
        m = Message(message_id=str(uuid.uuid4()), role=Role.ROLE_USER, parts=[Part(text=text)])
        if task_id:
            m.task_id = task_id
        return SendMessageRequest(message=m)

    plain = await create_client(url, client_config=ClientConfig(streaming=False))
    emit({"step": "card", "name": plain._card.name if hasattr(plain, "_card") else None})
    async for event in plain.send_message(message("hello agent")):
        emit({"step": "message", "event": as_dict(event)})
    streaming = await create_client(url, client_config=ClientConfig(streaming=True))
    task_id = None
    async for event in streaming.send_message(message("please make a task")):
        d = as_dict(event)
        emit({"step": "stream", "event": d})
        task_id = task_id or d.get("task", {}).get("id") or d.get("statusUpdate", {}).get("taskId")
    task = await plain.get_task(GetTaskRequest(id=task_id))
    emit({"step": "get", "task": as_dict(task)})
    async for event in plain.send_message(message("slow job")):
        d = as_dict(event)
        slow = d.get("task", {}).get("id")
        emit({"step": "slow", "event": d})
    canceled = await plain.cancel_task(CancelTaskRequest(id=slow))
    emit({"step": "cancel", "task": as_dict(canceled)})
    await plain.close()
    await streaming.close()


if __name__ == "__main__":
    if sys.argv[1] == "server":
        asyncio.run(server())
    else:
        asyncio.run(client(sys.argv[2]))
