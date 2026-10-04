"""Unchanged GraphQL peers for NetGet's tests.

client URL   gql 4.4.0 with its requests transport: builds the schema from the server's
             introspection (fetch_schema_from_transport), validates every document against it
             locally, then runs the operations below and prints one JSON line per result.
ws URL       gql's websockets transport (graphql-transport-ws): schema over the socket, a query,
             a completed countdown, a refused subscription, then two pushed bookAdded events
             before it cancels; prints {"step": "waiting"} before that last subscription.
server       strawberry-graphql 0.330.2 ASGI app under uvicorn on 127.0.0.1:0; prints
             {"port": N} and serves until stdin closes.
"""
import json, sys


def client(url):
    from gql import Client, GraphQLRequest, gql
    from gql.transport.exceptions import TransportQueryError
    from gql.transport.requests import RequestsHTTPTransport as transport
    from graphql import GraphQLError

    out = lambda **kw: print(json.dumps(kw), flush=True)
    for accept in [None, "application/graphql-response+json"]:
        headers = {"Accept": accept} if accept else None
        with Client(transport=transport(url=url, headers=headers, timeout=20), fetch_schema_from_transport=True) as s:
            schema = s.client.schema
            out(step="schema", accept=accept, types=sorted(t for t in schema.type_map if not t.startswith("__")),
                query=sorted(schema.query_type.fields), mutation=sorted(schema.mutation_type.fields))
            r = s.execute(GraphQLRequest("query Book($id: ID!) { book(id: $id) { id title year author { name } } }", variable_values={"id": "1"}))
            out(step="book", result=r)
            r = s.execute(gql("{ a: book(id: \"1\") { title } b: book(id: \"2\") { title } search(term: \"du\") { __typename ... on Book { title } ... on Author { name } } }"))
            out(step="aliases", result=r)
            r = s.execute(gql("mutation { addBook(title: \"Emma\", year: 1815) { id title } }"))
            out(step="mutation", result=r)
            try:
                s.execute(gql("{ book(id: \"404\") { title } }"))
                out(step="field_error", error=None)
            except TransportQueryError as e:
                out(step="field_error", error=e.errors, data=e.data)
            try:
                s.execute(gql("{ book(id: \"1\") { isbn } }"))
                out(step="local_validation", error=None)
            except GraphQLError as e:
                out(step="local_validation", error=str(e))


def ws(url):
    import asyncio
    from gql import Client, gql
    from gql.transport.exceptions import TransportQueryError
    from gql.transport.websockets import WebsocketsTransport

    out = lambda **kw: print(json.dumps(kw), flush=True)

    async def main():
        # gql reads the agreed subprotocol from a case-sensitive "Sec-WebSocket-Protocol"
        # key and falls back to the legacy Apollo protocol when it misses; hyper writes header
        # names in lower case, so name graphql-transport-ws explicitly.
        transport = WebsocketsTransport(
            url=url.replace("http://", "ws://", 1),
            subprotocols=[WebsocketsTransport.GRAPHQLWS_SUBPROTOCOL],
        )
        async with Client(transport=transport, fetch_schema_from_transport=True) as s:
            out(step="schema", subprotocol=transport.subprotocol,
                subscription=sorted(s.client.schema.subscription_type.fields))
            out(step="query", result=await s.execute(gql("{ hello(name: \"socket\") }")))
            events = [r async for r in s.subscribe(gql("subscription { countdown(from: 3) }"))]
            out(step="countdown", events=events)
            try:
                [r async for r in s.subscribe(gql("subscription { forbidden }"))]
                out(step="forbidden", error=None)
            except TransportQueryError as e:
                out(step="forbidden", error=e.errors)
            out(step="waiting")
            got = []
            async for r in s.subscribe(gql("subscription { bookAdded { title author { name } } }")):
                got.append(r)
                if len(got) == 2:
                    break
            out(step="pushed", events=got)
            await asyncio.sleep(1.0)

    asyncio.run(main())


def server():
    import asyncio, typing
    import strawberry, uvicorn
    from strawberry.asgi import GraphQL

    @strawberry.type
    class Author:
        name: str

    @strawberry.type
    class Book:
        id: strawberry.ID
        title: str
        year: typing.Optional[int]
        author: Author

    SearchResult = typing.Annotated[typing.Union[Book, Author], strawberry.union("SearchResult")]
    BOOKS = {"1": Book(id="1", title="Dune", year=1965, author=Author(name="Frank Herbert"))}

    @strawberry.type
    class Query:
        @strawberry.field
        def hello(self, name: typing.Optional[str] = None) -> str:
            return f"Hello, {name or 'world'}"

        @strawberry.field
        def book(self, id: strawberry.ID) -> typing.Optional[Book]:
            if id == "404":
                raise ValueError("book 404 is gone")
            return BOOKS.get(id)

        @strawberry.field
        def search(self, term: str) -> typing.List[SearchResult]:
            return [BOOKS["1"], Author(name="Frank Herbert")]

    @strawberry.type
    class Subscription:
        @strawberry.subscription
        async def countdown(self, from_: typing.Annotated[int, strawberry.argument(name="from")]) -> typing.AsyncGenerator[int, None]:
            for i in range(from_, 0, -1):
                yield i
                await asyncio.sleep(0.01)

        @strawberry.subscription
        async def ticks(self) -> typing.AsyncGenerator[int, None]:
            i = 0
            while True:
                i += 1
                yield i
                await asyncio.sleep(0.05)

    @strawberry.type
    class Mutation:
        @strawberry.mutation
        def add_book(self, title: str, year: typing.Optional[int] = None) -> Book:
            book = Book(id=str(len(BOOKS) + 1), title=title, year=year, author=Author(name="Anonymous"))
            BOOKS[book.id] = book
            return book

    app = GraphQL(strawberry.Schema(query=Query, mutation=Mutation, subscription=Subscription))

    async def main():
        config = uvicorn.Config(app, host="127.0.0.1", port=0, log_level="warning", lifespan="off")
        srv = uvicorn.Server(config)
        task = asyncio.create_task(srv.serve())
        while not srv.started:
            await asyncio.sleep(0.01)
        port = srv.servers[0].sockets[0].getsockname()[1]
        print(json.dumps({"port": port}), flush=True)
        await asyncio.get_running_loop().run_in_executor(None, sys.stdin.read)
        srv.should_exit = True
        await task

    asyncio.run(main())


if __name__ == "__main__":
    {"client": lambda: client(sys.argv[2]), "ws": lambda: ws(sys.argv[2]), "server": server}[sys.argv[1]]()
