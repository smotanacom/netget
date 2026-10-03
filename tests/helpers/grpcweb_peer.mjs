// Mandatory independent Connect-ES peers. Descriptor is generated from grpc_streams.proto.
import assert from "node:assert/strict";
import http from "node:http";
import fs from "node:fs";
import { fileDesc } from "@bufbuild/protobuf/codegenv2";
import { createClient, ConnectError, Code } from "@connectrpc/connect";
import { connectNodeAdapter, createGrpcWebTransport, compressionGzip } from "@connectrpc/connect-node";
import { createGrpcWebTransport as fetchTransport } from "@connectrpc/connect-web";
for (const [name, version] of Object.entries({"@connectrpc/connect":"2.2.0", "@connectrpc/connect-node":"2.2.0", "@connectrpc/connect-web":"2.2.0", "@bufbuild/protobuf":"2.16.0"})) {
  assert.equal(JSON.parse(fs.readFileSync(`node_modules/${name}/package.json`)).version, version, `Install tests/helpers/grpcweb-package.json: wrong ${name}`);
}
const service = fileDesc(descriptor).services[0];
const [mode, target, scenario = "normal"] = process.argv.slice(1);
const sleep = ms => new Promise(resolve => setTimeout(resolve, ms));
if (mode === "server") {
  const handler = connectNodeAdapter({
    grpc: false, connect: false, grpcWeb: true,
    readMaxBytes: 4*1024*1024, writeMaxBytes: 4*1024*1024+1,
    acceptCompression: [compressionGzip], compressMinBytes: 0,
    routes(router) {
      router.service(service, {
        echo(req, ctx) {
          if (req.name === "status") throw new ConnectError("denied: peer test", Code.PermissionDenied);
          ctx.responseTrailer.set("x-peer-note", "contains:colon:values");
          return {...req, name:`echo:${req.name}`, value:req.value+1};
        },
        async *watch(req, ctx) {
          if (req.name === "bound" || req.name === "overflow") {
            yield {name:"x".repeat(4*1024*1024-5+(req.name === "overflow" ? 1 : 0))}; return;
          }
          if (req.name === "subscribe") {
            while (!ctx.signal.aborted) {yield {name:"subscription", value:1};await sleep(20);} return;
          }
          const total=req.name === "many" ? 257 : 3;
          for (let i=0;i<total;i++) yield {name:`${req.name}-${i}`, value:i+1, tags:req.tags, counts:req.counts};
        },
        async collect() {throw new ConnectError("client streaming excluded",Code.Unimplemented);},
        async *chat() {throw new ConnectError("bidi excluded",Code.Unimplemented);},
      });
    },
  });
  const server=http.createServer(handler);
  server.listen(0,"127.0.0.1",()=>console.log(JSON.stringify({port:server.address().port,connect:"2.2.0",protobuf:"2.16.0"})));
} else {
  const transport=mode === "fetch-client"
    ? fetchTransport({baseUrl:`http://${target}`,useBinaryFormat:true})
    : createGrpcWebTransport({httpVersion:"1.1",baseUrl:`http://${target}`,useBinaryFormat:true,
        readMaxBytes:4*1024*1024,writeMaxBytes:4*1024*1024+1,defaultTimeoutMs:5000,
        sendCompression:(scenario === "gzip" || scenario === "request-bounds-gzip") ? compressionGzip : undefined,acceptCompression:[compressionGzip],compressMinBytes:0});
  const client=createClient(service,transport);
  let result;
  if (scenario === "status") {
    let code=0, message="";
    try {await client.echo({name:"status"});} catch(error) {code=error.code;message=error.rawMessage;}
    assert.equal(code,7);assert.match(message,/denied: peer test/);result={code,message};
  } else if (scenario.startsWith("request-bounds")) {
    let accepted=0,rejected=0;
    for (const length of [4*1024*1024-5,4*1024*1024-4]) {
      try { const r=await client.echo({name:"x".repeat(length)});assert.equal(r.name,"bounded");accepted++; }
      catch(error) {assert.equal(error.code,8,error.rawMessage);rejected++;}
    }
    assert.equal(accepted,1);assert.equal(rejected,1);result={accepted,rejected};
  } else {
    const name=scenario === "gzip" ? "gzip-"+"x".repeat(1024) : "peer";
    const req={name,value:4,tags:["blue","green"],counts:{first:2,second:3}};
    const echo=await client.echo(req);
    assert.equal(echo.name,`echo:${name}`);assert.equal(echo.value,5);
    assert.deepEqual(echo.tags,["blue","green"]);assert.deepEqual(echo.counts,{first:2,second:3});
    let count=0;
    for await (const message of client.watch(req)) {
      assert.equal(message.name,`${name}-${count}`);assert.equal(message.value,count+1);count++;
    }
    assert.equal(count,3);result={echo:5,watch:count,client:mode};
  }
  console.log(JSON.stringify(result));
}
