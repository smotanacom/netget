// ugorji/go's MsgpackSpecRpc codec (net/rpc speaking the MessagePack-RPC spec), unchanged,
// driven against NetGet:
//
//	peer client ADDR   calls add, echo and a missing method; prints one JSON line
//	peer server        serves Arith.Add, Arith.Echo and Arith.Fail; prints "READY 127.0.0.1:PORT"
package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"net/rpc"
	"os"

	"github.com/ugorji/go/codec"
)

var handle = func() *codec.MsgpackHandle {
	h := &codec.MsgpackHandle{}
	h.WriteExt = true
	h.RawToString = true
	return h
}()

func run(addr string) map[string]any {
	conn, err := net.Dial("tcp", addr)
	if err != nil {
		return map[string]any{"fatal": err.Error()}
	}
	client := rpc.NewClientWithCodec(codec.MsgpackSpecRpc.ClientCodec(conn, handle))
	defer client.Close()
	out := map[string]any{}
	var sum int64
	if err := client.Call("add", codec.MsgpackSpecRpcMultiArgs{2, 3}, &sum); err != nil {
		out["add_error"] = err.Error()
	}
	out["add"] = sum
	var echoed any
	if err := client.Call("echo", codec.MsgpackSpecRpcMultiArgs{"go", []int{1, 2}}, &echoed); err != nil {
		out["echo_error"] = err.Error()
	}
	out["echo"] = echoed
	var nothing any
	out["missing_error"] = fmt.Sprint(client.Call("missing", codec.MsgpackSpecRpcMultiArgs{}, &nothing))
	return out
}

type Arith struct{}

func (Arith) Add(args []int64, reply *int64) error {
	var s int64
	for _, a := range args {
		s += a
	}
	*reply = s
	return nil
}

func (Arith) Echo(args string, reply *string) error {
	*reply = "go says " + args
	return nil
}

func (Arith) Fail(args string, reply *string) error {
	return errors.New("arith refuses: " + args)
}

func serve() {
	server := rpc.NewServer()
	if err := server.Register(Arith{}); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	fmt.Println("READY " + ln.Addr().String())
	for {
		c, err := ln.Accept()
		if err != nil {
			os.Exit(1)
		}
		go server.ServeCodec(codec.MsgpackSpecRpc.ServerCodec(c, handle))
	}
}

func main() {
	if len(os.Args) < 2 {
		os.Exit(2)
	}
	switch os.Args[1] {
	case "client":
		b, _ := json.Marshal(run(os.Args[2]))
		fmt.Println(string(b))
	case "server":
		serve()
	default:
		os.Exit(2)
	}
}
