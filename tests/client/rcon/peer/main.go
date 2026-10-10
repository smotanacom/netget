// Independent RCON peers for NetGet's tests, both from github.com/gorcon/rcon, unchanged:
//
//	client ADDR PASSWORD COMMAND...  log in with gorcon's client, run each command, print JSON
//	server PASSWORD                  serve gorcon's rcontest server, print "READY addr", and
//	                                 run until stdin closes
package main

import (
	"encoding/json"
	"fmt"
	"io"
	"os"
	"strings"

	"github.com/gorcon/rcon"
	"github.com/gorcon/rcon/rcontest"
)

func emit(v any) {
	out, _ := json.Marshal(v)
	fmt.Println(string(out))
}

func handle(c *rcontest.Context) {
	request := c.Request()
	var body string
	switch request.Body() {
	case "players":
		body = "There are 2 of a max of 20 players online: alice, bob"
	case "seed":
		body = "Seed: [-4172144997902289642]"
	default:
		body = "Unknown command: " + request.Body()
	}
	_, _ = rcon.NewPacket(rcon.SERVERDATA_RESPONSE_VALUE, request.ID, body).WriteTo(c.Conn())
}

func main() {
	if len(os.Args) < 3 {
		fmt.Fprintln(os.Stderr, "usage: rcon-peer client ADDR PASSWORD COMMAND... | rcon-peer server PASSWORD")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "client":
		out := map[string]any{}
		conn, err := rcon.Dial(os.Args[2], os.Args[3])
		if err != nil {
			out["error"] = err.Error()
			emit(out)
			return
		}
		defer conn.Close()
		responses := []string{}
		for _, command := range os.Args[4:] {
			response, err := conn.Execute(command)
			if err != nil {
				out["error"] = err.Error()
				break
			}
			responses = append(responses, response)
		}
		out["responses"] = responses
		emit(out)
	case "server":
		server := rcontest.NewUnstartedServer(
			rcontest.SetSettings(rcontest.Settings{Password: os.Args[2]}),
			rcontest.SetCommandHandler(handle),
		)
		server.Start()
		defer server.Close()
		fmt.Printf("READY %s\n", server.Addr())
		_, _ = io.Copy(io.Discard, os.Stdin)
	default:
		fmt.Fprintln(os.Stderr, "unknown mode "+strings.TrimSpace(os.Args[1]))
		os.Exit(2)
	}
}
