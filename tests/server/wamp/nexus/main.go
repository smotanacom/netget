// nexus v3.3.0, unchanged, as the independent WAMP peer for NetGet's tests.
//
//	nexus client URL   one session against NetGet's router in realm1: register com.example.add2,
//	    subscribe com.example.topic, call add2 through the router, publish (acknowledged, not
//	    excluding itself), call the router's own com.example.time and com.example.forbidden,
//	    then try realm "blocked" and leave
//	nexus router PORT  a router for realm1 on 127.0.0.1:PORT, with a local session that
//	    registers com.example.add2, subscribes com.example.fromnetget, publishes
//	    com.example.tick every 200 ms and calls com.example.netget.echo until it answers
//
// One JSON line per observation.
package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log"
	"os"
	"strconv"
	"sync"
	"time"

	"github.com/gammazero/nexus/v3/client"
	"github.com/gammazero/nexus/v3/router"
	"github.com/gammazero/nexus/v3/wamp"
)

var mu sync.Mutex

func emit(v map[string]any) {
	mu.Lock()
	defer mu.Unlock()
	b, _ := json.Marshal(v)
	fmt.Println(string(b))
}

func callErr(err error) map[string]any {
	var rpc client.RPCError
	if errors.As(err, &rpc) && rpc.Err != nil {
		return map[string]any{"error": string(rpc.Err.Error), "args": rpc.Err.Arguments}
	}
	return map[string]any{"error": err.Error()}
}

func add2(_ context.Context, inv *wamp.Invocation) client.InvokeResult {
	if len(inv.Arguments) != 2 {
		return client.InvokeResult{Err: wamp.URI("com.example.error.arity")}
	}
	a, _ := wamp.AsInt64(inv.Arguments[0])
	b, _ := wamp.AsInt64(inv.Arguments[1])
	return client.InvokeResult{Args: wamp.List{a + b}}
}

func runClient(url string) {
	quiet := log.New(os.Stderr, "", 0)
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	cli, err := client.ConnectNet(ctx, url, client.Config{Realm: "realm1", Logger: quiet})
	if err != nil {
		emit(map[string]any{"step": "join", "error": err.Error()})
		os.Exit(1)
	}
	emit(map[string]any{"step": "join", "session": cli.ID()})
	events := make(chan *wamp.Event, 4)
	must := func(step string, err error) {
		if err != nil {
			emit(map[string]any{"step": step, "error": err.Error()})
			os.Exit(1)
		}
	}
	must("register", cli.Register("com.example.add2", add2, nil))
	must("subscribe", cli.Subscribe("com.example.topic", func(e *wamp.Event) { events <- e }, nil))
	res, err := cli.Call(ctx, "com.example.add2", nil, wamp.List{2, 3}, nil, nil)
	if err != nil {
		emit(map[string]any{"step": "add2", "error": err.Error()})
	} else {
		emit(map[string]any{"step": "add2", "args": res.Arguments})
	}
	must("publish", cli.Publish("com.example.topic", wamp.Dict{"acknowledge": true, "exclude_me": false}, wamp.List{"hello"}, wamp.Dict{"from": "nexus"}))
	select {
	case e := <-events:
		emit(map[string]any{"step": "event", "args": e.Arguments, "kwargs": e.ArgumentsKw})
	case <-time.After(5 * time.Second):
		emit(map[string]any{"step": "event", "error": "no event"})
	}
	for _, p := range []string{"com.example.time", "com.example.forbidden"} {
		res, err := cli.Call(ctx, p, nil, wamp.List{"x"}, nil, nil)
		if err != nil {
			out := callErr(err)
			out["step"] = p
			emit(out)
		} else {
			emit(map[string]any{"step": p, "args": res.Arguments, "kwargs": res.ArgumentsKw})
		}
	}
	_ = cli.Close()
	if _, err := client.ConnectNet(ctx, url, client.Config{Realm: "blocked", Logger: quiet}); err != nil {
		emit(map[string]any{"step": "blocked", "error": err.Error()})
	} else {
		emit(map[string]any{"step": "blocked", "joined": true})
	}
}

func runRouter(port string) {
	quiet := log.New(os.Stderr, "", 0)
	r, err := router.NewRouter(&router.Config{RealmConfigs: []*router.RealmConfig{{URI: "realm1", AnonymousAuth: true, AllowDisclose: true}}}, quiet)
	if err != nil {
		panic(err)
	}
	closer, err := router.NewWebsocketServer(r).ListenAndServe("127.0.0.1:" + port)
	if err != nil {
		panic(err)
	}
	defer closer.Close()
	local, err := client.ConnectLocal(r, client.Config{Realm: "realm1", Logger: quiet})
	if err != nil {
		panic(err)
	}
	if err := local.Register("com.example.add2", add2, nil); err != nil {
		panic(err)
	}
	if err := local.Subscribe("com.example.fromnetget", func(e *wamp.Event) {
		emit(map[string]any{"step": "fromnetget", "args": e.Arguments, "kwargs": e.ArgumentsKw})
	}, nil); err != nil {
		panic(err)
	}
	emit(map[string]any{"event": "listening", "port": port})
	go func() {
		for i := 0; ; i++ {
			_ = local.Publish("com.example.tick", nil, wamp.List{i}, nil)
			time.Sleep(200 * time.Millisecond)
		}
	}()
	for {
		ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		res, err := local.Call(ctx, "com.example.netget.echo", nil, wamp.List{"ping", strconv.Itoa(1)}, nil, nil)
		cancel()
		if err == nil {
			emit(map[string]any{"step": "echo", "args": res.Arguments})
			break
		}
		var rpc client.RPCError
		if errors.As(err, &rpc) && rpc.Err != nil && rpc.Err.Error != wamp.ErrNoSuchProcedure {
			emit(map[string]any{"step": "echo", "error": string(rpc.Err.Error)})
			break
		}
		time.Sleep(300 * time.Millisecond)
	}
	select {}
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: nexus client URL | nexus router PORT")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "client":
		runClient(os.Args[2])
	case "router":
		runRouter(os.Args[2])
	}
}
