// Independent A2S peers for NetGet's tests, both from github.com/woozymasta/a2s, unchanged:
//
//	client ADDR QUERY...  query with woozymasta's client (info, players, rules), print JSON
//	server                serve woozymasta's UDP A2S server on 127.0.0.1, print
//	                      "READY addr", and run until stdin closes
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"os"
	"time"

	"github.com/woozymasta/a2s/pkg/a2s"
	"github.com/woozymasta/a2s/pkg/a2s/server"
)

func emit(v any) {
	out, _ := json.Marshal(v)
	fmt.Println(string(out))
}

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: a2s-peer client ADDR QUERY... | a2s-peer server")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "client":
		client, err := a2s.NewWithString(os.Args[2])
		if err != nil {
			emit(map[string]any{"error": err.Error()})
			return
		}
		defer client.Close()
		out := map[string]any{}
		for _, query := range os.Args[3:] {
			ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
			switch query {
			case "info":
				info, err := client.GetInfo(ctx)
				if err != nil {
					out["info_error"] = err.Error()
				} else {
					out["info"] = info
				}
			case "players":
				players, err := client.GetPlayers(ctx)
				if err != nil {
					out["players_error"] = err.Error()
				} else {
					names := []map[string]any{}
					for _, p := range players {
						names = append(names, map[string]any{"name": p.Name, "score": p.Score, "seconds": p.Duration.Seconds()})
					}
					out["players"] = names
				}
			case "rules":
				rules, err := client.GetRules(ctx)
				if err != nil {
					out["rules_error"] = err.Error()
				} else {
					values := map[string]string{}
					for _, r := range rules {
						values[r.Name] = r.Value
					}
					out["rules"] = values
				}
			}
			cancel()
		}
		emit(out)
	case "server":
		handler := server.HandlerFunc(func(_ context.Context, request *server.Request) (server.Response, error) {
			switch request.Query.Type {
			case a2s.InfoRequest:
				return server.InfoResponse{Info: a2s.Info{
					Format:      a2s.InfoFormat(a2s.ResponseInfo),
					Protocol:    17,
					Name:        "Independent A2S",
					Map:         "cp_badlands",
					Folder:      "tf",
					Game:        "Team Fortress",
					AppID:       440,
					Players:     2,
					MaxPlayers:  24,
					ServerType:  a2s.ServerType('d'),
					Environment: a2s.Environment('l'),
					Version:     "8622567",
				}}, nil
			case a2s.PlayerRequest:
				return server.PlayersResponse{Players: []a2s.Player{
					{Name: "alice", Score: 7, Duration: 90 * time.Second},
					{Name: "bob", Score: 3, Duration: 45 * time.Second},
				}}, nil
			case a2s.RulesRequest:
				return server.RulesResponse{Rules: a2s.Rules{{Name: "mp_timelimit", Value: "30"}, {Name: "tf_gamemode_cp", Value: "1"}}}, nil
			}
			return nil, server.ErrDrop
		})
		instance, err := server.New(handler)
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		conn, err := net.ListenPacket("udp", "127.0.0.1:0")
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		go func() { _ = instance.Serve(conn) }()
		fmt.Printf("READY %s\n", conn.LocalAddr())
		_, _ = io.Copy(io.Discard, os.Stdin)
		_ = instance.Shutdown(context.Background())
	}
}
